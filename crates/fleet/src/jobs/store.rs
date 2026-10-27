//! The bounded local job journal, and the workflow runs it follows until they finish.
//!
//! Writes only this dashboard's own file, atomically. A corrupt or newer-format file is
//! preserved: jobs are still kept in memory for this session, and the view shows why
//! they can't be saved.
//!
//! Saving is two steps so the lock the fleet view also takes is never held for disk I/O:
//! [`JobStore::prepare_save`] serializes under the lock, the caller writes the bytes without
//! it, then reports back with [`JobStore::finish_save`]. New jobs are saved at most every
//! [`SAVE_INTERVAL`]; the next history round or app exit flushes anything newer.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::analytics::{Coverage, Span};
use super::{FleetActivity, FleetCompletedJob};
use crate::model::{GhJob, GhRunner, JobScan};

pub(crate) const JOB_RETENTION: Duration = Duration::days(30);
pub(crate) const MAX_JOBS: usize = 20_000;
/// Completed jobs shown per runner and per machine in the fleet view.
pub(crate) const RECENT_PER_ENTITY: usize = 10;
const VERSION: u32 = 1;
const MAX_PENDING_RUNS: usize = 2_000;
// Self-hosted jobs can run for up to five days; a run older than this isn't coming back.
const PENDING_RETENTION: Duration = Duration::days(6);
/// A run that left the scan but wasn't finished is asked about again after this long.
const RECHECK_AFTER: Duration = Duration::minutes(2);
/// Changes that don't add or finish anything (a run seen again) are saved at most this often.
const CHECKPOINT: Duration = Duration::minutes(10);
/// New or finished jobs are saved at most this often, so a burst of feed deliveries is one write.
pub(crate) const SAVE_INTERVAL: Duration = Duration::seconds(10);
/// A run whose job listing stays truncated is given up after this many reads.
const MAX_UNREADABLE: u32 = 10;
/// Recording continues the previous span when the gap since it is at most this long.
const COVERAGE_GAP: Duration = Duration::minutes(3);
const MAX_SPANS_PER_ORG: usize = 10_000;
/// Jobs seen completed, kept in memory so late deliveries can't bring them back.
const RECENTLY_COMPLETED: usize = 5_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    version: u32,
    /// When recording started; analytics never count time before it as idle.
    started_at: DateTime<Utc>,
    /// Ascending by completion time, then job ID.
    jobs: Vec<FleetCompletedJob>,
    #[serde(default)]
    pending: BTreeMap<u64, PendingRun>,
    /// Recorded time per organization: the app ran and the organization's job data was fresh.
    /// Sorted, non-overlapping spans.
    #[serde(default)]
    coverage: BTreeMap<String, Vec<Span>>,
}

impl Journal {
    fn new(now: DateTime<Utc>) -> Self {
        Self {
            version: VERSION,
            started_at: now,
            jobs: Vec::new(),
            pending: BTreeMap::new(),
            coverage: BTreeMap::new(),
        }
    }
}

/// A workflow run seen with a queued or running self-hosted job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PendingRun {
    /// Organization login, lowercase.
    pub org: String,
    /// `owner/name`
    pub repo: String,
    pub run_id: u64,
    /// Self-hosted jobs seen queued or running in this run.
    pub jobs: BTreeSet<u64>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    #[serde(default)]
    pub checked_at: Option<DateTime<Utc>>,
    /// Reads whose job listing was truncated.
    #[serde(default)]
    pub unreadable: u32,
}

/// What a final-state read learned about a followed run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunCheck {
    /// Still going, or couldn't be read this time: ask again later.
    Pending,
    /// The listing was cut short; after [`MAX_UNREADABLE`] of these the run is given up.
    Truncated,
}

/// Serialized journal bytes to write outside the store's lock.
#[derive(Debug)]
pub(crate) struct PendingWrite {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    /// Increases with every prepared write; a writer never replaces a newer one.
    pub seq: u64,
}

#[derive(Debug)]
pub(crate) struct JobStore {
    path: PathBuf,
    journal: Journal,
    ids: HashSet<u64>,
    storage_error: Option<String>,
    writable: bool,
    /// Jobs or followed runs were added or removed since the last save.
    dirty: bool,
    /// Only timestamps changed since the last save.
    touched: bool,
    saved_at: Option<DateTime<Utc>>,
    seq: u64,
    completed: HashSet<u64>,
    completed_order: VecDeque<u64>,
}

impl JobStore {
    pub fn load(path: PathBuf, now: DateTime<Utc>) -> Self {
        let result = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Journal>(&bytes)
                .map_err(|e| format!("{e}."))
                .and_then(|j| {
                    if j.version == VERSION {
                        Ok(j)
                    } else {
                        Err(format!("Unsupported job history version {}.", j.version))
                    }
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Journal::new(now)),
            Err(e) => Err(format!("{e}.")),
        };
        let writable = result.is_ok();
        let (mut journal, storage_error) = match result {
            Ok(journal) => (journal, None),
            Err(e) => (
                Journal::new(now),
                Some(format!(
                    "Couldn't read job history: {e} The existing file has been preserved; new jobs are kept only until the app quits."
                )),
            ),
        };
        journal.jobs.sort_by_cached_key(FleetCompletedJob::cursor);
        let mut ids = HashSet::new();
        journal.jobs.retain(|j| ids.insert(j.id));
        Self {
            path,
            journal,
            ids,
            storage_error,
            writable,
            dirty: false,
            touched: false,
            saved_at: None,
            seq: 0,
            completed: HashSet::new(),
            completed_order: VecDeque::new(),
        }
    }

    #[cfg(test)]
    pub fn storage_error(&self) -> Option<&str> {
        self.storage_error.as_deref()
    }

    #[cfg(test)]
    pub fn history_since(&self) -> DateTime<Utc> {
        self.journal.started_at
    }

    /// Completed jobs, oldest first.
    pub fn jobs(&self) -> &[FleetCompletedJob] {
        &self.journal.jobs
    }

    /// Recorded time for analytics.
    pub fn coverage(&self) -> Coverage {
        Coverage::new(self.journal.coverage.clone())
    }

    /// Marks `org`'s job data as fresh since `fresh_at`, extending its recorded time to `now`.
    pub fn cover(&mut self, org: &str, fresh_at: DateTime<Utc>, now: DateTime<Utc>) {
        let spans = self.journal.coverage.entry(org.to_lowercase()).or_default();
        match spans.last_mut() {
            Some(last) if now - last.to <= COVERAGE_GAP => last.to = last.to.max(now),
            last => {
                let from = last.map_or(fresh_at, |l| fresh_at.max(l.to)).min(now);
                spans.push(Span { from, to: now });
            }
        }
        self.touched = true;
    }

    /// Whether the job finished: recorded, or seen completed this session.
    pub fn is_completed(&self, id: u64) -> bool {
        self.ids.contains(&id) || self.completed.contains(&id)
    }

    /// Remembers a job GitHub reported completed, whether or not it's recorded.
    pub fn mark_completed(&mut self, id: u64) {
        if self.completed.insert(id) {
            self.completed_order.push_back(id);
            while self.completed_order.len() > RECENTLY_COMPLETED {
                if let Some(old) = self.completed_order.pop_front() {
                    self.completed.remove(&old);
                }
            }
        }
    }

    /// Follows the runs of queued and running self-hosted jobs until they finish.
    pub fn track(&mut self, org: &str, jobs: &[GhJob], runners: &[GhRunner], now: DateTime<Utc>) {
        let org = org.to_lowercase();
        let followed: Vec<&GhJob> = jobs
            .iter()
            .filter(|j| super::self_hosted(j, runners) && !self.is_completed(j.id))
            .collect();
        for job in followed {
            let run = self.journal.pending.entry(job.run_id).or_insert_with(|| PendingRun {
                org: org.clone(),
                repo: job.repo.clone(),
                run_id: job.run_id,
                jobs: BTreeSet::new(),
                first_seen: now,
                last_seen: now,
                checked_at: None,
                unreadable: 0,
            });
            run.last_seen = now;
            self.dirty |= run.jobs.insert(job.id);
            self.touched = true;
        }
    }

    /// Followed runs of `org` that are no longer in the scan and are due a final-state read.
    pub fn due(&self, org: &str, scan: &JobScan, now: DateTime<Utc>, limit: usize) -> Vec<PendingRun> {
        let org = org.to_lowercase();
        let active: HashSet<u64> = scan.jobs.iter().map(|j| j.run_id).collect();
        let mut due: Vec<_> = self
            .journal
            .pending
            .values()
            .filter(|r| r.org == org && !active.contains(&r.run_id))
            .filter(|r| r.checked_at.is_none_or(|at| now - at >= RECHECK_AFTER))
            .cloned()
            .collect();
        due.sort_by_key(|r| (r.checked_at, r.last_seen));
        due.truncate(limit);
        due
    }

    /// The run isn't finished, or couldn't be read completely; ask again later.
    pub fn checked(&mut self, run_id: u64, check: RunCheck, now: DateTime<Utc>) {
        let Some(run) = self.journal.pending.get_mut(&run_id) else {
            return;
        };
        run.checked_at = Some(now);
        self.touched = true;
        if check == RunCheck::Truncated {
            run.unreadable += 1;
            if run.unreadable >= MAX_UNREADABLE {
                self.finish(run_id);
            }
        }
    }

    /// A job seen in a run was read back completed (recorded or not): stop asking about it.
    pub fn resolve(&mut self, run_id: u64, job_id: u64) {
        if let Some(run) = self.journal.pending.get_mut(&run_id) {
            self.dirty |= run.jobs.remove(&job_id);
        }
    }

    /// Stop following a run: it finished, or GitHub no longer has it.
    pub fn finish(&mut self, run_id: u64) {
        self.dirty |= self.journal.pending.remove(&run_id).is_some();
    }

    /// Adds a completed job unless it's already recorded. Returns whether it was new.
    pub fn record(&mut self, job: FleetCompletedJob) -> bool {
        if !self.ids.insert(job.id) {
            return false;
        }
        if let Some(run) = self.journal.pending.get_mut(&job.run_id) {
            run.jobs.remove(&job.id);
        }
        let cursor = job.cursor();
        let at = self.journal.jobs.partition_point(|j| j.cursor() < cursor);
        self.journal.jobs.insert(at, job);
        self.dirty = true;
        true
    }

    /// Applies the age and count limits to jobs and followed runs.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let cutoff = now - JOB_RETENTION;
        let expired = self.journal.jobs.partition_point(|j| j.completed_at < cutoff);
        let excess = (self.journal.jobs.len() - expired).saturating_sub(MAX_JOBS);
        let drop = expired + excess;
        if drop > 0 {
            for job in self.journal.jobs.drain(..drop) {
                self.ids.remove(&job.id);
            }
            self.dirty = true;
        }
        let before = self.journal.pending.len();
        self.journal
            .pending
            .retain(|_, r| now - r.last_seen <= PENDING_RETENTION);
        if self.journal.pending.len() > MAX_PENDING_RUNS {
            let mut oldest: Vec<_> = self.journal.pending.values().map(|r| (r.last_seen, r.run_id)).collect();
            oldest.sort_unstable();
            for (_, run_id) in oldest.iter().take(self.journal.pending.len() - MAX_PENDING_RUNS) {
                self.journal.pending.remove(run_id);
            }
        }
        self.dirty |= self.journal.pending.len() != before;
        let cutoff = now - JOB_RETENTION;
        for spans in self.journal.coverage.values_mut() {
            let expired = spans.partition_point(|s| s.to < cutoff);
            let excess = (spans.len() - expired).saturating_sub(MAX_SPANS_PER_ORG);
            spans.drain(..expired + excess);
        }
        self.journal.coverage.retain(|_, spans| !spans.is_empty());
    }

    /// Serializes the journal if a save is due (`force` ignores the save interval). The
    /// caller writes it without holding the store's lock, then calls [`Self::finish_save`].
    pub fn prepare_save(&mut self, now: DateTime<Utc>, force: bool) -> Option<PendingWrite> {
        let since = |interval| force || self.saved_at.is_none_or(|at| now - at >= interval);
        let due = (self.dirty && since(SAVE_INTERVAL)) || (self.touched && since(CHECKPOINT));
        // An unreadable file's load error already explains why nothing is saved.
        if !due || !self.writable {
            return None;
        }
        let bytes = match serde_json::to_vec(&self.journal) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.storage_error = Some(format!(
                    "Couldn't save job history: {e}. Recent jobs are kept in memory."
                ));
                return None;
            }
        };
        self.dirty = false;
        self.touched = false;
        self.saved_at = Some(now);
        self.seq += 1;
        Some(PendingWrite {
            path: self.path.clone(),
            bytes,
            seq: self.seq,
        })
    }

    /// Records how a prepared write went. A failed write is retried by the next save.
    pub fn finish_save(&mut self, seq: u64, result: Result<(), String>) {
        match result {
            Ok(()) if seq == self.seq => self.storage_error = None,
            Ok(()) => {}
            Err(e) => {
                self.dirty = true;
                self.saved_at = None;
                self.storage_error = Some(format!(
                    "Couldn't save job history: {e}. Recent jobs are kept in memory."
                ));
            }
        }
    }

    /// Prepares and writes in one step, for tests.
    #[cfg(test)]
    pub fn save(&mut self, now: DateTime<Utc>) {
        if let Some(write) = self.prepare_save(now, true) {
            let result = crate::atomic_write::atomic_write(&write.path, &write.bytes).map_err(|e| e.to_string());
            self.finish_save(write.seq, result);
        }
    }

    /// Recent jobs and storage state for the fleet view.
    pub fn activity(&self) -> FleetActivity {
        FleetActivity {
            history_since: Some(self.journal.started_at),
            storage_error: self.storage_error.clone(),
            recent: recent(&self.journal.jobs),
            feeds: Vec::new(),
        }
    }
}

/// Up to [`RECENT_PER_ENTITY`] jobs per runner and per machine, newest first.
pub(crate) fn recent(jobs: &[FleetCompletedJob]) -> Vec<FleetCompletedJob> {
    let mut per_runner: HashMap<&str, usize> = HashMap::new();
    let mut per_machine: HashMap<&str, usize> = HashMap::new();
    let mut overall = 0;
    let mut recent = Vec::new();
    for job in jobs.iter().rev() {
        let mut wanted = overall < RECENT_PER_ENTITY;
        for (key, counts) in [(&job.runner_key, &mut per_runner), (&job.machine_key, &mut per_machine)] {
            if let Some(key) = key {
                let count = counts.entry(key.as_str()).or_default();
                if *count < RECENT_PER_ENTITY {
                    *count += 1;
                    wanted = true;
                }
            }
        }
        if wanted {
            overall += 1;
            recent.push(job.clone());
        }
    }
    recent
}
