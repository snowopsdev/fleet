//! Utilization and job summaries for one time window, computed without I/O.
//!
//! Busy time is the overlap of each job's run (start to completion) with the window. Busy
//! percentages divide by recorded time ([`Coverage`]): time the app ran with fresh job data
//! for the runner's organization. Unrecorded time is no data, never idle. Only runners GitHub
//! lists now count as capacity, so a re-registered runner doesn't halve its machine's share.

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
    /// Runners counted as this machine's capacity: those GitHub lists now, or, for a machine
    /// that's gone, those in its history.
    pub runners: u32,
    /// Runner-seconds busy within the window, including runners that are gone.
    pub busy_secs: u32,
    /// Busy runner time / the capacity runners' recorded time, 0–100.
    pub busy_percent: Option<f64>,
    pub totals: FleetJobTotals,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetRunnerActivity {
    pub runner_key: Option<String>,
    pub name: String,
    /// Organization login, lowercase.
    pub org: Option<String>,
    pub machine_key: Option<String>,
    pub machine_name: Option<String>,
    /// GitHub lists this runner now; only listed runners count as capacity.
    pub listed: bool,
    pub busy_secs: u32,
    /// Busy time / the organization's recorded time in the window, 0–100.
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
    /// Seconds of this bucket recorded for any organization; 0 means no data, not idle.
    pub covered_secs: u32,
    /// Runner-seconds of recorded capacity in this bucket.
    pub capacity_secs: u32,
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
    /// Start of the first recorded time within the window; null without any.
    pub observed_from: Option<DateTime<Utc>>,
    /// Seconds of the window recorded for any organization.
    pub covered_secs: u32,
    /// Runner-seconds busy, across all runners.
    pub busy_secs: u32,
    /// Runner-seconds of recorded capacity: listed runners times their organization's recorded time.
    pub capacity_secs: u32,
    /// `busy_secs / capacity_secs`, 0–100; null with less than a minute of capacity.
    pub busy_percent: Option<f64>,
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

/// A stretch of recorded time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

/// Recorded time per organization (lowercase login): the app was running and the
/// organization's job data was fresh. Time outside it is unknown, never idle.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Coverage {
    spans: BTreeMap<String, Vec<Span>>,
}

impl Coverage {
    pub fn new(spans: BTreeMap<String, Vec<Span>>) -> Self {
        Self { spans }
    }

    /// Every organization recorded throughout `from..to` (demo data).
    pub fn full(orgs: impl IntoIterator<Item = String>, from: DateTime<Utc>, to: DateTime<Utc>) -> Self {
        Self::new(
            orgs.into_iter()
                .map(|o| (o.to_lowercase(), vec![Span { from, to }]))
                .collect(),
        )
    }

    fn clipped(spans: &[Span], from: DateTime<Utc>, to: DateTime<Utc>) -> impl Iterator<Item = Span> + '_ {
        spans.iter().filter_map(move |s| {
            let span = Span {
                from: s.from.max(from),
                to: s.to.min(to),
            };
            (span.to > span.from).then_some(span)
        })
    }

    /// Seconds of `org`'s recorded time within `from..to`.
    pub fn seconds(&self, org: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> i64 {
        self.spans.get(&org.to_lowercase()).map_or(0, |spans| {
            Self::clipped(spans, from, to)
                .map(|s| (s.to - s.from).num_seconds())
                .sum()
        })
    }

    fn union(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<Span> {
        let mut spans: Vec<Span> = self.spans.values().flat_map(|s| Self::clipped(s, from, to)).collect();
        spans.sort_by_key(|s| s.from);
        let mut merged: Vec<Span> = Vec::new();
        for span in spans {
            match merged.last_mut() {
                Some(last) if span.from <= last.to => last.to = last.to.max(span.to),
                _ => merged.push(span),
            }
        }
        merged
    }

    /// Seconds within `from..to` recorded for any organization.
    pub fn any_seconds(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> i64 {
        self.union(from, to).iter().map(|s| (s.to - s.from).num_seconds()).sum()
    }
}

/// Summarizes `jobs` (any order) for the window ending at `now`.
pub fn summarize(
    jobs: &[FleetCompletedJob],
    coverage: &Coverage,
    view: &FleetView,
    rates: &FleetHostedRates,
    window: FleetAnalyticsWindow,
    now: DateTime<Utc>,
) -> FleetAnalytics {
    let from = now - window.duration();
    // Busy time counts every job overlapping the window; totals count jobs completed in it.
    let overlapping: Vec<&FleetCompletedJob> = jobs.iter().filter(|j| j.completed_at > from).collect();
    let completed: Vec<&FleetCompletedJob> = overlapping.iter().copied().filter(|j| j.completed_at <= now).collect();
    let busy = |job: &FleetCompletedJob, start: DateTime<Utc>, end: DateTime<Utc>| -> i64 {
        let Some(started) = job.started_at else { return 0 };
        (job.completed_at.min(end) - started.max(start)).num_seconds().max(0)
    };

    let mut runners = runner_rows(
        view,
        &overlapping,
        &completed,
        |j| busy(j, from, now),
        |org| coverage.seconds(org, from, now),
    );
    runners.sort_by_key(|r| (r.name.to_lowercase(), r.runner_key.clone()));
    let mut machines = machine_rows(view, &runners, &completed, |org| coverage.seconds(org, from, now));
    machines.sort_by_key(|m| (m.name.to_lowercase(), m.machine_key.clone()));

    // Fleet capacity: listed runners, or every runner in history when none is listed.
    let any_listed = runners.iter().any(|r| r.listed);
    let capacity_orgs: Vec<&str> = runners
        .iter()
        .filter(|r| r.listed || !any_listed)
        .filter_map(|r| r.org.as_deref())
        .collect();
    let capacity: i64 = capacity_orgs.iter().map(|org| coverage.seconds(org, from, now)).sum();
    let busy_total: i64 = runners.iter().map(|r| i64::from(r.busy_secs)).sum();
    let buckets = buckets(
        window,
        from,
        now,
        &overlapping,
        &completed,
        busy,
        coverage,
        &capacity_orgs,
    );
    FleetAnalytics {
        window,
        from,
        to: now,
        observed_from: coverage.union(from, now).first().map(|s| s.from),
        covered_secs: secs(coverage.any_seconds(from, now)),
        busy_secs: secs(busy_total),
        capacity_secs: secs(capacity),
        busy_percent: percent(busy_total, capacity),
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

/// Every runner GitHub lists, plus runners that only appear in history.
fn runner_rows(
    view: &FleetView,
    overlapping: &[&FleetCompletedJob],
    completed: &[&FleetCompletedJob],
    window_busy: impl Fn(&FleetCompletedJob) -> i64,
    covered: impl Fn(&str) -> i64,
) -> Vec<FleetRunnerActivity> {
    let mut rows: BTreeMap<String, FleetRunnerActivity> = BTreeMap::new();
    let listed = |runner: &crate::model::RunnerView, machine: Option<&crate::model::MachineView>| FleetRunnerActivity {
        runner_key: Some(runner.key.clone()),
        name: runner.name.clone(),
        org: runner.org.as_ref().map(|o| o.to_lowercase()),
        machine_key: machine.map(crate::model::MachineView::preferences_key),
        machine_name: machine.map(|m| m.display_name.clone()),
        listed: true,
        busy_secs: 0,
        busy_percent: None,
        totals: FleetJobTotals::default(),
    };
    for machine in view.machines.iter().chain(&view.other_machines) {
        for runner in machine.runners.iter().filter(|r| r.github.is_some()) {
            rows.insert(runner.key.clone(), listed(runner, Some(machine)));
        }
    }
    for runner in view.unmatched_runners.iter().filter(|r| r.github.is_some()) {
        rows.insert(runner.key.clone(), listed(runner, None));
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
        rows.entry(id.clone()).or_insert_with(|| FleetRunnerActivity {
            runner_key: job.runner_key.clone(),
            name: job.runner_name.clone().unwrap_or_else(|| "Unknown runner".into()),
            org: Some(job.org.clone()),
            machine_key: job.machine_key.clone(),
            machine_name: job.machine_name.clone(),
            listed: false,
            busy_secs: 0,
            busy_percent: None,
            totals: FleetJobTotals::default(),
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
            row.busy_percent = percent(seconds, row.org.as_deref().map_or(0, &covered));
            row.totals = totals(by_runner.get(&id).into_iter().flatten().copied());
            row
        })
        .collect()
}

fn machine_rows(
    view: &FleetView,
    runners: &[FleetRunnerActivity],
    completed: &[&FleetCompletedJob],
    covered: impl Fn(&str) -> i64,
) -> Vec<FleetMachineActivity> {
    let names: HashMap<String, String> = view
        .machines
        .iter()
        .chain(&view.other_machines)
        .map(|m| (m.preferences_key(), m.display_name.clone()))
        .collect();
    let mut grouped: BTreeMap<&str, Vec<&FleetRunnerActivity>> = BTreeMap::new();
    for runner in runners {
        if let Some(key) = runner.machine_key.as_deref() {
            grouped.entry(key).or_default().push(runner);
        }
    }
    grouped
        .into_iter()
        .map(|(key, rows)| {
            let name = names
                .get(key)
                .cloned()
                .or_else(|| rows.iter().find_map(|r| r.machine_name.clone()))
                .unwrap_or_else(|| key.to_string());
            // Re-registered runners keep their busy time but not their capacity.
            let any_listed = rows.iter().any(|r| r.listed);
            let capacity: Vec<_> = rows.iter().filter(|r| r.listed || !any_listed).collect();
            let capacity_secs: i64 = capacity.iter().map(|r| r.org.as_deref().map_or(0, &covered)).sum();
            let busy: i64 = rows.iter().map(|r| i64::from(r.busy_secs)).sum();
            FleetMachineActivity {
                machine_key: key.to_string(),
                name,
                runners: count_u32(capacity.len()),
                busy_secs: secs(busy),
                busy_percent: percent(busy, capacity_secs),
                totals: totals(
                    completed
                        .iter()
                        .copied()
                        .filter(|j| j.machine_key.as_deref() == Some(key)),
                ),
            }
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

#[expect(
    clippy::too_many_arguments,
    reason = "one call site; each input is a different view of the window"
)]
fn buckets(
    window: FleetAnalyticsWindow,
    from: DateTime<Utc>,
    now: DateTime<Utc>,
    overlapping: &[&FleetCompletedJob],
    completed: &[&FleetCompletedJob],
    busy: impl Fn(&FleetCompletedJob, DateTime<Utc>, DateTime<Utc>) -> i64,
    coverage: &Coverage,
    capacity_orgs: &[&str],
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
                covered_secs: secs(coverage.any_seconds(start, end)),
                capacity_secs: secs(capacity_orgs.iter().map(|org| coverage.seconds(org, start, end)).sum()),
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
