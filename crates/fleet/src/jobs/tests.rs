use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, TimeZone};
use serde_json::json;
use tower::ServiceExt;

use super::analytics::{FleetAnalyticsWindow, summarize};
use super::history::{FleetJobHistoryFilter, FleetJobOrder};
use super::webhook::{FeedAction, parse_workflow_job, router, sign, verify_signature};
use super::*;
use crate::model::{DataSource, JobScan};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
}

fn demo_view(at: DateTime<Utc>) -> FleetView {
    let (config, obs) = crate::demo::dataset(at);
    crate::derive::derive(
        &config,
        &obs,
        &crate::demo_desired::plan_inputs(at),
        at,
        DataSource::Demo,
    )
}

fn demo_runners() -> Vec<GhRunner> {
    let (_, obs) = crate::demo::dataset(now());
    obs.github["acme-demo"].runners.as_ref().unwrap().value.clone()
}

fn gh_job(id: u64, runner_id: Option<u64>, labels: &[&str]) -> GhJob {
    GhJob {
        id,
        run_id: id * 10,
        repo: "acme-demo/web".into(),
        name: "build".into(),
        workflow_name: Some("CI".into()),
        status: "completed".into(),
        labels: labels.iter().map(ToString::to_string).collect(),
        runner_id,
        runner_name: runner_id.map(|_| "runner".into()),
        created_at: Some(now() - Duration::minutes(10)),
        started_at: Some(now() - Duration::minutes(8)),
        html_url: None,
        head_branch: Some("main".into()),
    }
}

fn finished(job: GhJob, conclusion: &str) -> FinishedJob {
    FinishedJob {
        job,
        conclusion: Some(conclusion.into()),
        completed_at: Some(now() - Duration::minutes(2)),
    }
}

/// A recorded job on `runner` that ran from `start` to `end` minutes before now.
fn completed(id: u64, runner: &str, machine: &str, start: i64, end: i64) -> FleetCompletedJob {
    FleetCompletedJob {
        id,
        run_id: id,
        org: "acme".into(),
        repo: "acme/web".into(),
        name: "build".into(),
        workflow_name: Some("CI".into()),
        branch: None,
        labels: vec!["self-hosted".into(), "macOS".into()],
        conclusion: FleetJobConclusion::Success,
        runner_key: Some(runner.into()),
        runner_name: Some(runner.into()),
        machine_key: Some(machine.into()),
        machine_name: Some(machine.into()),
        os: Some(FleetJobOs::Macos),
        created_at: Some(now() - Duration::minutes(start) - Duration::seconds(30)),
        started_at: Some(now() - Duration::minutes(start)),
        completed_at: now() - Duration::minutes(end),
        queue_secs: Some(30),
        run_secs: u32::try_from((start - end) * 60).ok(),
        html_url: None,
        source: FleetJobSource::Poll,
    }
}

#[test]
fn finished_self_hosted_jobs_are_attributed_to_their_runner_and_machine() {
    let view = demo_view(now());
    let record = FleetCompletedJob::from_finished(
        "Acme-Demo",
        &finished(gh_job(1, Some(101), &["self-hosted", "macOS"]), "timed_out"),
        &demo_runners(),
        &view,
        FleetJobSource::Poll,
    )
    .unwrap();
    assert_eq!(record.org, "acme-demo");
    assert_eq!(record.runner_key.as_deref(), Some("gh:acme-demo:101"));
    assert_eq!(record.runner_name.as_deref(), Some("studio-1"));
    assert_eq!(record.machine_key.as_deref(), Some("inv:studio"));
    assert_eq!(record.conclusion, FleetJobConclusion::TimedOut);
    assert_eq!(record.os, Some(FleetJobOs::Macos));
    assert_eq!((record.queue_secs, record.run_secs), (Some(120), Some(360)));

    let hosted = finished(gh_job(2, Some(9_999), &["ubuntu-latest"]), "success");
    let never_started = finished(gh_job(3, None, &["self-hosted"]), "cancelled");
    for job in [hosted, never_started] {
        assert_eq!(
            FleetCompletedJob::from_finished("acme-demo", &job, &demo_runners(), &view, FleetJobSource::Poll),
            None
        );
    }
}

#[test]
fn the_journal_dedupes_orders_bounds_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("job-history.json");
    let mut store = JobStore::load(path.clone(), now() - Duration::days(40));
    assert!(store.record(completed(2, "r", "m", 30, 20)));
    assert!(store.record(completed(1, "r", "m", 50, 40)));
    assert!(
        !store.record(completed(2, "r", "m", 30, 20)),
        "job IDs are recorded once"
    );
    assert!(store.record(completed(3, "r", "m", 31 * 24 * 60, 31 * 24 * 60 - 5)));
    store.prune(now());
    assert_eq!(store.jobs().iter().map(|j| j.id).collect::<Vec<_>>(), [1, 2]);
    store.save(now());
    let reloaded = JobStore::load(path, now());
    assert_eq!(reloaded.jobs(), store.jobs());
    assert_eq!(reloaded.history_since(), now() - Duration::days(40));
    assert_eq!(reloaded.storage_error(), None);
}

#[test]
fn the_journal_keeps_at_most_the_newest_max_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = JobStore::load(dir.path().join("job-history.json"), now());
    for id in 0..=u64::try_from(MAX_JOBS).unwrap() {
        let mut job = completed(id, "r", "m", 10, 5);
        job.completed_at = now() - Duration::seconds(i64::try_from(MAX_JOBS).unwrap() - i64::try_from(id).unwrap());
        store.record(job);
    }
    store.prune(now());
    assert_eq!(store.jobs().len(), MAX_JOBS);
    assert_eq!(store.jobs()[0].id, 1, "the oldest job goes first");
}

#[test]
fn unreadable_and_newer_journals_are_preserved_with_a_visible_error() {
    for (contents, needle) in [
        ("{not json", "Couldn't read job history"),
        (
            r#"{"version": 9, "started_at": "2026-10-01T00:00:00Z", "jobs": []}"#,
            "version 9",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("job-history.json");
        std::fs::write(&path, contents).unwrap();
        let mut store = JobStore::load(path.clone(), now());
        assert!(
            store.storage_error().unwrap().contains(needle),
            "{:?}",
            store.storage_error()
        );
        assert!(
            store.record(completed(1, "r", "m", 10, 5)),
            "jobs are still kept for this session"
        );
        store.save(now());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        assert!(store.activity().storage_error.is_some());
    }
}

#[test]
fn runs_are_followed_until_they_leave_the_scan_and_finish() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = JobStore::load(dir.path().join("job-history.json"), now());
    let mut running = gh_job(5, Some(101), &["self-hosted", "macOS"]);
    running.status = "in_progress".into();
    let hosted = gh_job(6, None, &["ubuntu-latest"]);
    let scan = JobScan {
        repos_scanned: vec!["web".into()],
        truncated: false,
        jobs: vec![running.clone(), hosted],
    };
    store.track("Acme", &scan.jobs, now());
    assert!(store.due("acme", &scan, now(), 10).is_empty(), "still in the scan");
    let empty = JobScan::default();
    let due = store.due("acme", &empty, now(), 10);
    assert_eq!(
        due.iter().map(|r| r.run_id).collect::<Vec<_>>(),
        [50],
        "hosted runs aren't followed"
    );
    assert_eq!(due[0].jobs.iter().copied().collect::<Vec<_>>(), [5]);
    store.checked(50, now());
    assert!(store.due("acme", &empty, now() + Duration::minutes(1), 10).is_empty());
    assert_eq!(store.due("acme", &empty, now() + Duration::minutes(2), 10).len(), 1);
    store.finish(50);
    assert!(store.due("acme", &empty, now() + Duration::hours(1), 10).is_empty());
}

#[test]
fn recent_jobs_are_bounded_per_runner_and_machine() {
    let jobs: Vec<_> = (0..30)
        .map(|i| {
            completed(
                i,
                if i % 2 == 0 { "a" } else { "b" },
                "m",
                300 - i64::try_from(i).unwrap(),
                299 - i64::try_from(i).unwrap(),
            )
        })
        .collect();
    let recent = store::recent(&jobs);
    assert_eq!(recent.len(), 20, "10 per runner covers the machine's 10 too");
    assert_eq!(recent[0].id, 29, "newest first");
    assert!(recent.iter().all(|j| j.id >= 10));
}

#[test]
fn history_pages_by_cursor_in_both_directions_with_filters() {
    let mut jobs: Vec<_> = (1..=7)
        .map(|i| {
            completed(
                i,
                "r",
                "m",
                100 - i64::try_from(i).unwrap() * 10,
                95 - i64::try_from(i).unwrap() * 10,
            )
        })
        .collect();
    jobs[3].conclusion = FleetJobConclusion::Failure;
    jobs[3].repo = "acme/api".into();
    let mut filter = FleetJobHistoryFilter {
        limit: Some(3),
        ..Default::default()
    };
    let mut seen = Vec::new();
    loop {
        let (page, order, more, next) = filter.page(&jobs);
        assert_eq!(order, FleetJobOrder::NewestFirst);
        seen.extend(page.iter().map(|j| j.id));
        if !more {
            assert_eq!(next, None);
            break;
        }
        assert!(history::valid_cursor(next.as_deref().unwrap()));
        filter.before = next;
    }
    assert_eq!(seen, [7, 6, 5, 4, 3, 2, 1]);

    let after = FleetJobHistoryFilter {
        after: Some(jobs[4].cursor()),
        ..Default::default()
    };
    let (page, order, _, _) = after.page(&jobs);
    assert_eq!(order, FleetJobOrder::OldestFirst);
    assert_eq!(page.iter().map(|j| j.id).collect::<Vec<_>>(), [6, 7]);

    for filter in [
        FleetJobHistoryFilter {
            repo: Some("API".into()),
            ..Default::default()
        },
        FleetJobHistoryFilter {
            conclusion: Some(FleetJobConclusion::Failure),
            ..Default::default()
        },
    ] {
        assert_eq!(filter.page(&jobs).0.iter().map(|j| j.id).collect::<Vec<_>>(), [4]);
    }
}

#[test]
fn analytics_count_busy_time_inside_the_recorded_part_of_the_window() {
    let view = demo_view(now());
    let mut jobs = vec![
        // 60 minutes busy, all inside the window.
        completed(1, "gh:acme-demo:101", "inv:studio", 180, 120),
        // Started before the window: only the last 30 minutes count.
        completed(2, "gh:acme-demo:101", "inv:studio", 24 * 60 + 30, 24 * 60 - 30),
        completed(3, "gh:acme-demo:102", "inv:studio", 61, 60),
    ];
    jobs[2].conclusion = FleetJobConclusion::Failure;
    jobs[2].queue_secs = Some(600);
    jobs[2].labels = vec!["self-hosted".into(), "macOS".into(), "xcode-16".into()];
    let rates = FleetHostedRates::default();
    let day = summarize(
        &jobs,
        Some(now() - Duration::days(3)),
        &view,
        &rates,
        FleetAnalyticsWindow::Day,
        now(),
    );
    let runner = day
        .runners
        .iter()
        .find(|r| r.runner_key.as_deref() == Some("gh:acme-demo:101"))
        .unwrap();
    assert_eq!(runner.busy_secs, 90 * 60);
    assert!((runner.busy_percent.unwrap() - 6.25).abs() < 1e-9);
    assert_eq!(runner.totals.jobs, 2);
    let studio = day.machines.iter().find(|m| m.machine_key == "inv:studio").unwrap();
    let registered = view
        .machines
        .iter()
        .find(|m| m.display_name == "studio")
        .map(|m| m.runners.iter().filter(|r| r.github.is_some()).count())
        .unwrap();
    assert_eq!(
        (studio.runners, studio.busy_secs),
        (u32::try_from(registered).unwrap(), 91 * 60)
    );
    assert_eq!(day.totals.jobs, 3);
    assert_eq!(day.totals.success_rate, Some(2.0 / 3.0));
    assert_eq!(day.totals.queue_p50_secs, Some(30));
    assert_eq!(day.totals.queue_p95_secs, Some(600));
    assert_eq!(
        day.label_sets[0].labels,
        ["macOS", "self-hosted", "xcode-16"],
        "slowest median first"
    );
    assert_eq!(day.buckets.len(), 24);
    assert_eq!(day.buckets.iter().map(|b| b.busy_secs).sum::<u32>(), 91 * 60);
    // 60 + 60 + 1 billable minutes, at the macOS rate.
    assert_eq!(day.hosted_cost.minutes, 121);
    assert!((day.hosted_cost.usd - 121.0 * rates.macos).abs() < 1e-9);
    assert_eq!(day.repos[0].name, "acme/web");

    // History started two hours ago: busy percentages divide by those hours only.
    let recent = summarize(
        &jobs[2..],
        Some(now() - Duration::hours(2)),
        &view,
        &rates,
        FleetAnalyticsWindow::Week,
        now(),
    );
    assert_eq!(recent.observed_from, Some(now() - Duration::hours(2)));
    let runner = recent
        .runners
        .iter()
        .find(|r| r.runner_key.as_deref() == Some("gh:acme-demo:102"))
        .unwrap();
    assert!((runner.busy_percent.unwrap() - 100.0 / 120.0).abs() < 1e-9);
    assert_eq!(recent.buckets.len(), 28);
}

#[test]
fn demo_history_is_deterministic_bounded_and_never_overlaps_on_a_runner() {
    let view = demo_view(now());
    let since = now() - Duration::days(30);
    let jobs = demo::history(&view, since, now());
    assert_eq!(jobs, demo::history(&view, since, now()));
    assert!(jobs.len() > 1_000, "{}", jobs.len());
    assert!(jobs.windows(2).all(|w| w[0].cursor() < w[1].cursor()));
    assert!(jobs.iter().all(|j| j.completed_at > since && j.completed_at <= now()));
    let mut ids: Vec<_> = jobs.iter().map(|j| j.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), jobs.len());
    let mut by_runner: std::collections::BTreeMap<_, Vec<_>> = std::collections::BTreeMap::new();
    for job in &jobs {
        by_runner.entry(job.runner_key.clone()).or_default().push(job);
    }
    for runs in by_runner.values_mut() {
        runs.sort_by_key(|j| j.started_at);
        assert!(runs.windows(2).all(|w| w[0].completed_at <= w[1].started_at.unwrap()));
    }
    assert!(jobs.iter().any(|j| j.machine_key.as_deref() == Some("inv:studio")));
    assert!(!view.activity.recent.is_empty(), "demo views carry recent jobs");
    let later = demo::history(&view, since, now() + Duration::hours(1));
    assert!(
        later.iter().any(|j| j.id == jobs.last().unwrap().id),
        "jobs keep their identity as time passes"
    );
}

fn delivery(action: &str, conclusion: Option<&str>) -> Vec<u8> {
    json!({
        "action": action,
        "workflow_job": {
            "id": 42, "run_id": 420, "name": "build", "workflow_name": "CI", "status": if action == "completed" { "completed" } else { action },
            "conclusion": conclusion, "labels": ["self-hosted", "Linux"], "runner_id": 301, "runner_name": "linux-01-a",
            "created_at": "2026-10-08T11:50:00Z", "started_at": "2026-10-08T11:51:00Z",
            "completed_at": conclusion.map(|_| "2026-10-08T11:55:00Z"), "html_url": "https://github.com/acme-demo/web/actions/runs/420/job/42",
            "head_branch": "main"
        },
        "repository": { "full_name": "acme-demo/web", "owner": { "login": "Acme-Demo" } },
        "organization": { "login": "Acme-Demo" }
    })
    .to_string()
    .into_bytes()
}

#[test]
fn workflow_job_deliveries_parse_into_finished_jobs() {
    let parsed = parse_workflow_job(&delivery("completed", Some("success"))).unwrap();
    assert_eq!(parsed.org, "acme-demo");
    assert_eq!(parsed.action, FeedAction::Completed);
    assert_eq!(parsed.job.job.repo, "acme-demo/web");
    assert_eq!(parsed.job.job.runner_id, Some(301));
    assert_eq!(parsed.job.conclusion.as_deref(), Some("success"));
    assert_eq!(parsed.job.completed_at, "2026-10-08T11:55:00Z".parse().ok());
    assert_eq!(
        parse_workflow_job(&delivery("in_progress", None)).unwrap().action,
        FeedAction::InProgress
    );
    assert!(parse_workflow_job(b"{}").is_err());
}

#[test]
fn signatures_must_match_the_secret() {
    let body = b"payload";
    assert!(verify_signature(b"secret", body, Some(&sign("secret", body))));
    assert!(!verify_signature(b"other", body, Some(&sign("secret", body))));
    assert!(!verify_signature(b"secret", body, None));
    assert!(!verify_signature(b"secret", body, Some("sha256=zz")));
    assert!(!verify_signature(b"secret", body, Some("sha1=00")));
    let secret = webhook::new_secret().unwrap();
    assert_eq!(secret.len(), 64);
    assert_ne!(secret, webhook::new_secret().unwrap());
}

#[tokio::test]
async fn the_receiver_accepts_only_signed_local_deliveries() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink: webhook::DeliverySink = {
        let received = received.clone();
        Arc::new(move |d| {
            received.lock().unwrap().push(d);
            Box::pin(async {})
        })
    };
    let app = router("s3cret", sink);
    let body = delivery("completed", Some("failure"));
    let request = |event: &str, signature: Option<String>, origin: bool| {
        let mut builder = Request::post(webhook::RECEIVER_PATH).header("x-github-event", event);
        if let Some(signature) = signature {
            builder = builder.header("x-hub-signature-256", signature);
        }
        if origin {
            builder = builder.header("origin", "https://evil.example");
        }
        builder.body(Body::from(body.clone())).unwrap()
    };
    let status = |r: Request<Body>| {
        let app = app.clone();
        async move { app.oneshot(r).await.unwrap().status() }
    };
    assert_eq!(
        status(request("workflow_job", None, false)).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(request("workflow_job", Some(sign("wrong", &body)), false)).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(request("workflow_job", Some(sign("s3cret", &body)), true)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(request("ping", Some(sign("s3cret", &body)), false)).await,
        StatusCode::OK
    );
    assert_eq!(
        status(request("push", Some(sign("s3cret", &body)), false)).await,
        StatusCode::ACCEPTED
    );
    assert!(received.lock().unwrap().is_empty());
    assert_eq!(
        status(request("workflow_job", Some(sign("s3cret", &body)), false)).await,
        StatusCode::OK
    );
    let received = received.lock().unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].job.conclusion.as_deref(), Some("failure"));
}
