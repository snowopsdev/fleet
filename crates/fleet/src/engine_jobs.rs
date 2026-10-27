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
use crate::jobs::{FinishedJob, FleetCompletedJob, FleetHostedRates, FleetJobSource, JobStore};
use crate::model::{DataSource, FleetView, GhRunner, JobScan, RepoScope};
use crate::sync::{MutexExt, RwLockExt};

/// Followed runs read per organization per round, bounding API use after a busy period.
const RUNS_PER_ROUND: usize = 20;
/// Jobs seen running but missing from their run's latest attempt, read one by one per run.
const JOBS_PER_RUN: usize = 5;
/// With a healthy job feed, the job scan still runs this often as a fallback.
pub(super) const FEED_FALLBACK_SCAN: Duration = Duration::minutes(3);

/// The job journal and the job feed's supervisors.
#[derive(Debug)]
pub(super) struct JobActivity {
    pub store: Arc<Mutex<JobStore>>,
    pub feeds: Arc<FeedRegistry>,
    /// Bumped when job feed settings change, so the feed controller reconciles at once.
    pub settings: tokio::sync::watch::Sender<u64>,
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
        }
    }
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
    pub checked: Vec<u64>,
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
        let clients = self.github.locked().clone();
        let rounds: Vec<OrgRound> = {
            let obs = self.obs.read_locked();
            config
                .orgs
                .iter()
                .filter_map(|o| {
                    let org = o.login.to_lowercase();
                    let observed = obs.github.get(&org)?;
                    Some(OrgRound {
                        client: clients.get(&org)?.clone(),
                        scan: observed.jobs.as_ref()?.value.clone(),
                        runners: observed.runners.as_ref().map(|r| r.value.clone()).unwrap_or_default(),
                        org,
                    })
                })
                .collect()
        };
        let due: Vec<_> = {
            let mut store = self.job_activity.store.locked();
            rounds
                .iter()
                .map(|round| {
                    store.track(&round.org, &round.scan.jobs, now);
                    store.due(&round.org, &round.scan, now, RUNS_PER_ROUND)
                })
                .collect()
        };
        let mut result = RoundResult::default();
        for (round, runs) in rounds.iter().zip(due) {
            for run in runs {
                match read_run(&round.client, &run.repo, run.run_id, &run.jobs).await {
                    Ok((jobs, complete)) => {
                        result
                            .finished
                            .extend(jobs.into_iter().map(|j| (round.org.clone(), round.runners.clone(), j)));
                        if complete {
                            result.done.push(run.run_id);
                        } else {
                            result.checked.push(run.run_id);
                        }
                    }
                    // The run was deleted, or the repository is no longer readable.
                    Err(GithubError::NotFound(_)) => result.done.push(run.run_id),
                    Err(error) => {
                        tracing::debug!(org = %round.org, run = run.run_id, "job history read failed: {error}");
                        result.checked.push(run.run_id);
                        if matches!(error, GithubError::RateLimited(_) | GithubError::Unauthorized) {
                            break;
                        }
                    }
                }
            }
        }
        self.apply_job_round(result, FleetJobSource::Poll, now).await;
    }

    /// Records finished jobs and run outcomes, then saves off the async runtime.
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
        let saved = tokio::task::spawn_blocking(move || {
            let mut store = store.locked();
            let mut added = 0usize;
            for record in records {
                added += usize::from(store.record(record));
            }
            for run in result.done {
                store.finish(run);
            }
            for run in result.checked {
                store.checked(run, now);
            }
            store.prune(now);
            store.save(now);
            added
        })
        .await;
        match saved {
            Ok(added) if added > 0 => self.view_changed(),
            Ok(_) => {}
            Err(error) => tracing::warn!("saving job history failed: {error}"),
        }
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
                let since = crate::jobs::demo::history_since(now);
                let jobs = crate::jobs::demo::history(view, now - window.duration(), now);
                analytics::summarize(&jobs, Some(since), view, &rates, window, now)
            }
            DataSource::Live => {
                let store = self.job_activity.store.locked();
                analytics::summarize(store.jobs(), Some(store.history_since()), view, &rates, window, now)
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
        if !self.config().orgs.iter().any(|o| o.login.to_lowercase() == key) {
            return Err(EngineError::NotFound(format!(
                "{login} isn't an added organization. Add it in Settings ▸ GitHub first."
            )));
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

/// A run's finished jobs and whether nothing in it is still going.
async fn read_run(
    client: &GithubClient,
    repo: &str,
    run_id: u64,
    seen: &BTreeSet<u64>,
) -> Result<(Vec<FinishedJob>, bool), GithubError> {
    let listing = client.finished_run_jobs(repo, run_id).await?;
    let mut complete = !listing.truncated && listing.jobs.iter().all(|j| j.job.status == "completed");
    let listed: BTreeSet<u64> = listing.jobs.iter().map(|j| j.job.id).collect();
    let mut finished: Vec<FinishedJob> = listing
        .jobs
        .into_iter()
        .filter(|j| j.job.status == "completed")
        .collect();
    // Jobs seen running in an earlier attempt aren't in the latest listing.
    for (index, id) in seen.iter().filter(|id| !listed.contains(id)).enumerate() {
        if index >= JOBS_PER_RUN {
            complete = false;
            break;
        }
        match client.finished_job(repo, *id).await {
            Ok(job) if job.job.status == "completed" => finished.push(job),
            Ok(_) => complete = false,
            Err(GithubError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok((finished, complete))
}
