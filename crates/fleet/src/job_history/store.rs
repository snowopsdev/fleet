//! The bounded local journal: finished jobs, runs still being followed, and recorded time.
//!
//! Writes only this dashboard's own file, atomically; a malformed or newer-format file is
//! preserved and new jobs are kept in memory only.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::{FleetFinishedJob, is_self_hosted};
use crate::model::{FleetView, GhJob, GhRunner, JobScan};

const VERSION: u32 = 1;
pub(crate) const RETENTION: Duration = Duration::days(30);
pub(crate) const MAX_JOBS: usize = 5_000;
const MAX_SPANS: usize = 2_000;
const MAX_PENDING_RUNS: usize = 500;
/// A run unseen for this long (deleted, or waiting on an approval) is no longer followed.
const PENDING_RETENTION: Duration = Duration::days(1);
/// A run read back unfinished is read again after this long.
const RECHECK_AFTER: Duration = Duration::minutes(5);
/// Recording continues the previous span across gaps up to this long (a few job polls).
const SPAN_GAP: Duration = Duration::minutes(3);
/// Extended spans and refreshed runs are saved at least this often.
const CHECKPOINT: Duration = Duration::minutes(5);

/// Time the app was recording: running, with every organization's job scan succeeding.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct Span {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PendingRun {
    /// Organization login, lowercase.
    org: String,
    /// `owner/name`
    repo: String,
    last_seen: DateTime<Utc>,
    checked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Journal {
    version: u32,
    /// Oldest first by completion time.
    jobs: Vec<FleetFinishedJob>,
    pending: BTreeMap<u64, PendingRun>,
    /// Sorted and non-overlapping.
    recorded: Vec<Span>,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            version: VERSION,
            jobs: Vec::new(),
            pending: BTreeMap::new(),
            recorded: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct JobHistory {
    path: PathBuf,
    journal: Journal,
    storage_error: Option<String>,
    writable: bool,
    dirty: bool,
    saved_at: Option<DateTime<Utc>>,
}

impl JobHistory {
    pub fn load(path: PathBuf) -> Self {
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Journal::default()),
            Err(e) => Err(format!("{e}.")),
        };
        let writable = result.is_ok();
        let (journal, storage_error) = match result {
            Ok(journal) => (journal, None),
            Err(e) => (
                Journal::default(),
                Some(format!(
                    "Couldn't read job history: {e} The existing file has been preserved; new jobs are kept only until the app quits."
                )),
            ),
        };
        Self {
            path,
            journal,
            storage_error,
            writable,
            dirty: false,
            saved_at: None,
        }
    }

    /// Extends recorded time to `now`, or starts a new span after a gap.
    pub fn cover(&mut self, now: DateTime<Utc>) {
        match self.journal.recorded.last_mut() {
            Some(last) if now < last.to => {}
            Some(last) if now - last.to <= SPAN_GAP => last.to = now,
            _ => {
                self.journal.recorded.push(Span { from: now, to: now });
                self.dirty = true;
            }
        }
    }

    /// Follows the runs of queued and running self-hosted jobs in a fresh scan.
    pub fn track(&mut self, org: &str, scan: &JobScan, runners: &[GhRunner], now: DateTime<Utc>) {
        for job in scan.jobs.iter().filter(|j| is_self_hosted(j, runners)) {
            let run = self.journal.pending.entry(job.run_id).or_insert_with(|| {
                self.dirty = true;
                PendingRun {
                    org: org.to_lowercase(),
                    repo: job.repo.clone(),
                    last_seen: now,
                    checked_at: None,
                }
            });
            run.last_seen = now;
            run.checked_at = None;
        }
    }

    /// Followed runs of `org` that left the scan and are due a read: `(run id, owner/name)`.
    pub fn due(&self, org: &str, scan: &JobScan, now: DateTime<Utc>, limit: usize) -> Vec<(u64, String)> {
        let org = org.to_lowercase();
        let active: HashSet<u64> = scan.jobs.iter().map(|j| j.run_id).collect();
        let mut due: Vec<_> = self
            .journal
            .pending
            .iter()
            .filter(|(id, run)| {
                run.org == org && !active.contains(id) && run.checked_at.is_none_or(|at| now - at >= RECHECK_AFTER)
            })
            .collect();
        due.sort_by_key(|(_, run)| (run.checked_at, run.last_seen));
        due.into_iter()
            .take(limit)
            .map(|(id, run)| (*id, run.repo.clone()))
            .collect()
    }

    /// Records a run's completed self-hosted jobs. A run whose complete listing has no
    /// unfinished self-hosted job is no longer followed; otherwise it's read again later.
    pub fn read_run(&mut self, run_id: u64, jobs: &[GhJob], truncated: bool, runners: &[GhRunner], now: DateTime<Utc>) {
        let Some(org) = self.journal.pending.get(&run_id).map(|r| r.org.clone()) else {
            return;
        };
        let mut finished = !truncated;
        for job in jobs.iter().filter(|j| is_self_hosted(j, runners)) {
            if job.status != "completed" {
                finished = false;
            } else if !self.journal.jobs.iter().any(|j| j.id == job.id)
                && let Some(record) = FleetFinishedJob::from_github(&org, job, runners)
            {
                self.journal.jobs.push(record);
                self.dirty = true;
            }
        }
        if finished {
            self.forget_run(run_id);
        } else {
            self.checked(run_id, now);
        }
    }

    /// Stops following a run GitHub no longer has.
    pub fn forget_run(&mut self, run_id: u64) {
        self.dirty |= self.journal.pending.remove(&run_id).is_some();
    }

    /// A read failed or found the run unfinished: wait before reading it again.
    pub fn checked(&mut self, run_id: u64, now: DateTime<Utc>) {
        if let Some(run) = self.journal.pending.get_mut(&run_id) {
            run.checked_at = Some(now);
        }
    }

    /// Applies the age and count limits.
    fn prune(&mut self, now: DateTime<Utc>) {
        let cutoff = now - RETENTION;
        let j = &mut self.journal;
        let before = (j.jobs.len(), j.pending.len(), j.recorded.len());
        j.jobs.retain(|job| job.completed_at >= cutoff);
        j.jobs.sort_by_key(|job| (job.completed_at, job.id));
        j.jobs.drain(..j.jobs.len().saturating_sub(MAX_JOBS));
        j.recorded.retain(|s| s.to >= cutoff);
        j.recorded.drain(..j.recorded.len().saturating_sub(MAX_SPANS));
        j.pending.retain(|_, r| now - r.last_seen <= PENDING_RETENTION);
        if j.pending.len() > MAX_PENDING_RUNS {
            let mut oldest: Vec<_> = j.pending.iter().map(|(id, r)| (r.last_seen, *id)).collect();
            oldest.sort_unstable();
            for (_, id) in &oldest[..oldest.len() - MAX_PENDING_RUNS] {
                j.pending.remove(id);
            }
        }
        self.dirty |= before != (j.jobs.len(), j.pending.len(), j.recorded.len());
    }

    /// Prunes, then saves new jobs and runs at once and other changes at checkpoints.
    /// Performs disk I/O; call off the async runtime.
    pub fn save_due(&mut self, now: DateTime<Utc>) {
        self.prune(now);
        if !self.dirty && self.saved_at.is_some_and(|at| now - at < CHECKPOINT) {
            return;
        }
        match self.save() {
            Ok(()) => {
                self.dirty = false;
                self.saved_at = Some(now);
                self.storage_error = None;
            }
            Err(e) => self.storage_error = Some(e),
        }
    }

    fn save(&self) -> Result<(), String> {
        if !self.writable {
            return Err(self
                .storage_error
                .clone()
                .unwrap_or_else(|| "Job history cannot be saved.".into()));
        }
        let bytes = serde_json::to_vec(&self.journal).map_err(|e| format!("Couldn't save job history: {e}."))?;
        crate::atomic_write::atomic_write(&self.path, &bytes).map_err(|e| format!("Couldn't save job history: {e}."))
    }

    pub fn attach(&self, view: &mut FleetView, now: DateTime<Utc>) {
        view.activity = super::summary::activity(&self.journal.jobs, &self.journal.recorded, view, now);
        view.activity.storage_error.clone_from(&self.storage_error);
    }

    #[cfg(test)]
    pub fn jobs(&self) -> &[FleetFinishedJob] {
        &self.journal.jobs
    }

    #[cfg(test)]
    pub fn recorded(&self) -> &[Span] {
        &self.journal.recorded
    }

    #[cfg(test)]
    pub fn storage_error(&self) -> Option<&str> {
        self.storage_error.as_deref()
    }
}
