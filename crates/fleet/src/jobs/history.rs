//! Filtering and cursor pagination over completed jobs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::{FleetCompletedJob, FleetJobConclusion};

/// Jobs per page unless the caller asks for fewer or more.
pub const DEFAULT_JOB_PAGE: usize = 50;
/// The largest page.
pub const MAX_JOB_PAGE: usize = 200;

/// Which completed jobs to read. Cursors come from [`FleetCompletedJob::cursor`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetJobHistoryFilter {
    /// A machine's history identity (`inv:<host>` or `ts:<node>`).
    pub machine_key: Option<String>,
    /// Organization login, case-insensitive.
    pub org: Option<String>,
    /// A runner key (`gh:<org>:<id>`).
    pub runner_key: Option<String>,
    /// A runner name, case-insensitive; matches runners GitHub no longer lists.
    pub runner_name: Option<String>,
    /// `owner/name`, or just the name, case-insensitive.
    pub repo: Option<String>,
    pub conclusion: Option<FleetJobConclusion>,
    /// Exclusive: older jobs than this cursor, newest first.
    pub before: Option<String>,
    /// Exclusive: newer jobs than this cursor, oldest first (unless `before` is also set).
    pub after: Option<String>,
    pub limit: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub enum FleetJobOrder {
    NewestFirst,
    OldestFirst,
}

/// One page of completed jobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetJobHistoryPage {
    pub jobs: Vec<FleetCompletedJob>,
    pub order: FleetJobOrder,
    pub has_more: bool,
    /// Pass as `before` (newest first) or `after` (oldest first) for the next page; null on the last.
    pub next_cursor: Option<String>,
    pub history_since: Option<DateTime<Utc>>,
    pub storage_error: Option<String>,
}

/// Whether `value` is a cursor this module produces: 40 digits.
pub fn valid_cursor(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_digit())
}

impl FleetJobHistoryFilter {
    fn matches(&self, job: &FleetCompletedJob) -> bool {
        let repo_matches = |filter: &str| {
            job.repo.eq_ignore_ascii_case(filter)
                || job
                    .repo
                    .split_once('/')
                    .is_some_and(|(_, name)| name.eq_ignore_ascii_case(filter))
        };
        self.org.as_ref().is_none_or(|o| job.org.eq_ignore_ascii_case(o))
            && self
                .machine_key
                .as_ref()
                .is_none_or(|k| job.machine_key.as_ref() == Some(k))
            && self
                .runner_key
                .as_ref()
                .is_none_or(|k| job.runner_key.as_ref() == Some(k))
            && self
                .runner_name
                .as_ref()
                .is_none_or(|n| job.runner_name.as_ref().is_some_and(|r| r.eq_ignore_ascii_case(n)))
            && self.repo.as_deref().is_none_or(repo_matches)
            && self.conclusion.is_none_or(|c| job.conclusion == c)
    }

    /// The page of `jobs` (oldest first) this filter selects.
    pub fn page(&self, jobs: &[FleetCompletedJob]) -> (Vec<FleetCompletedJob>, FleetJobOrder, bool, Option<String>) {
        let limit = self.limit.map_or(DEFAULT_JOB_PAGE, usize::from).clamp(1, MAX_JOB_PAGE);
        let order = if self.after.is_some() && self.before.is_none() {
            FleetJobOrder::OldestFirst
        } else {
            FleetJobOrder::NewestFirst
        };
        let selected = |job: &&FleetCompletedJob| {
            let cursor = job.cursor();
            self.before.as_ref().is_none_or(|b| cursor < *b)
                && self.after.as_ref().is_none_or(|a| cursor > *a)
                && self.matches(job)
        };
        let mut page: Vec<FleetCompletedJob> = match order {
            FleetJobOrder::NewestFirst => jobs.iter().rev().filter(selected).take(limit + 1).cloned().collect(),
            FleetJobOrder::OldestFirst => jobs.iter().filter(selected).take(limit + 1).cloned().collect(),
        };
        let more = page.len() > limit;
        page.truncate(limit);
        let next = more.then(|| page.last().map(FleetCompletedJob::cursor)).flatten();
        (page, order, more, next)
    }
}
