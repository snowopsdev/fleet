//! Completed-job history, utilization analytics and the optional real-time job feed.
//!
//! - `store`: the bounded local journal (`job-history.json`) and the runs it still follows
//! - `analytics`: pure summaries of the journal for a time window
//! - `history`: filters and cursor pages over completed jobs
//! - `webhook`, `feed`: `workflow_job` deliveries forwarded by `gh webhook forward`
//! - `demo`: fictional history for demo mode; it never touches the journal
//!
//! Only jobs that ran on a self-hosted runner are recorded. History is collected while the
//! app runs: polling sees jobs that are queued or running during a scan, and the job feed
//! (when on) reports the rest.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::{FleetView, GhJob, GhRunner};

pub mod analytics;
pub mod demo;
pub mod feed;
pub mod history;
mod store;
pub mod webhook;

pub(crate) use store::{JOB_RETENTION, JobStore, MAX_JOBS};

/// How a finished job ended, as GitHub reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub enum FleetJobConclusion {
    Success,
    Failure,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
    Neutral,
    Stale,
    StartupFailure,
    Unknown,
}

impl FleetJobConclusion {
    /// Maps GitHub's `conclusion` field; a missing or new value is `Unknown`.
    pub fn from_github(value: Option<&str>) -> Self {
        match value {
            Some("success") => Self::Success,
            Some("failure") => Self::Failure,
            Some("cancelled") => Self::Cancelled,
            Some("skipped") => Self::Skipped,
            Some("timed_out") => Self::TimedOut,
            Some("action_required") => Self::ActionRequired,
            Some("neutral") => Self::Neutral,
            Some("stale") => Self::Stale,
            Some("startup_failure") => Self::StartupFailure,
            _ => Self::Unknown,
        }
    }

    /// Failures count against the success rate; cancelled and skipped jobs don't.
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Failure | Self::TimedOut | Self::StartupFailure)
    }
}

/// The operating system a job ran on, used to price the hosted-cost estimate.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, schemars::JsonSchema,
)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub enum FleetJobOs {
    Linux,
    Windows,
    Macos,
}

impl FleetJobOs {
    /// From the job's labels first, then the runner's reported OS.
    pub fn detect(labels: &[String], runner_os: Option<&str>) -> Option<Self> {
        let parse = |value: &str| match value.to_ascii_lowercase().as_str() {
            "macos" | "osx" => Some(Self::Macos),
            "windows" => Some(Self::Windows),
            "linux" => Some(Self::Linux),
            _ => None,
        };
        labels
            .iter()
            .find_map(|l| parse(l))
            .or_else(|| runner_os.and_then(parse))
    }
}

/// Where a job's final state came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub enum FleetJobSource {
    /// Read from the GitHub API after the job left the in-flight scan.
    Poll,
    /// A `workflow_job` delivery from the real-time job feed.
    Webhook,
    /// Fictional demo data.
    Demo,
}

/// A job that ran on a self-hosted runner and finished, as recorded on this Mac.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetCompletedJob {
    #[ts(type = "number")]
    pub id: u64,
    #[ts(type = "number")]
    pub run_id: u64,
    /// Organization login, lowercase.
    pub org: String,
    /// `owner/name`
    pub repo: String,
    pub name: String,
    pub workflow_name: Option<String>,
    pub branch: Option<String>,
    pub labels: Vec<String>,
    pub conclusion: FleetJobConclusion,
    /// `gh:<org>:<runner id>` when GitHub still lists the runner.
    pub runner_key: Option<String>,
    pub runner_name: Option<String>,
    /// The machine's stable history identity (`inv:<host>` or `ts:<node>`), when known.
    pub machine_key: Option<String>,
    pub machine_name: Option<String>,
    pub os: Option<FleetJobOs>,
    pub created_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: DateTime<Utc>,
    /// Seconds from creation to start: time spent waiting for a runner.
    pub queue_secs: Option<u32>,
    /// Seconds from start to completion: time a runner spent on the job.
    pub run_secs: Option<u32>,
    pub html_url: Option<String>,
    pub source: FleetJobSource,
}

impl FleetCompletedJob {
    /// A 40-digit cursor ordered by completion time, then job ID.
    pub fn cursor(&self) -> String {
        let millis = u64::try_from(self.completed_at.timestamp_millis()).unwrap_or(0);
        format!("{millis:020}{:020}", self.id)
    }

    /// Builds the record for a finished job, attributing it to a runner and machine in `view`.
    /// Returns `None` unless the job completed on a self-hosted runner.
    pub(crate) fn from_finished(
        org: &str,
        finished: &FinishedJob,
        runners: &[GhRunner],
        view: &FleetView,
        source: FleetJobSource,
    ) -> Option<Self> {
        let job = &finished.job;
        if job.status != "completed" || !ran_self_hosted(job, runners) {
            return None;
        }
        let completed_at = finished.completed_at?;
        let org = org.to_lowercase();
        let runner = job
            .runner_id
            .and_then(|id| runners.iter().find(|r| r.id == id))
            .or_else(|| {
                let name = job.runner_name.as_deref()?;
                runners.iter().find(|r| r.name.eq_ignore_ascii_case(name))
            });
        let runner_key = runner.map(|r| format!("gh:{org}:{}", r.id));
        let machine = runner_key.as_ref().and_then(|key| {
            let machines = || view.machines.iter().chain(&view.other_machines);
            let id = machines()
                .flat_map(|m| &m.runners)
                .find(|r| &r.key == key)
                .and_then(|r| r.machine_id.as_ref())?;
            machines().find(|m| &m.id == id)
        });
        let span = |from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>| {
            let secs = (to? - from?).num_seconds();
            u32::try_from(secs.max(0)).ok()
        };
        Some(Self {
            id: job.id,
            run_id: job.run_id,
            org,
            repo: job.repo.clone(),
            name: job.name.clone(),
            workflow_name: job.workflow_name.clone(),
            branch: job.head_branch.clone(),
            labels: job.labels.clone(),
            conclusion: FleetJobConclusion::from_github(finished.conclusion.as_deref()),
            runner_key,
            runner_name: runner.map(|r| r.name.clone()).or_else(|| job.runner_name.clone()),
            machine_key: machine.map(crate::model::MachineView::preferences_key),
            machine_name: machine.map(|m| m.display_name.clone()),
            os: FleetJobOs::detect(&job.labels, runner.map(|r| r.os.as_str())),
            created_at: job.created_at,
            started_at: job.started_at,
            completed_at,
            queue_secs: span(job.created_at, job.started_at),
            run_secs: span(job.started_at, Some(completed_at)),
            html_url: job.html_url.clone(),
            source,
        })
    }
}

/// A job with how it ended, from the jobs API or a `workflow_job` delivery.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishedJob {
    pub job: GhJob,
    pub conclusion: Option<String>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// Self-hosted jobs ask for the `self-hosted` label or ran on one of the org's runners.
/// Jobs that never reached a runner (cancelled while queued, skipped) are not runner work.
pub(crate) fn ran_self_hosted(job: &GhJob, runners: &[GhRunner]) -> bool {
    let assigned = job.runner_id.is_some() || job.runner_name.is_some();
    assigned && (wants_self_hosted(job) || job.runner_id.is_some_and(|id| runners.iter().any(|r| r.id == id)))
}

/// Whether a queued or running job could be on one of this dashboard's runners.
pub(crate) fn wants_self_hosted(job: &GhJob) -> bool {
    job.labels.iter().any(|l| l.eq_ignore_ascii_case("self-hosted"))
}

/// Per-minute USD rates for GitHub-hosted standard runners, for the cost estimate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetHostedRates {
    /// Linux 2-core (x64).
    pub linux: f64,
    /// Windows 2-core (x64).
    pub windows: f64,
    /// macOS 3- or 4-core (M1 or Intel).
    pub macos: f64,
    /// When the rates were taken from GitHub's pricing page, `YYYY-MM-DD`.
    pub as_of: String,
}

/// GitHub's published standard runner rates, from docs.github.com ("Actions runner pricing").
impl Default for FleetHostedRates {
    fn default() -> Self {
        Self {
            linux: 0.006,
            windows: 0.010,
            macos: 0.062,
            as_of: "2026-10-26".into(),
        }
    }
}

impl FleetHostedRates {
    pub fn rate(&self, os: FleetJobOs) -> f64 {
        match os {
            FleetJobOs::Linux => self.linux,
            FleetJobOs::Windows => self.windows,
            FleetJobOs::Macos => self.macos,
        }
    }
}

/// Settings for job history, kept in the app settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetJobSettings {
    #[serde(default)]
    pub hosted_rates: FleetHostedRates,
    /// Organization logins (lowercase) with the real-time job feed turned on. Off by default.
    #[serde(default)]
    pub feed_orgs: Vec<String>,
}

/// Recently completed jobs for the runner and machine panes, and the job feed's state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetActivity {
    /// When this Mac started recording job history. Earlier jobs aren't known.
    pub history_since: Option<DateTime<Utc>>,
    pub storage_error: Option<String>,
    /// The latest completed jobs: up to 10 per runner and per machine, newest first.
    pub recent: Vec<FleetCompletedJob>,
    /// One entry per organization with the real-time job feed turned on.
    pub feeds: Vec<feed::FleetJobFeedStatus>,
}

#[cfg(test)]
mod tests;
