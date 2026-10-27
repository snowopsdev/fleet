//! Completed-job history: following in-flight runs to their end, settings, and reads.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};

use super::{Engine, EngineError};
use crate::github::{GithubClient, GithubError};
use crate::jobs::analytics::{self, FleetAnalytics, FleetAnalyticsWindow};
use crate::jobs::feed::FeedRegistry;
use crate::jobs::history::{FleetJobHistoryFilter, FleetJobHistoryPage};
use crate::jobs::{FinishedJob, FleetCompletedJob, FleetHostedRates, FleetJobSource, JobStore, RunCheck};
use crate::model::{DataSource, FleetView, GhRunner, JobScan, RepoScope};
use crate::sync::{MutexExt, RwLockExt};

/// Followed runs read per organization per round, bounding API use after a busy period.
const RUNS_PER_ROUND: usize = 20;
/// Jobs seen running but missing from their run's latest attempt, read one by one per run.
const JOBS_PER_RUN: usize = 5;
/// History reads stop for the round when an account has fewer requests left than this,
/// leaving its budget to the runner and job polls.
const KEEP_REQUESTS: u32 = 200;
/// With a healthy job feed, the job scan still runs this often as a fallback.
pub(super) const FEED_FALLBACK_SCAN: Duration = Duration::minutes(3);

/// The job journal and the job feed's supervisors.
#[derive(Debug)]
pub(super) struct JobActivity {
    pub store: Arc<Mutex<JobStore>>,
    pub feeds: Arc<FeedRegistry>,
    /// Bumped when job feed settings change, so the feed controller reconciles at once.
    pub settings: tokio::sync::watch::Sender<u64>,
    /// The sequence number of the last journal write, so an older one never lands last.
    pub writer: Arc<Mutex<u64>>,
}

impl JobActivity {
    pub fn load(config_path: &Path) -> Self {
        Self {
            store: Arc::new(Mutex::new(JobStore::load(
                config_path.with_file_name("job-history.json"),
                Utc::now(),
            ))),
            feeds: Arc::new(FeedRegistry::default()),
            settings: tokio::sync::watch::channel(0).0,
            writer: Arc::new(Mutex::new(0)),
        }
    }
}

/// Writes the journal if a save is due, serializing under the store's lock and writing
/// without it. Performs disk I/O; call off the async runtime.
fn persist(store: &Mutex<JobStore>, writer: &Mutex<u64>, now: DateTime<Utc>, force: bool) {
    let Some(write) = store.locked().prepare_save(now, force) else {
        return;
    };
    let result = {
        let mut written = writer.locked();
        if write.seq <= *written {
            // A newer snapshot is already on disk.
            Ok(())
        } else {
            let result = crate::atomic_write::atomic_write(&write.path, &write.bytes).map_err(|e| e.to_string());
            if result.is_ok() {
                *written = write.seq;
            }
            result
        }
    };
    store.locked().finish_save(write.seq, result);
}

/// One organization's inputs for a history round.
struct OrgRound {
    org: String,
    client: Arc<GithubClient>,
    scan: JobScan,
    runners: Vec<GhRunner>,
}

/// What a round learned, applied to the journal in one write.
#[derive(Default)]
pub(super) struct RoundResult {
    /// Organization, its runners, and a job GitHub reports finished.
    pub finished: Vec<(String, Vec<GhRunner>, FinishedJob)>,
    /// Runs no longer worth following.
    pub done: Vec<u64>,
    /// Runs to ask about again later.
    pub checked: Vec<(u64, RunCheck)>,
    /// (run, job) pairs read back completed, recorded or not.
    pub resolved: Vec<(u64, u64)>,
}

impl Engine {
    /// The scheduled job poll: scans in-flight jobs (less often while the job feed is
    /// healthy), then reads final states for runs that left the scan.
    pub async fn poll_github_jobs_and_history(&self) {
        if !self.job_feed_covers_scans(Utc::now()) {
            self.poll_github_jobs().await;
        }
        self.update_job_history().await;
    }

    /// True when every organization with a job scan has a forwarding feed and a recent scan.
    fn job_feed_covers_scans(&self, now: DateTime<Utc>) -> bool {
        let config = self.config();
        let obs = self.obs.read_locked();
        let mut scanned = config
            .orgs
            .iter()
            .filter(|o| o.repo_scope != RepoScope::None)
            .peekable();
        scanned.peek().is_some()
            && scanned.all(|o| {
                let key = o.login.to_lowercase();
                self.job_activity.feeds.forwarding(&key)
                    && obs
                        .github
                        .get(&key)
                        .and_then(|g| g.jobs.as_ref())
                        .is_some_and(|scan| now - scan.at < FEED_FALLBACK_SCAN)
            })
    }

    /// Follows self-hosted runs seen in the latest scans and records their jobs once GitHub
    /// reports them finished. Uses only clients the regular polls already created.
    pub async fn update_job_history(&self) {
        let config = self.config();
        if config.demo_mode {
            return;
        }
        let now = Utc::now();
        let clients: Vec<_> = config
            .orgs
            .iter()
            .filter_map(|o| {
                let org = o.login.to_lowercase();
                Some((self.polled_client(&org, now)?, org))
            })
            .collect();
        let rounds: Vec<OrgRound> = {
            let obs = self.obs.read_locked();
            clients
                .into_iter()
                .filter_map(|(client, org)| {
                    let observed = obs.github.get(&org)?;
                    Some(OrgRound {
                        client,
                        scan: observed.jobs.as_ref()?.value.clone(),
                        runners: crate::jobs::account_runners(observed),
                        org,
                    })
                })
                .collect()
        };
        // Recorded time: the app is running and the account's job data is fresh.
        let fresh: Vec<(String, DateTime<Utc>)> = {
            let obs = self.obs.read_locked();
            config
                .orgs
                .iter()
                .filter_map(|o| {
                    let org = o.login.to_lowercase();
                    let at = obs.github.get(&org)?.jobs.as_ref()?.at;
                    (now - at <= crate::derive::JOBS_STALE_AFTER).then_some((org, at))
                })
                .collect()
        };
        let due: Vec<_> = {
            let mut store = self.job_activity.store.locked();
            for (org, at) in &fresh {
                store.cover(org, *at, now);
            }
            rounds
                .iter()
                .map(|round| {
                    store.track(&round.org, &round.scan.jobs, &round.runners, now);
                    store.due(&round.org, &round.scan, now, RUNS_PER_ROUND)
                })
                .collect()
        };
        let mut result = RoundResult::default();
        for (round, runs) in rounds.iter().zip(due) {
            for run in runs {
                if round.client.rate_limit().is_some_and(|r| r.remaining < KEEP_REQUESTS) {
                    break;
                }
                match read_run(&round.client, &run.repo, run.run_id, &run.jobs).await {
                    Ok(read) => {
                        result
                            .resolved
                            .extend(read.resolved.into_iter().map(|id| (run.run_id, id)));
                        result.finished.extend(
                            read.finished
                                .into_iter()
                                .map(|j| (round.org.clone(), round.runners.clone(), j)),
                        );
                        match read.state {
                            None => result.done.push(run.run_id),
                            Some(check) => result.checked.push((run.run_id, check)),
                        }
                    }
                    // The run was deleted, or the repository is no longer readable.
                    Err(GithubError::NotFound(_)) => result.done.push(run.run_id),
                    Err(error) => {
                        tracing::debug!(org = %round.org, run = run.run_id, "job history read failed: {error}");
                        result.checked.push((run.run_id, RunCheck::Pending));
                        if let GithubError::RateLimited { resets_at } = error {
                            self.back_off(&round.org, resets_at);
                            break;
                        }
                        if matches!(error, GithubError::Unauthorized) {
                            break;
                        }
                    }
                }
            }
            if let Some(observed) = self.obs.write_locked().github.get_mut(&round.org) {
                observed.rate_limit = round.client.rate_limit();
            }
        }
        self.apply_job_round(result, FleetJobSource::Poll, now).await;
    }

    /// A history read hit the rate limit: every poll of the account waits for GitHub's reset.
    fn back_off(&self, org: &str, resets_at: DateTime<Utc>) {
        let now = Utc::now();
        if let Some(observed) = self.obs.write_locked().github.get_mut(org) {
            let until = resets_at.max(now) + Duration::seconds(2);
            observed.rate_limited_until = Some(observed.rate_limited_until.map_or(until, |at| at.max(until)));
        }
    }

    /// Records finished jobs and run outcomes, then saves off the async runtime. New jobs reach
    /// disk within the save interval (10 seconds) of the next round, or at app exit.
    pub(super) async fn apply_job_round(&self, result: RoundResult, source: FleetJobSource, now: DateTime<Utc>) {
        let records: Vec<FleetCompletedJob> = if result.finished.is_empty() {
            Vec::new()
        } else {
            let view = self.view();
            result
                .finished
                .iter()
                .filter_map(|(org, runners, job)| FleetCompletedJob::from_finished(org, job, runners, &view, source))
                .collect()
        };
        let store = self.job_activity.store.clone();
        let writer = self.job_activity.writer.clone();
        let saved = tokio::task::spawn_blocking(move || {
            let added = {
                let mut store = store.locked();
                let mut added = 0usize;
                for (_, _, job) in &result.finished {
                    store.mark_completed(job.job.id);
                }
                for record in records {
                    added += usize::from(store.record(record));
                }
                for (run, job) in result.resolved {
                    store.resolve(run, job);
                }
                for run in result.done {
                    store.finish(run);
                }
                for (run, check) in result.checked {
                    store.checked(run, check, now);
                }
                store.prune(now);
                added
            };
            persist(&store, &writer, now, false);
            added
        })
        .await;
        match saved {
            Ok(added) if added > 0 => self.view_changed(),
            Ok(_) => {}
            Err(error) => tracing::warn!("saving job history failed: {error}"),
        }
    }

    /// Writes any unsaved job history now. Performs disk I/O; the app calls it on exit.
    pub fn flush_job_history(&self) {
        persist(&self.job_activity.store, &self.job_activity.writer, Utc::now(), true);
    }

    pub(super) fn attach_job_activity(&self, view: &mut FleetView) {
        let mut activity = self.job_activity.store.locked().activity();
        activity.feeds = self.job_activity.feeds.statuses();
        view.activity = activity;
    }

    /// One page of completed jobs from `view`'s source: the journal, or demo history.
    pub fn job_history(&self, view: &FleetView, filter: &FleetJobHistoryFilter) -> FleetJobHistoryPage {
        let (jobs, order, has_more, next_cursor) = match view.source {
            DataSource::Demo => {
                let since = crate::jobs::demo::history_since(view.generated_at);
                filter.page(&crate::jobs::demo::history(view, since, view.generated_at))
            }
            DataSource::Live => filter.page(self.job_activity.store.locked().jobs()),
        };
        FleetJobHistoryPage {
            jobs,
            order,
            has_more,
            next_cursor,
            history_since: view.activity.history_since,
            storage_error: view.activity.storage_error.clone(),
        }
    }

    /// Utilization and job summaries for the window ending at `view.generated_at`.
    pub fn job_analytics(&self, view: &FleetView, window: FleetAnalyticsWindow) -> FleetAnalytics {
        let rates = self.config().job_activity.hosted_rates;
        let now = view.generated_at;
        match view.source {
            DataSource::Demo => {
                let jobs = crate::jobs::demo::history(view, now - window.duration(), now);
                analytics::summarize(
                    &jobs,
                    &crate::jobs::demo::coverage(view, now),
                    view,
                    &rates,
                    window,
                    now,
                )
            }
            DataSource::Live => {
                let store = self.job_activity.store.locked();
                analytics::summarize(store.jobs(), &store.coverage(), view, &rates, window, now)
            }
        }
    }

    /// Saves the per-minute rates used for the hosted-cost estimate.
    pub fn set_hosted_rates(&self, rates: FleetHostedRates) -> Result<(), EngineError> {
        let valid = |rate: f64| rate.is_finite() && (0.0..=10.0).contains(&rate);
        if ![rates.linux, rates.windows, rates.macos].into_iter().all(valid) {
            return Err(EngineError::Invalid(
                "Enter per-minute rates between $0 and $10.".into(),
            ));
        }
        let as_of = rates.as_of.trim().to_string();
        if as_of.is_empty() || as_of.chars().count() > 32 {
            return Err(EngineError::Invalid(
                "Enter when the rates were checked, like 2026-10-26.".into(),
            ));
        }
        self.update_config(|c| c.job_activity.hosted_rates = FleetHostedRates { as_of, ..rates })
    }

    /// Turns the real-time job feed on or off for an added organization.
    pub fn set_job_feed(&self, login: &str, enabled: bool) -> Result<(), EngineError> {
        let key = login.to_lowercase();
        let Some(account) = self.config().orgs.into_iter().find(|o| o.login.to_lowercase() == key) else {
            return Err(EngineError::NotFound(format!(
                "{login} isn't an added organization. Add it in Settings ▸ GitHub first."
            )));
        };
        if enabled && account.kind == crate::model::AccountKind::User {
            return Err(EngineError::Invalid(
                "The real-time job feed forwards organization webhooks, so it isn't available for personal accounts."
                    .into(),
            ));
        }
        self.update_config(|c| {
            let orgs = &mut c.job_activity.feed_orgs;
            orgs.retain(|o| *o != key);
            if enabled {
                orgs.push(key);
                orgs.sort();
            }
        })?;
        self.job_activity.settings.send_modify(|v| *v = v.wrapping_add(1));
        Ok(())
    }
}

/// What one read of a followed run found.
struct RunRead {
    /// Jobs GitHub reports completed, recordable or not.
    finished: Vec<FinishedJob>,
    /// Jobs seen earlier that were read back completed or are gone.
    resolved: Vec<u64>,
    /// `None` when nothing in the run is left to follow.
    state: Option<RunCheck>,
}

/// Reads a run's latest attempt, plus up to [`JOBS_PER_RUN`] jobs seen in earlier attempts.
async fn read_run(
    client: &GithubClient,
    repo: &str,
    run_id: u64,
    seen: &BTreeSet<u64>,
) -> Result<RunRead, GithubError> {
    let listing = client.finished_run_jobs(repo, run_id).await?;
    let mut pending = listing.jobs.iter().any(|j| j.job.status != "completed");
    let listed: BTreeSet<u64> = listing.jobs.iter().map(|j| j.job.id).collect();
    let mut finished: Vec<FinishedJob> = listing
        .jobs
        .into_iter()
        .filter(|j| j.job.status == "completed")
        .collect();
    let mut resolved: Vec<u64> = finished
        .iter()
        .map(|j| j.job.id)
        .filter(|id| seen.contains(id))
        .collect();
    // Jobs seen running in an earlier attempt aren't in the latest listing. Each is read once
    // it's done; later rounds read the rest.
    for (index, id) in seen.iter().filter(|id| !listed.contains(id)).enumerate() {
        if index >= JOBS_PER_RUN {
            pending = true;
            break;
        }
        match client.finished_job(repo, *id).await {
            Ok(job) if job.job.status == "completed" => {
                resolved.push(*id);
                finished.push(job);
            }
            Ok(_) => pending = true,
            Err(GithubError::NotFound(_)) => resolved.push(*id),
            Err(error) => return Err(error),
        }
    }
    let state = if listing.truncated {
        Some(RunCheck::Truncated)
    } else if pending {
        Some(RunCheck::Pending)
    } else {
        None
    };
    Ok(RunRead {
        finished,
        resolved,
        state,
    })
}
