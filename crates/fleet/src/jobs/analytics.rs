//! Utilization and job summaries for one time window, computed without I/O.
//!
//! Busy time is the overlap of each job's run (start to completion) with the window. Busy
//! percentages divide by the time this Mac was recording within the window, never by time
//! before history started.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::{FleetCompletedJob, FleetHostedRates, FleetJobConclusion, FleetJobOs};
use crate::model::{FleetView, count_u32};

/// Rows kept in the top repository and workflow lists.
pub const TOP_ENTRIES: usize = 10;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
pub enum FleetAnalyticsWindow {
    #[default]
    #[serde(rename = "24h")]
    Day,
    #[serde(rename = "7d")]
    Week,
    #[serde(rename = "30d")]
    Month,
}

impl FleetAnalyticsWindow {
    pub fn duration(self) -> Duration {
        match self {
            Self::Day => Duration::hours(24),
            Self::Week => Duration::days(7),
            Self::Month => Duration::days(30),
        }
    }

    /// Chart bucket width: hourly for a day, six-hourly for a week, daily for a month.
    pub fn bucket(self) -> Duration {
        match self {
            Self::Day => Duration::hours(1),
            Self::Week => Duration::hours(6),
            Self::Month => Duration::days(1),
        }
    }
}

/// Counts and timings for a set of completed jobs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetJobTotals {
    pub jobs: u32,
    pub succeeded: u32,
    /// Failed, timed out or failed to start.
    pub failed: u32,
    pub cancelled: u32,
    /// Skipped, neutral, stale, action required or unknown.
    pub other: u32,
    /// Succeeded / (succeeded + failed); null without either.
    pub success_rate: Option<f64>,
    /// Seconds runners spent on these jobs.
    pub run_secs: u32,
    pub queue_p50_secs: Option<u32>,
    pub queue_p95_secs: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetMachineActivity {
    pub machine_key: String,
    pub name: String,
    /// Runners counted as this machine's capacity.
    pub runners: u32,
    /// Runner-seconds busy within the window.
    pub busy_secs: u32,
    /// Busy runner time / (runners × recorded time in the window), 0–100.
    pub busy_percent: Option<f64>,
    pub totals: FleetJobTotals,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetRunnerActivity {
    pub runner_key: Option<String>,
    pub name: String,
    pub machine_key: Option<String>,
    pub machine_name: Option<String>,
    pub busy_secs: u32,
    /// Busy time / recorded time in the window, 0–100.
    pub busy_percent: Option<f64>,
    pub totals: FleetJobTotals,
}

/// Queue wait for jobs that asked for the same labels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetLabelSetWait {
    pub labels: Vec<String>,
    pub jobs: u32,
    pub p50_secs: u32,
    pub p95_secs: u32,
    pub max_secs: u32,
}

/// A repository or workflow and the runner time it used.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetRunnerTimeShare {
    pub name: String,
    pub repo: String,
    pub jobs: u32,
    pub run_secs: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetActivityBucket {
    pub start: DateTime<Utc>,
    /// Jobs completed in this bucket.
    pub jobs: u32,
    /// Runner-seconds busy in this bucket, across all runners.
    pub busy_secs: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetHostedCostLine {
    pub os: FleetJobOs,
    pub jobs: u32,
    /// Each job's run time rounded up to whole minutes, as GitHub bills.
    pub minutes: u32,
    pub rate: f64,
    pub usd: f64,
}

/// What the window's jobs would have cost on GitHub-hosted standard runners. An estimate:
/// hosted runners differ in speed and size, and included minutes aren't subtracted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetHostedCost {
    pub rates: FleetHostedRates,
    pub lines: Vec<FleetHostedCostLine>,
    pub minutes: u32,
    pub usd: f64,
    /// Jobs without a known operating system, left out of the estimate.
    pub unpriced_jobs: u32,
}

/// Activity for one window, ending now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetAnalytics {
    pub window: FleetAnalyticsWindow,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// Start of recorded time within the window; null when history began after `to`.
    pub observed_from: Option<DateTime<Utc>>,
    pub totals: FleetJobTotals,
    pub machines: Vec<FleetMachineActivity>,
    pub runners: Vec<FleetRunnerActivity>,
    /// Slowest median wait first.
    pub label_sets: Vec<FleetLabelSetWait>,
    pub repos: Vec<FleetRunnerTimeShare>,
    pub workflows: Vec<FleetRunnerTimeShare>,
    pub buckets: Vec<FleetActivityBucket>,
    pub hosted_cost: FleetHostedCost,
}

/// Summarizes `jobs` (any order) for the window ending at `now`.
pub fn summarize(
    jobs: &[FleetCompletedJob],
    history_since: Option<DateTime<Utc>>,
    view: &FleetView,
    rates: &FleetHostedRates,
    window: FleetAnalyticsWindow,
    now: DateTime<Utc>,
) -> FleetAnalytics {
    let from = now - window.duration();
    let observed_from = history_since.map_or(Some(from), |since| (since < now).then(|| since.max(from)));
    let observed_secs = observed_from.map_or(0, |at| (now - at).num_seconds());
    // Busy time counts every job overlapping the window; totals count jobs completed in it.
    let overlapping: Vec<&FleetCompletedJob> = jobs.iter().filter(|j| j.completed_at > from).collect();
    let completed: Vec<&FleetCompletedJob> = overlapping.iter().copied().filter(|j| j.completed_at <= now).collect();
    let busy = |job: &FleetCompletedJob, start: DateTime<Utc>, end: DateTime<Utc>| -> i64 {
        let Some(started) = job.started_at else { return 0 };
        (job.completed_at.min(end) - started.max(start)).num_seconds().max(0)
    };
    let window_busy = |job: &FleetCompletedJob| busy(job, observed_from.unwrap_or(now), now);

    let mut runners = runner_rows(view, &overlapping, &completed, window_busy, observed_secs);
    runners.sort_by_key(|r| (r.name.to_lowercase(), r.runner_key.clone()));
    let mut machines = machine_rows(view, &runners, &completed, observed_secs);
    machines.sort_by_key(|m| (m.name.to_lowercase(), m.machine_key.clone()));

    let buckets = buckets(window, from, now, &overlapping, &completed, busy);
    FleetAnalytics {
        window,
        from,
        to: now,
        observed_from,
        totals: totals(completed.iter().copied()),
        machines,
        runners,
        label_sets: label_sets(&completed),
        repos: shares(&completed, |j| (j.repo.clone(), j.repo.clone())),
        workflows: shares(&completed, |j| {
            (
                j.workflow_name.clone().unwrap_or_else(|| j.name.clone()),
                j.repo.clone(),
            )
        }),
        buckets,
        hosted_cost: hosted_cost(&completed, rates),
    }
}

fn secs(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

fn percent(busy: i64, capacity: i64) -> Option<f64> {
    // Less than a minute of recording says nothing about utilization.
    if capacity < 60 {
        return None;
    }
    // Seconds within 30 days, times a runner count, are far below 2^52: exact as f64.
    Some((busy as f64 / capacity as f64 * 100.0).clamp(0.0, 100.0))
}

fn percentile(sorted: &[u32], p: f64) -> Option<u32> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p * sorted.len() as f64).ceil();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "rank is between 1 and len"
    )]
    let index = (rank as usize).clamp(1, sorted.len()) - 1;
    sorted.get(index).copied()
}

pub(crate) fn totals<'a>(jobs: impl Iterator<Item = &'a FleetCompletedJob>) -> FleetJobTotals {
    let mut t = FleetJobTotals::default();
    let mut waits = Vec::new();
    let mut run_secs = 0u64;
    for job in jobs {
        t.jobs += 1;
        match job.conclusion {
            FleetJobConclusion::Success => t.succeeded += 1,
            FleetJobConclusion::Cancelled => t.cancelled += 1,
            c if c.is_failure() => t.failed += 1,
            _ => t.other += 1,
        }
        run_secs += u64::from(job.run_secs.unwrap_or(0));
        waits.extend(job.queue_secs);
    }
    waits.sort_unstable();
    let decided = t.succeeded + t.failed;
    t.success_rate = (decided > 0).then(|| f64::from(t.succeeded) / f64::from(decided));
    t.run_secs = u32::try_from(run_secs).unwrap_or(u32::MAX);
    t.queue_p50_secs = percentile(&waits, 0.5);
    t.queue_p95_secs = percentile(&waits, 0.95);
    t
}

/// Every runner GitHub lists on a machine, plus runners that only appear in history.
fn runner_rows(
    view: &FleetView,
    overlapping: &[&FleetCompletedJob],
    completed: &[&FleetCompletedJob],
    window_busy: impl Fn(&FleetCompletedJob) -> i64,
    observed_secs: i64,
) -> Vec<FleetRunnerActivity> {
    let mut rows: BTreeMap<String, FleetRunnerActivity> = BTreeMap::new();
    let row = |key: Option<String>, name: String, machine_key, machine_name| FleetRunnerActivity {
        runner_key: key,
        name,
        machine_key,
        machine_name,
        busy_secs: 0,
        busy_percent: None,
        totals: FleetJobTotals::default(),
    };
    for machine in view.machines.iter().chain(&view.other_machines) {
        for runner in machine.runners.iter().filter(|r| r.github.is_some()) {
            rows.insert(
                runner.key.clone(),
                row(
                    Some(runner.key.clone()),
                    runner.name.clone(),
                    Some(machine.preferences_key()),
                    Some(machine.display_name.clone()),
                ),
            );
        }
    }
    for runner in view.unmatched_runners.iter().filter(|r| r.github.is_some()) {
        rows.insert(
            runner.key.clone(),
            row(Some(runner.key.clone()), runner.name.clone(), None, None),
        );
    }
    let identity = |job: &FleetCompletedJob| {
        job.runner_key.clone().or_else(|| {
            job.runner_name
                .as_ref()
                .map(|n| format!("name:{}:{}", job.org, n.to_lowercase()))
        })
    };
    let mut busy: HashMap<String, i64> = HashMap::new();
    for job in overlapping {
        let Some(id) = identity(job) else { continue };
        rows.entry(id.clone()).or_insert_with(|| {
            row(
                job.runner_key.clone(),
                job.runner_name.clone().unwrap_or_else(|| "Unknown runner".into()),
                job.machine_key.clone(),
                job.machine_name.clone(),
            )
        });
        *busy.entry(id).or_default() += window_busy(job);
    }
    let mut by_runner: HashMap<String, Vec<&FleetCompletedJob>> = HashMap::new();
    for job in completed {
        if let Some(id) = identity(job) {
            by_runner.entry(id).or_default().push(job);
        }
    }
    rows.into_iter()
        .map(|(id, mut row)| {
            let seconds = busy.get(&id).copied().unwrap_or(0);
            row.busy_secs = secs(seconds);
            row.busy_percent = percent(seconds, observed_secs);
            row.totals = totals(by_runner.get(&id).into_iter().flatten().copied());
            row
        })
        .collect()
}

fn machine_rows(
    view: &FleetView,
    runners: &[FleetRunnerActivity],
    completed: &[&FleetCompletedJob],
    observed_secs: i64,
) -> Vec<FleetMachineActivity> {
    let names: HashMap<String, String> = view
        .machines
        .iter()
        .chain(&view.other_machines)
        .map(|m| (m.preferences_key(), m.display_name.clone()))
        .collect();
    let mut grouped: BTreeMap<&str, (String, u32, i64)> = BTreeMap::new();
    for runner in runners {
        let Some(key) = runner.machine_key.as_deref() else {
            continue;
        };
        let name = names
            .get(key)
            .cloned()
            .or_else(|| runner.machine_name.clone())
            .unwrap_or_else(|| key.to_string());
        let entry = grouped.entry(key).or_insert((name, 0, 0));
        entry.1 += 1;
        entry.2 += i64::from(runner.busy_secs);
    }
    grouped
        .into_iter()
        .map(|(key, (name, count, busy))| FleetMachineActivity {
            machine_key: key.to_string(),
            name,
            runners: count,
            busy_secs: secs(busy),
            busy_percent: percent(busy, observed_secs.saturating_mul(i64::from(count))),
            totals: totals(
                completed
                    .iter()
                    .copied()
                    .filter(|j| j.machine_key.as_deref() == Some(key)),
            ),
        })
        .collect()
}

fn label_sets(completed: &[&FleetCompletedJob]) -> Vec<FleetLabelSetWait> {
    let mut groups: BTreeMap<String, (Vec<String>, Vec<u32>)> = BTreeMap::new();
    for job in completed {
        let Some(wait) = job.queue_secs else { continue };
        let labels: BTreeSet<String> = job.labels.iter().cloned().collect();
        let mut labels: Vec<String> = labels.into_iter().collect();
        labels.sort_by_key(|l| l.to_lowercase());
        labels.dedup_by_key(|l| l.to_lowercase());
        let key = labels.iter().map(|l| l.to_lowercase()).collect::<Vec<_>>().join(",");
        groups.entry(key).or_insert_with(|| (labels, Vec::new())).1.push(wait);
    }
    let mut sets: Vec<_> = groups
        .into_values()
        .map(|(labels, mut waits)| {
            waits.sort_unstable();
            FleetLabelSetWait {
                labels,
                jobs: count_u32(waits.len()),
                p50_secs: percentile(&waits, 0.5).unwrap_or(0),
                p95_secs: percentile(&waits, 0.95).unwrap_or(0),
                max_secs: waits.last().copied().unwrap_or(0),
            }
        })
        .collect();
    sets.sort_by(|a, b| b.p50_secs.cmp(&a.p50_secs).then_with(|| a.labels.cmp(&b.labels)));
    sets
}

fn shares(
    completed: &[&FleetCompletedJob],
    key: impl Fn(&FleetCompletedJob) -> (String, String),
) -> Vec<FleetRunnerTimeShare> {
    let mut groups: BTreeMap<(String, String), (u32, u64)> = BTreeMap::new();
    for job in completed {
        let entry = groups.entry(key(job)).or_default();
        entry.0 += 1;
        entry.1 += u64::from(job.run_secs.unwrap_or(0));
    }
    let mut rows: Vec<_> = groups
        .into_iter()
        .map(|((name, repo), (jobs, run_secs))| FleetRunnerTimeShare {
            name,
            repo,
            jobs,
            run_secs: u32::try_from(run_secs).unwrap_or(u32::MAX),
        })
        .collect();
    rows.sort_by(|a, b| b.run_secs.cmp(&a.run_secs).then_with(|| a.name.cmp(&b.name)));
    rows.truncate(TOP_ENTRIES);
    rows
}

fn buckets(
    window: FleetAnalyticsWindow,
    from: DateTime<Utc>,
    now: DateTime<Utc>,
    overlapping: &[&FleetCompletedJob],
    completed: &[&FleetCompletedJob],
    busy: impl Fn(&FleetCompletedJob, DateTime<Utc>, DateTime<Utc>) -> i64,
) -> Vec<FleetActivityBucket> {
    let size = window.bucket();
    let count = (window.duration().num_seconds() / size.num_seconds()).max(1);
    (0..count)
        .map(|i| {
            let start = from + size * i32::try_from(i).unwrap_or(i32::MAX);
            let end = (start + size).min(now);
            FleetActivityBucket {
                start,
                jobs: count_u32(
                    completed
                        .iter()
                        .filter(|j| j.completed_at > start && j.completed_at <= end)
                        .count(),
                ),
                busy_secs: secs(overlapping.iter().map(|j| busy(j, start, end)).sum()),
            }
        })
        .collect()
}

fn hosted_cost(completed: &[&FleetCompletedJob], rates: &FleetHostedRates) -> FleetHostedCost {
    let mut lines: BTreeMap<FleetJobOs, (u32, u32)> = BTreeMap::new();
    let mut unpriced_jobs = 0;
    for job in completed {
        let Some(os) = job.os else {
            unpriced_jobs += 1;
            continue;
        };
        let entry = lines.entry(os).or_default();
        entry.0 += 1;
        entry.1 += job.run_secs.unwrap_or(0).div_ceil(60);
    }
    let lines: Vec<_> = lines
        .into_iter()
        .map(|(os, (jobs, minutes))| FleetHostedCostLine {
            os,
            jobs,
            minutes,
            rate: rates.rate(os),
            usd: f64::from(minutes) * rates.rate(os),
        })
        .collect();
    FleetHostedCost {
        rates: rates.clone(),
        minutes: lines.iter().map(|l| l.minutes).sum(),
        usd: lines.iter().map(|l| l.usd).sum(),
        lines,
        unpriced_jobs,
    }
}
