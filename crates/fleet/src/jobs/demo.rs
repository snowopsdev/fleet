//! Fictional completed-job history for demo mode. Never reads or writes the live journal.
//!
//! Jobs sit on a fixed time grid, so a job keeps its timestamps as `now` advances and the
//! output for a given `now` is always the same.

use chrono::{DateTime, Duration, Utc};

use super::{FleetCompletedJob, FleetJobConclusion, FleetJobOs, FleetJobSource, JOB_RETENTION, store};
use crate::model::FleetView;

/// Each runner gets at most one job per slot; a job's wait and run always fit in its slot.
const SLOT_SECS: i64 = 40 * 60;

/// SplitMix64: a small, deterministic generator for fictional data.
fn mix(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A value in `0..n`.
fn pick(seed: u64, n: u64) -> u64 {
    mix(seed) % n.max(1)
}

struct Profile {
    /// Percent of slots with a job.
    busy: u64,
    work: &'static [(&'static str, &'static str, &'static str)],
}

fn profile(os: Option<FleetJobOs>, labels: &[String]) -> Profile {
    let has = |label: &str| labels.iter().any(|l| l.eq_ignore_ascii_case(label));
    if has("gpu") {
        return Profile {
            busy: 18,
            work: &[
                ("acme-demo/ml", "Nightly", "train-small"),
                ("acme-demo/ml", "Eval", "benchmark"),
            ],
        };
    }
    match os {
        Some(FleetJobOs::Macos) => Profile {
            busy: 62,
            work: &[
                ("acme-demo/ios-app", "CI", "build (macOS)"),
                ("acme-demo/ios-app", "CI", "ui-tests"),
                ("acme-demo/ios-app", "Release", "archive"),
                ("acme-demo/web", "CI", "test-unit"),
            ],
        },
        Some(FleetJobOs::Windows) => Profile {
            busy: 14,
            work: &[
                ("acme-demo/desktop", "CI", "package (Windows)"),
                ("acme-demo/desktop", "CI", "test"),
            ],
        },
        Some(FleetJobOs::Linux) | None => Profile {
            busy: 38,
            work: &[
                ("acme-demo/web", "CI", "lint"),
                ("acme-demo/web", "Deploy", "deploy-preview"),
                ("acme-demo/api", "CI", "test"),
                ("acme-demo/infra", "Terraform", "plan"),
            ],
        },
    }
}

/// Fictional jobs completed after `since`, oldest first, for every GitHub runner in `view`.
pub fn history(view: &FleetView, since: DateTime<Utc>, now: DateTime<Utc>) -> Vec<FleetCompletedJob> {
    let since = since.max(now - JOB_RETENTION);
    let machines = || view.machines.iter().chain(&view.other_machines);
    let runners = machines()
        .flat_map(|m| m.runners.iter().map(move |r| (r, Some(m))))
        .chain(view.unmatched_runners.iter().map(|r| (r, None)));
    let mut jobs = Vec::new();
    for (runner, machine) in runners {
        let Some(gh) = &runner.github else { continue };
        let index = gh.id;
        let os = FleetJobOs::detect(&gh.labels, Some(&gh.os));
        let profile = profile(os, &gh.labels);
        // A runner GitHub reports offline stopped taking jobs a while ago.
        let until = if gh.status == "offline" {
            now - Duration::hours(3)
        } else {
            now
        };
        let offset = i64::try_from(pick(index, 600)).unwrap_or(0);
        let first = (since.timestamp() - SLOT_SECS) / SLOT_SECS;
        let last = until.timestamp() / SLOT_SECS;
        for slot in first..=last {
            let seed = index.wrapping_mul(1_000_003) ^ u64::try_from(slot).unwrap_or(0);
            if pick(seed, 100) >= profile.busy {
                continue;
            }
            let created = slot * SLOT_SECS + offset;
            let queue = 4 + i64::try_from(pick(seed ^ 1, if pick(seed ^ 2, 10) == 0 { 600 } else { 120 })).unwrap_or(0);
            let run = 90 + i64::try_from(pick(seed ^ 3, 1600)).unwrap_or(0);
            let (Some(created_at), Some(started_at), Some(completed_at)) = (
                DateTime::from_timestamp(created, 0),
                DateTime::from_timestamp(created + queue, 0),
                DateTime::from_timestamp(created + queue + run, 0),
            ) else {
                continue;
            };
            if completed_at <= since || completed_at > until {
                continue;
            }
            let work = usize::try_from(pick(seed ^ 4, u64::try_from(profile.work.len()).unwrap_or(1))).unwrap_or(0);
            let (repo, workflow, name) = profile
                .work
                .get(work)
                .copied()
                .unwrap_or(("acme-demo/web", "CI", "build"));
            let conclusion = match pick(seed ^ 5, 100) {
                0..=87 => FleetJobConclusion::Success,
                88..=95 => FleetJobConclusion::Failure,
                _ => FleetJobConclusion::Cancelled,
            };
            let mut labels: Vec<String> = gh
                .labels
                .iter()
                .filter(|l| !l.eq_ignore_ascii_case("ARM64") && !l.eq_ignore_ascii_case("X64"))
                .cloned()
                .collect();
            if pick(seed ^ 6, 2) == 0 && labels.len() > 2 {
                labels.truncate(2);
            }
            let id = 7_000_000_000 + index * 100_000 + u64::try_from(slot.rem_euclid(100_000)).unwrap_or(0);
            let run_id = id - 6_100_000_000;
            jobs.push(FleetCompletedJob {
                id,
                run_id,
                org: runner.org.clone().unwrap_or_else(|| "acme-demo".into()),
                repo: repo.into(),
                name: name.into(),
                workflow_name: Some(workflow.into()),
                branch: Some(if pick(seed ^ 7, 4) == 0 { "release" } else { "main" }.into()),
                labels,
                conclusion,
                runner_key: Some(runner.key.clone()),
                runner_name: Some(runner.name.clone()),
                machine_key: machine.map(crate::model::MachineView::preferences_key),
                machine_name: machine.map(|m| m.display_name.clone()),
                os,
                created_at: Some(created_at),
                started_at: Some(started_at),
                completed_at,
                queue_secs: u32::try_from(queue).ok(),
                run_secs: u32::try_from(run).ok(),
                html_url: Some(format!("https://github.com/{repo}/actions/runs/{run_id}/job/{id}")),
                source: FleetJobSource::Demo,
            });
        }
    }
    jobs.sort_by_cached_key(FleetCompletedJob::cursor);
    jobs
}

/// When demo history "started": the full retention window.
pub fn history_since(now: DateTime<Utc>) -> DateTime<Utc> {
    now - JOB_RETENTION
}

/// Recent demo jobs for the runner and machine panes.
pub(crate) fn attach(view: &mut FleetView, now: DateTime<Utc>) {
    let jobs = history(view, now - Duration::days(2), now);
    view.activity.history_since = Some(history_since(now));
    view.activity.recent = store::recent(&jobs);
}
