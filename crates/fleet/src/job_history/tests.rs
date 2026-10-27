use chrono::{DateTime, Duration, TimeZone, Utc};

use super::store::{MAX_JOBS, RETENTION, Span};
use super::summary::{activity, percentile, recorded_overlap};
use super::*;
use crate::model::{DataSource, JobScan};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap()
}

fn runner(id: u64, name: &str, labels: &[&str]) -> GhRunner {
    GhRunner {
        id,
        name: name.into(),
        os: "macOS".into(),
        status: "online".into(),
        busy: false,
        labels: labels.iter().map(ToString::to_string).collect(),
        ephemeral: None,
        version: None,
        runner_group_id: None,
    }
}

fn job(id: u64, run_id: u64, status: &str, labels: &[&str], runner: Option<u64>) -> GhJob {
    let done = status == "completed";
    GhJob {
        id,
        run_id,
        repo: "acme/web".into(),
        name: format!("job-{id}"),
        workflow_name: Some("CI".into()),
        status: status.into(),
        labels: labels.iter().map(ToString::to_string).collect(),
        runner_id: runner,
        runner_name: runner.map(|r| format!("r{r}")),
        created_at: Some(t0()),
        started_at: Some(t0() + Duration::minutes(1)),
        completed_at: done.then(|| t0() + Duration::minutes(5)),
        conclusion: done.then(|| "success".into()),
        html_url: None,
        head_branch: None,
    }
}

fn scan(jobs: Vec<GhJob>) -> JobScan {
    JobScan {
        repos_scanned: vec!["web".into()],
        truncated: false,
        jobs,
    }
}

fn history() -> (tempfile::TempDir, JobHistory) {
    let dir = tempfile::tempdir().unwrap();
    let history = JobHistory::load(dir.path().join("job-history.json"));
    (dir, history)
}

fn ids(history: &JobHistory) -> Vec<u64> {
    history.jobs().iter().map(|j| j.id).collect()
}

#[test]
fn self_hosted_by_label_runner_or_compatible_labels() {
    let runners = [runner(7, "r7", &["self-hosted", "macOS", "ARM64"])];
    assert!(is_self_hosted(
        &job(1, 1, "queued", &["self-hosted", "gpu"], None),
        &runners
    ));
    assert!(is_self_hosted(
        &job(2, 1, "completed", &["anything"], Some(7)),
        &runners
    ));
    assert!(is_self_hosted(
        &job(3, 1, "queued", &["macos", "arm64"], None),
        &runners
    ));
    assert!(!is_self_hosted(
        &job(4, 1, "queued", &["ubuntu-latest"], None),
        &runners
    ));
    assert!(!is_self_hosted(
        &job(5, 1, "completed", &["ubuntu-latest"], Some(99)),
        &runners
    ));
}

#[test]
fn records_finished_runs_once_and_stops_reading_them() {
    let (_dir, mut h) = history();
    let runners = [runner(7, "r7", &["self-hosted", "macOS"])];
    let now = t0();
    let in_flight = scan(vec![
        job(11, 1, "in_progress", &["self-hosted"], Some(7)),
        job(21, 2, "queued", &["ubuntu-latest"], None),
        job(31, 3, "in_progress", &["self-hosted"], Some(7)),
    ]);
    h.track("Acme", &in_flight, &runners, now);
    assert_eq!(
        h.due("acme", &in_flight, now, 10),
        vec![],
        "runs in the scan aren't read"
    );

    let later = now + Duration::minutes(1);
    let empty = scan(Vec::new());
    h.track("acme", &empty, &runners, later);
    assert_eq!(
        h.due("acme", &empty, later, 10),
        vec![(1, "acme/web".into()), (3, "acme/web".into())],
        "only self-hosted runs are followed"
    );
    let finished = [
        job(11, 1, "completed", &["self-hosted"], Some(7)),
        job(12, 1, "completed", &["ubuntu-latest"], Some(99)),
    ];
    h.read_run(1, &finished, false, &runners, later);
    h.read_run(
        3,
        &[job(31, 3, "in_progress", &["self-hosted"], Some(7))],
        false,
        &runners,
        later,
    );
    assert_eq!(ids(&h), vec![11]);
    assert_eq!(h.jobs()[0].org, "acme");
    assert_eq!(h.jobs()[0].conclusion, FleetJobConclusion::Success);
    assert_eq!(h.jobs()[0].os, Some(FleetJobOs::Macos));
    assert_eq!(h.due("acme", &empty, later, 10), vec![], "run 1 is done; run 3 waits");
    assert_eq!(
        h.due("acme", &empty, later + Duration::minutes(5), 10),
        vec![(3, "acme/web".into())]
    );

    // A re-run brings run 1 back with the same and a new job; only the new one is added.
    h.track(
        "acme",
        &scan(vec![job(13, 1, "in_progress", &["self-hosted"], Some(7))]),
        &runners,
        later,
    );
    h.read_run(
        1,
        &[finished[0].clone(), job(13, 1, "completed", &["self-hosted"], Some(7))],
        false,
        &runners,
        later,
    );
    assert_eq!(ids(&h), vec![11, 13]);

    // A truncated listing could hide unfinished jobs, so the run stays followed.
    h.track(
        "acme",
        &scan(vec![job(41, 4, "in_progress", &["self-hosted"], Some(7))]),
        &runners,
        later,
    );
    h.read_run(
        4,
        &[job(41, 4, "completed", &["self-hosted"], Some(7))],
        true,
        &runners,
        later,
    );
    assert_eq!(ids(&h), vec![11, 13, 41]);
    assert_eq!(
        h.due("acme", &empty, later + Duration::minutes(5), 10),
        vec![(3, "acme/web".into()), (4, "acme/web".into())]
    );
}

#[test]
fn bounds_jobs_by_age_and_count_and_persists() {
    let (dir, mut h) = history();
    let runners = [runner(7, "r7", &["self-hosted"])];
    let now = t0();
    let done_at = now + Duration::minutes(5);
    let jobs: Vec<GhJob> = (0..=u64::try_from(MAX_JOBS).unwrap())
        .map(|i| job(10 + i, 2, "completed", &["self-hosted"], Some(7)))
        .collect();
    h.track(
        "acme",
        &scan(vec![job(1, 1, "queued", &["self-hosted"], None)]),
        &runners,
        now,
    );
    h.track(
        "acme",
        &scan(vec![job(10, 2, "queued", &["self-hosted"], None)]),
        &runners,
        now,
    );
    h.read_run(
        1,
        &[job(1, 1, "completed", &["self-hosted"], Some(7))],
        false,
        &runners,
        now,
    );
    h.read_run(2, &jobs, false, &runners, now);
    h.save_due(done_at + RETENTION - Duration::seconds(1));
    assert_eq!(h.jobs().len(), MAX_JOBS, "the oldest jobs beyond the cap go");
    assert_eq!(h.jobs()[0].id, 11);
    h.save_due(done_at + RETENTION + Duration::seconds(1));
    assert!(h.jobs().is_empty(), "jobs older than 30 days go");

    h.cover(now);
    h.cover(now + Duration::minutes(2));
    h.cover(now + Duration::minutes(10));
    h.save_due(now + Duration::minutes(10));
    let reloaded = JobHistory::load(dir.path().join("job-history.json"));
    let spans = [
        Span {
            from: now,
            to: now + Duration::minutes(2),
        },
        Span {
            from: now + Duration::minutes(10),
            to: now + Duration::minutes(10),
        },
    ];
    assert_eq!(reloaded.recorded(), spans, "a gap starts a new span");
}

#[test]
fn unreadable_file_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("job-history.json");
    std::fs::write(&path, br#"{"version": 2}"#).unwrap();
    let mut h = JobHistory::load(path.clone());
    h.cover(t0());
    h.save_due(t0());
    assert!(
        h.storage_error()
            .unwrap()
            .contains("Unsupported job history version 2.")
    );
    assert_eq!(std::fs::read(&path).unwrap(), br#"{"version": 2}"#);
}

#[test]
fn percentiles_use_nearest_rank() {
    let waits: Vec<u32> = (1..=20).collect();
    assert_eq!((percentile(&waits, 50), percentile(&waits, 95)), (Some(10), Some(19)));
    assert_eq!((percentile(&[7], 50), percentile(&[7], 95)), (Some(7), Some(7)));
    assert_eq!(percentile(&[], 50), None);
}

#[test]
fn busy_percent_counts_recorded_time_and_listed_runners_only() {
    let now = t0();
    let (config, obs) = crate::demo::dataset(now);
    let view = crate::derive::derive(
        &config,
        &obs,
        &crate::demo_desired::plan_inputs(now),
        now,
        DataSource::Live,
    );
    let mini = view
        .machines
        .iter()
        .find(|m| m.runners.iter().any(|r| r.name == "mini-02"))
        .unwrap();
    let listed = mini.runners.iter().filter(|r| r.github.is_some()).count();
    assert_eq!(listed, 1, "the demo's mac-mini-02 has one runner GitHub lists");

    // Recorded for an hour, closed for an hour, recorded for an hour.
    let start = now - Duration::hours(3);
    let spans = [
        Span {
            from: start,
            to: start + Duration::hours(1),
        },
        Span {
            from: start + Duration::hours(2),
            to: now,
        },
    ];
    // Two hours of work across the gap; one of them while the app was closed.
    let worked = FleetFinishedJob {
        id: 1,
        run_id: 1,
        org: "acme-demo".into(),
        repo: "acme-demo/web".into(),
        name: "build".into(),
        workflow_name: None,
        labels: vec!["self-hosted".into()],
        conclusion: FleetJobConclusion::Success,
        runner_name: Some("MINI-02".into()),
        os: Some(FleetJobOs::Macos),
        created_at: Some(start),
        started_at: Some(start + Duration::minutes(30)),
        completed_at: start + Duration::minutes(150) + Duration::seconds(1),
        html_url: None,
    };
    assert_eq!(recorded_overlap(&spans, start, now), 7200);
    let summary = activity(&[worked], &spans, &view, now + Duration::hours(5));
    assert_eq!(summary.as_of, Some(now), "windows end at the latest recorded moment");
    let day = &summary.windows[0];
    assert_eq!(
        (day.days, day.jobs, day.recorded_secs, day.runner_secs),
        (1, 1, 7200, 7201)
    );
    let busy = day.machines.iter().find(|m| m.machine_id == mini.id).unwrap();
    assert_eq!((busy.runners, busy.busy_secs), (1, 3601));
    assert!((busy.busy_percent.unwrap() - 50.0139).abs() < 0.001, "{busy:?}");
    assert_eq!((day.queue_median_secs, day.queue_p95_secs), (Some(1800), Some(1800)));
    let mac = day.hosted.iter().find(|c| c.os == FleetJobOs::Macos).unwrap();
    assert_eq!(mac.minutes, 121, "each job rounds up to a whole minute");
    assert!((mac.usd - 121.0 * 0.062).abs() < 1e-9);
    assert!(day.machines.iter().all(|m| m.runners > 0));
}
