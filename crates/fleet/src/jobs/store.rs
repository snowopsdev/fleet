//! The bounded local job journal, and the workflow runs it follows until they finish.
//!
//! Writes only this dashboard's own file, atomically. A corrupt or newer-format file is
//! preserved: jobs are still kept in memory for this session, and the view shows why
//! they can't be saved.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::{FleetActivity, FleetCompletedJob};
use crate::model::{GhJob, JobScan};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    version: u32,
    /// When recording started; analytics never count time before it as idle.
    started_at: DateTime<Utc>,
    /// Ascending by completion time, then job ID.
    jobs: Vec<FleetCompletedJob>,
    #[serde(default)]
    pending: BTreeMap<u64, PendingRun>,
}

impl Journal {
    fn new(now: DateTime<Utc>) -> Self {
        Self {
            version: VERSION,
            started_at: now,
            jobs: Vec::new(),
            pending: BTreeMap::new(),
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
        }
    }

    #[cfg(test)]
    pub fn storage_error(&self) -> Option<&str> {
        self.storage_error.as_deref()
    }

    pub fn history_since(&self) -> DateTime<Utc> {
        self.journal.started_at
    }

    /// Completed jobs, oldest first.
    pub fn jobs(&self) -> &[FleetCompletedJob] {
        &self.journal.jobs
    }

    /// Follows the runs of queued and running self-hosted jobs until they finish.
    pub fn track(&mut self, org: &str, jobs: &[GhJob], now: DateTime<Utc>) {
        let org = org.to_lowercase();
        for job in jobs
            .iter()
            .filter(|j| super::wants_self_hosted(j) && !self.ids.contains(&j.id))
        {
            let run = self.journal.pending.entry(job.run_id).or_insert_with(|| PendingRun {
                org: org.clone(),
                repo: job.repo.clone(),
                run_id: job.run_id,
                jobs: BTreeSet::new(),
                first_seen: now,
                last_seen: now,
                checked_at: None,
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

    /// The run is still going (or couldn't be read completely); ask again later.
    pub fn checked(&mut self, run_id: u64, now: DateTime<Utc>) {
        if let Some(run) = self.journal.pending.get_mut(&run_id) {
            run.checked_at = Some(now);
            self.touched = true;
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
    }

    /// Writes the journal if it changed. Performs disk I/O; call off the async runtime.
    pub fn save(&mut self, now: DateTime<Utc>) {
        let checkpoint = self.touched && self.saved_at.is_none_or(|at| now - at >= CHECKPOINT);
        if !self.dirty && !checkpoint {
            return;
        }
        if !self.writable {
            // The load error already explains why; keep it visible.
            return;
        }
        let write = || -> Result<(), Box<dyn std::error::Error>> {
            crate::atomic_write::atomic_write(&self.path, &serde_json::to_vec(&self.journal)?)?;
            Ok(())
        };
        match write() {
            Ok(()) => {
                self.dirty = false;
                self.touched = false;
                self.saved_at = Some(now);
                self.storage_error = None;
            }
            Err(e) => {
                self.storage_error = Some(format!(
                    "Couldn't save job history: {e}. Recent jobs are kept in memory."
                ))
            }
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
