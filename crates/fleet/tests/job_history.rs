//! Completed-job history and the real-time job feed, against a fake GitHub and a fake
//! forwarder. No test starts `gh webhook forward` or contacts GitHub.
#![expect(clippy::unwrap_used, reason = "test fixtures fail the test loudly")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use fleet::engine::{Engine, MemorySecrets, SecretStore};
use fleet::jobs::feed::{Backoff, FleetJobFeedState, ForwardProcess, ForwardSpec, Launch, ProcessEvent};
use fleet::jobs::history::FleetJobHistoryFilter;
use fleet::jobs::{FleetJobConclusion, FleetJobSource};
use fleet::model::{AccountKind, AppConfig, OrgAuth, OrgConfig, RepoScope};
use serde_json::{Value, json};

mod common;
use common::serve;

fn runner_job(id: u64, status: &str, conclusion: Option<&str>) -> Value {
    json!({"id": id, "run_id": 11, "name": format!("job-{id}"), "workflow_name": "CI", "status": status,
           "conclusion": conclusion, "labels": ["self-hosted", "Linux"], "runner_id": 7, "runner_name": "fake-runner",
           "created_at": "2026-10-08T11:50:00Z", "started_at": "2026-10-08T11:51:00Z",
           "completed_at": conclusion.map(|_| "2026-10-08T11:56:00Z"), "head_branch": "main"})
}

/// A fake GitHub with one runner and one run that finishes when `finished` is set.
fn fake_github(finished: Arc<AtomicBool>, run_lists: Arc<AtomicUsize>) -> Router {
    fake_github_with(finished, run_lists, Budget::default())
}

/// Rate-limit headers the fake reports: `remaining` on run lists, and a refused (rate
/// limited) run job listing while `limited` is set.
#[derive(Clone)]
struct Budget {
    remaining: Arc<AtomicU32>,
    limited: Arc<AtomicBool>,
    runner_lists: Arc<AtomicUsize>,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            remaining: Arc::new(AtomicU32::new(5000)),
            limited: Arc::default(),
            runner_lists: Arc::default(),
        }
    }
}

fn rate_headers(remaining: u32) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-ratelimit-limit", "5000".parse().unwrap());
    headers.insert("x-ratelimit-remaining", remaining.to_string().parse().unwrap());
    let reset = (Utc::now() + chrono::Duration::minutes(30)).timestamp();
    headers.insert("x-ratelimit-reset", reset.to_string().parse().unwrap());
    headers
}

fn fake_github_with(finished: Arc<AtomicBool>, run_lists: Arc<AtomicUsize>, budget: Budget) -> Router {
    let runs_finished = finished.clone();
    let runners = budget.runner_lists.clone();
    let remaining = budget.remaining.clone();
    Router::new()
        .route(
            "/orgs/{org}/actions/runners",
            get(move || {
                runners.fetch_add(1, Ordering::SeqCst);
                async {
                    Json(
                        json!({"total_count": 1, "runners": [{"id": 7, "name": "fake-runner", "os": "Linux",
                        "status": "online", "busy": true, "labels": [{"name": "self-hosted"}, {"name": "Linux"}]}]}),
                    )
                }
            }),
        )
        .route(
            "/repos/{org}/{repo}/actions/runs",
            get(move |Query(q): Query<HashMap<String, String>>| {
                let finished = runs_finished.clone();
                let left = remaining.load(Ordering::SeqCst);
                run_lists.fetch_add(1, Ordering::SeqCst);
                async move {
                    let running = q["status"] == "in_progress" && !finished.load(Ordering::SeqCst);
                    let runs = if running { json!([{"id": 11}]) } else { json!([]) };
                    (
                        rate_headers(left),
                        Json(json!({"total_count": runs.as_array().unwrap().len(), "workflow_runs": runs})),
                    )
                }
            }),
        )
        .route(
            "/repos/{org}/{repo}/actions/runs/{run}/jobs",
            get(move || {
                let finished = finished.load(Ordering::SeqCst);
                let limited = budget.limited.load(Ordering::SeqCst);
                async move {
                    if limited {
                        let body = Json(json!({"message": "API rate limit exceeded"}));
                        return (StatusCode::FORBIDDEN, rate_headers(0), body);
                    }
                    let jobs = if finished {
                        // 112 finished between two scans and was never seen running.
                        json!([
                            runner_job(111, "completed", Some("failure")),
                            runner_job(112, "completed", Some("success"))
                        ])
                    } else {
                        json!([runner_job(111, "in_progress", None)])
                    };
                    let body = Json(json!({"total_count": jobs.as_array().unwrap().len(), "jobs": jobs}));
                    (StatusCode::OK, rate_headers(5000), body)
                }
            }),
        )
}

fn engine(dir: &std::path::Path, base: String) -> Arc<Engine> {
    let config = AppConfig {
        orgs: vec![OrgConfig {
            login: "acme".into(),
            api_base: base,
            repo_scope: RepoScope::Selected {
                repos: vec!["web".into()],
            },
            added_at: Utc::now(),
            auth: OrgAuth::Token,
            kind: AccountKind::Organization,
            runner_repos: Vec::new(),
        }],
        ..Default::default()
    };
    let path = dir.join("config.json");
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    secrets.set("acme", "fake").unwrap();
    let engine = Engine::load(path, secrets).unwrap();
    engine
        .set_inventory_path(Some(dir.join("missing.toml").display().to_string()))
        .unwrap();
    Arc::new(engine)
}

fn history(engine: &Engine) -> Vec<fleet::jobs::FleetCompletedJob> {
    engine
        .job_history(&engine.view(), &FleetJobHistoryFilter::default())
        .jobs
}

#[tokio::test]
async fn runs_that_leave_the_scan_are_read_once_and_recorded_with_their_runner() {
    let finished = Arc::new(AtomicBool::new(false));
    let base = serve(fake_github(finished.clone(), Arc::default())).await;
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), base);
    engine.poll_github_runners().await;
    engine.poll_github_jobs_and_history().await;
    assert_eq!(engine.view().queue.jobs.len(), 0);
    assert!(history(&engine).is_empty(), "the job is still running");

    finished.store(true, Ordering::SeqCst);
    engine.poll_github_jobs_and_history().await;
    let jobs = history(&engine);
    assert_eq!(jobs.iter().map(|j| j.id).collect::<Vec<_>>(), [112, 111]);
    let failed = &jobs[1];
    assert_eq!(failed.conclusion, FleetJobConclusion::Failure);
    assert_eq!(failed.runner_key.as_deref(), Some("gh:acme:7"));
    assert_eq!(failed.repo, "acme/web");
    assert_eq!((failed.queue_secs, failed.run_secs), (Some(60), Some(300)));
    assert_eq!(failed.source, FleetJobSource::Poll);
    assert_eq!(engine.view().activity.recent.len(), 2);
    let read =
        || -> Value { serde_json::from_slice(&std::fs::read(dir.path().join("job-history.json")).unwrap()).unwrap() };
    // Saves are debounced: the first round saved the followed run moments ago.
    assert_eq!(read()["jobs"], json!([]));
    engine.flush_job_history();
    let saved = read();
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["jobs"].as_array().unwrap().len(), 2);
    assert_eq!(saved["pending"], json!({}), "the finished run is no longer followed");

    engine.poll_github_jobs_and_history().await;
    assert_eq!(history(&engine).len(), 2, "jobs are recorded once");
}

/// Prints the forwarder's ready line, then runs until terminated.
#[derive(Default)]
struct FakeLauncher {
    specs: Mutex<Vec<ForwardSpec>>,
    terminated: Arc<AtomicUsize>,
}

struct FakeProcess {
    ready: bool,
    terminated: Arc<AtomicUsize>,
}

impl ForwardProcess for FakeProcess {
    async fn next_event(&mut self) -> ProcessEvent {
        if !std::mem::replace(&mut self.ready, true) {
            return ProcessEvent::Line("Forwarding Webhook events from GitHub...".into());
        }
        std::future::pending().await
    }
    fn terminate(&mut self) {
        self.terminated.fetch_add(1, Ordering::SeqCst);
    }
    fn group(&self) -> Option<u32> {
        None
    }
}

impl Launch for FakeLauncher {
    type Process = FakeProcess;
    fn launch(&self, spec: &ForwardSpec) -> std::io::Result<FakeProcess> {
        self.specs.lock().unwrap().push(spec.clone());
        Ok(FakeProcess {
            ready: false,
            terminated: self.terminated.clone(),
        })
    }
}

fn signature(secret: &str, body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let tag = ring::hmac::sign(&key, body);
    format!(
        "sha256={}",
        tag.as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

async fn eventually(what: &str, check: impl Fn() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn the_job_feed_records_deliveries_slows_polling_and_stops_its_forwarders() {
    let run_lists = Arc::new(AtomicUsize::new(0));
    let base = serve(fake_github(Arc::default(), run_lists.clone())).await;
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), base);
    engine.poll_github_runners().await;
    engine.poll_github_jobs().await;
    assert!(engine.set_job_feed("nope", true).is_err());
    engine.set_job_feed("ACME", true).unwrap();
    assert_eq!(engine.config().job_activity.feed_orgs, ["acme"]);

    let launcher = Arc::new(FakeLauncher::default());
    let feeds = tokio::spawn(engine.clone().run_job_feeds_with(
        launcher.clone(),
        || Some("/fake/gh".into()),
        Backoff::default(),
    ));
    eventually("forwarding", || {
        engine
            .view()
            .activity
            .feeds
            .first()
            .is_some_and(|f| f.state == FleetJobFeedState::Forwarding)
    })
    .await;
    let spec = launcher.specs.lock().unwrap()[0].clone();
    assert_eq!(spec.org, "acme");
    assert!(spec.url.starts_with("http://127.0.0.1:"), "{}", spec.url);
    assert!(spec.args().contains(&"--events=workflow_job".to_string()));

    let body = json!({
        "action": "completed",
        "workflow_job": runner_job(333, "completed", Some("success")),
        "repository": {"full_name": "acme/web", "owner": {"login": "acme"}},
        "organization": {"login": "acme"}
    })
    .to_string();
    let post = |signature: String| {
        reqwest::Client::new()
            .post(&spec.url)
            .header("x-github-event", "workflow_job")
            .header("x-hub-signature-256", signature)
            .body(body.clone())
            .send()
    };
    assert_eq!(post(signature("wrong", body.as_bytes())).await.unwrap().status(), 401);
    assert!(history(&engine).is_empty());
    assert_eq!(
        post(signature(&spec.secret, body.as_bytes())).await.unwrap().status(),
        200
    );
    let jobs = history(&engine);
    assert_eq!(jobs.len(), 1);
    assert_eq!((jobs[0].id, jobs[0].source), (333, FleetJobSource::Webhook));
    assert!(engine.view().activity.feeds[0].last_delivery_at.is_some());

    // A forwarding feed and a recent scan cover the scheduled job poll.
    let before = run_lists.load(Ordering::SeqCst);
    engine.poll_github_jobs_and_history().await;
    assert_eq!(run_lists.load(Ordering::SeqCst), before);

    // A late delivery can't revive a completed job, and other repositories stay out of the scan.
    for (id, repo) in [(333, "acme/web"), (555, "acme/other"), (444, "acme/web")] {
        let body = json!({
            "action": "queued",
            "workflow_job": runner_job(id, "queued", None),
            "repository": {"full_name": repo, "owner": {"login": "acme"}},
            "organization": {"login": "acme"}
        })
        .to_string();
        let response = reqwest::Client::new()
            .post(&spec.url)
            .header("x-github-event", "workflow_job")
            .header("x-hub-signature-256", signature(&spec.secret, body.as_bytes()))
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
    let queued: Vec<u64> = engine.view().queue.jobs.iter().map(|q| q.job.id).collect();
    assert_eq!(queued, [444]);

    engine.stop_job_feeds();
    eventually("forwarder terminated", || {
        launcher.terminated.load(Ordering::SeqCst) == 1
    })
    .await;
    tokio::time::timeout(Duration::from_secs(10), feeds)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn feed_settings_and_rates_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), "http://127.0.0.1:9".into());
    let mut rates = engine.config().job_activity.hosted_rates;
    rates.linux = -1.0;
    assert!(engine.set_hosted_rates(rates.clone()).is_err());
    rates.linux = 0.008;
    rates.as_of = " 2026-11-01 ".into();
    engine.set_hosted_rates(rates).unwrap();
    let saved = engine.config().job_activity.hosted_rates;
    assert_eq!((saved.linux, saved.as_of.as_str()), (0.008, "2026-11-01"));
    engine.set_job_feed("acme", true).unwrap();
    engine.set_job_feed("acme", false).unwrap();
    assert!(engine.config().job_activity.feed_orgs.is_empty());
    // Removing an account forgets its opt-in, so adding it again starts without a forwarder.
    engine.set_job_feed("acme", true).unwrap();
    engine.remove_org("acme").await.unwrap();
    assert!(engine.config().job_activity.feed_orgs.is_empty());
}

#[tokio::test]
async fn history_reads_leave_a_low_budget_to_the_polls_and_honour_rate_limits() {
    let finished = Arc::new(AtomicBool::new(false));
    let budget = Budget::default();
    let base = serve(fake_github_with(finished.clone(), Arc::default(), budget.clone())).await;
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), base);
    engine.poll_github_runners().await;
    engine.poll_github_jobs_and_history().await;

    // The scan reports a nearly spent budget: the finished run waits for a later round.
    finished.store(true, Ordering::SeqCst);
    budget.remaining.store(150, Ordering::SeqCst);
    engine.poll_github_jobs_and_history().await;
    assert!(history(&engine).is_empty());

    // A rate-limited history read makes every poll of the account wait for the reset.
    budget.remaining.store(5000, Ordering::SeqCst);
    budget.limited.store(true, Ordering::SeqCst);
    engine.poll_github_jobs_and_history().await;
    assert!(history(&engine).is_empty());
    let before = budget.runner_lists.load(Ordering::SeqCst);
    engine.poll_github_runners().await;
    assert_eq!(
        budget.runner_lists.load(Ordering::SeqCst),
        before,
        "runner polls wait for the reset"
    );
}

#[tokio::test]
async fn earlier_attempt_jobs_are_read_once_even_when_they_cant_be_recorded() {
    let finished = Arc::new(AtomicBool::new(false));
    let single_reads = Arc::new(AtomicUsize::new(0));
    let (runs_done, jobs_done, reads) = (finished.clone(), finished.clone(), single_reads.clone());
    let router = Router::new()
        .route(
            "/orgs/{org}/actions/runners",
            get(|| async {
                Json(
                    json!({"total_count": 1, "runners": [{"id": 7, "name": "fake-runner", "os": "Linux",
                    "status": "online", "busy": true, "labels": [{"name": "self-hosted"}, {"name": "Linux"}]}]}),
                )
            }),
        )
        .route(
            "/repos/{org}/{repo}/actions/runs",
            get(move |Query(q): Query<HashMap<String, String>>| {
                let running = q["status"] == "in_progress" && !runs_done.load(Ordering::SeqCst);
                async move {
                    let runs = if running { json!([{"id": 11}]) } else { json!([]) };
                    Json(json!({"total_count": runs.as_array().unwrap().len(), "workflow_runs": runs}))
                }
            }),
        )
        .route(
            "/repos/{org}/{repo}/actions/runs/{run}/jobs",
            get(move || {
                // Re-run: the latest attempt has a new job, still running.
                let job = if jobs_done.load(Ordering::SeqCst) {
                    runner_job(112, "in_progress", None)
                } else {
                    runner_job(111, "in_progress", None)
                };
                async move { Json(json!({"total_count": 1, "jobs": [job]})) }
            }),
        )
        .route(
            "/repos/{org}/{repo}/actions/jobs/{id}",
            get(move || {
                reads.fetch_add(1, Ordering::SeqCst);
                async {
                    // Cancelled before a runner took it: completed, but not runner work.
                    let mut job = runner_job(111, "completed", Some("cancelled"));
                    job["runner_id"] = Value::Null;
                    job["runner_name"] = Value::Null;
                    Json(job)
                }
            }),
        );
    let base = serve(router).await;
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), base);
    engine.poll_github_runners().await;
    engine.poll_github_jobs_and_history().await;
    finished.store(true, Ordering::SeqCst);
    engine.poll_github_jobs_and_history().await;
    engine.flush_job_history();
    assert_eq!(single_reads.load(Ordering::SeqCst), 1);
    assert!(history(&engine).is_empty());
    let saved: Value = serde_json::from_slice(&std::fs::read(dir.path().join("job-history.json")).unwrap()).unwrap();
    assert_eq!(saved["pending"]["11"]["jobs"], json!([]), "111 isn't asked about again");
    assert!(
        saved["coverage"]["acme"]
            .as_array()
            .is_some_and(|spans| !spans.is_empty())
    );
}
