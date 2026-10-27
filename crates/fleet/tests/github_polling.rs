//! Polling behaviour against a local fake GitHub: rate-limit backoff, token re-reads that keep
//! the conditional cache, job-scan widening for busy runners, personal accounts and runner
//! repositories that can't be listed.
#![expect(clippy::unwrap_used, reason = "test fixtures fail the test loudly")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use fleet::engine::{AccountSetup, Engine, MemorySecrets, SecretStore};
use fleet::model::{AccountKind, AppConfig, OrgAuth, OrgConfig, RepoScope, RunnerActivity};
use fleet::sync::MutexExt;
use serde_json::json;

mod common;
use common::serve;

fn engine(dir: &std::path::Path, base: &str, scope: RepoScope, secrets: Arc<MemorySecrets>) -> Engine {
    engine_with_repos(dir, base, scope, &[], secrets)
}

fn engine_with_repos(
    dir: &std::path::Path,
    base: &str,
    scope: RepoScope,
    runner_repos: &[&str],
    secrets: Arc<dyn SecretStore>,
) -> Engine {
    secrets.set("acme", "first").unwrap();
    let config = AppConfig {
        orgs: vec![OrgConfig {
            login: "acme".into(),
            api_base: base.into(),
            repo_scope: scope,
            added_at: Utc::now(),
            auth: OrgAuth::Token,
            kind: AccountKind::Organization,
            runner_repos: runner_repos.iter().map(ToString::to_string).collect(),
        }],
        ..Default::default()
    };
    let path = dir.join("config.json");
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    Engine::load(path, secrets).unwrap()
}

fn rate_limited() -> (StatusCode, HeaderMap, Json<serde_json::Value>) {
    let mut h = HeaderMap::new();
    h.insert("x-ratelimit-remaining", "0".parse().unwrap());
    let reset = (Utc::now() + chrono::Duration::hours(1)).timestamp();
    h.insert("x-ratelimit-reset", reset.to_string().parse().unwrap());
    (
        StatusCode::FORBIDDEN,
        h,
        Json(json!({"message": "API rate limit exceeded"})),
    )
}

#[tokio::test]
async fn rate_limited_accounts_are_skipped_until_the_reset() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let reset = (Utc::now() + chrono::Duration::hours(1)).timestamp();
    let app = Router::new().route(
        "/orgs/{org}/actions/runners",
        get(move || {
            let hits = seen.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let mut h = HeaderMap::new();
                h.insert("x-ratelimit-limit", "5000".parse().unwrap());
                h.insert("x-ratelimit-remaining", "0".parse().unwrap());
                h.insert("x-ratelimit-reset", reset.to_string().parse().unwrap());
                (
                    StatusCode::FORBIDDEN,
                    h,
                    Json(json!({"message": "API rate limit exceeded"})),
                )
            }
        }),
    );
    let base = serve(app).await;
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), &base, RepoScope::None, Arc::new(MemorySecrets::default()));
    engine.poll_github_runners().await;
    let view = engine.view();
    let org = &view.orgs[0];
    let until = org.rate_limited_until.unwrap();
    assert!(
        until > chrono::DateTime::from_timestamp(reset, 0).unwrap(),
        "resumes after the reset"
    );
    assert!(until < chrono::DateTime::from_timestamp(reset + 10, 0).unwrap());
    assert!(org.error.as_ref().unwrap().contains("rate limit"), "{:?}", org.error);
    assert_eq!(org.rate_limit.as_ref().unwrap().remaining, 0);

    engine.poll_github_runners().await;
    engine.poll_github_jobs().await;
    assert_eq!(hits.load(Ordering::SeqCst), 1, "no request until the reset");
}

#[tokio::test]
async fn rejected_tokens_are_reread_without_losing_cached_responses() {
    let rejected = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (reject, log) = (rejected.clone(), seen.clone());
    let app = Router::new().route(
        "/orgs/{org}/actions/runners",
        get(move |headers: HeaderMap| {
            let (reject, log) = (reject.clone(), log.clone());
            async move {
                let auth = headers["authorization"].to_str().unwrap().to_string();
                log.locked().push((auth.clone(), headers.contains_key("if-none-match")));
                if reject.load(Ordering::SeqCst) && auth == "Bearer first" {
                    return (StatusCode::UNAUTHORIZED, Json(json!({"message": "Bad credentials"}))).into_response();
                }
                if headers.contains_key("if-none-match") {
                    return StatusCode::NOT_MODIFIED.into_response();
                }
                let mut h = HeaderMap::new();
                h.insert("etag", "\"v1\"".parse().unwrap());
                let runners = json!([{"id": 7, "name": "fake", "status": "online", "busy": false}]);
                (h, Json(json!({"total_count": 1, "runners": runners}))).into_response()
            }
        }),
    );
    let base = serve(app).await;
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecrets::default());
    let engine = engine(dir.path(), &base, RepoScope::None, secrets.clone());
    engine.poll_github_runners().await;
    rejected.store(true, Ordering::SeqCst);
    engine.poll_github_runners().await;
    assert!(engine.view().orgs[0].error.is_some());
    secrets.set("acme", "second").unwrap();
    engine.poll_github_runners().await;
    let view = engine.view();
    assert!(view.orgs[0].error.is_none(), "{:?}", view.orgs[0].error);
    assert_eq!(view.unmatched_runners.len(), 1);
    assert_eq!(
        *seen.locked(),
        vec![
            ("Bearer first".to_string(), false),
            ("Bearer first".to_string(), true),
            ("Bearer second".to_string(), true),
        ],
        "the re-read token sends the cached ETag and gets a 304"
    );
}

#[tokio::test]
async fn busy_runner_jobs_are_found_outside_recently_pushed_repos() {
    let probes = Arc::new(AtomicUsize::new(0));
    let probed = probes.clone();
    let app = Router::new()
        .route(
            "/orgs/{org}/actions/runners",
            get(|| async {
                let runners = json!([{"id": 7, "name": "mini", "status": "online", "busy": true}]);
                Json(json!({"total_count": 1, "runners": runners}))
            }),
        )
        .route(
            "/orgs/{org}/repos",
            get(move |Query(q): Query<HashMap<String, String>>| {
                let probed = probed.clone();
                async move {
                    if q["sort"] == "full_name" {
                        probed.fetch_add(1, Ordering::SeqCst);
                        return Json(json!([{"name": "nightly"}, {"name": "web"}]));
                    }
                    Json(json!([{"name": "web"}]))
                }
            }),
        )
        .route(
            "/repos/{org}/{repo}/actions/runs",
            get(
                |Path((_org, repo)): Path<(String, String)>, Query(q): Query<HashMap<String, String>>| async move {
                    let runs = if repo == "nightly" && q["status"] == "in_progress" {
                        json!([{"id": 9}])
                    } else {
                        json!([])
                    };
                    Json(json!({"total_count": 0, "workflow_runs": runs}))
                },
            ),
        )
        .route(
            "/repos/{org}/{repo}/actions/runs/{run}/jobs",
            get(|| async {
                let jobs = json!([{"id": 90, "run_id": 9, "name": "scheduled", "status": "in_progress",
                                   "labels": ["self-hosted"], "runner_id": 7, "runner_name": "mini"}]);
                Json(json!({"total_count": 1, "jobs": jobs}))
            }),
        );
    let base = serve(app).await;
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(
        dir.path(),
        &base,
        RepoScope::Recent { max_repos: 1 },
        Arc::new(MemorySecrets::default()),
    );
    engine.poll_github_runners().await;
    engine.poll_github_jobs().await;
    let view = engine.view();
    let runner = &view.unmatched_runners[0];
    assert!(matches!(runner.activity, RunnerActivity::Busy { .. }));
    assert_eq!(runner.current_job.as_ref().map(|j| j.id), Some(90));
    assert_eq!(view.orgs[0].repos_scanned, 2, "web, then the probed nightly");

    // Found: the repository stays in the scan without probing again.
    engine.poll_github_jobs().await;
    let view = engine.view();
    assert_eq!(view.unmatched_runners[0].current_job.as_ref().map(|j| j.id), Some(90));
    assert_eq!(view.orgs[0].repos_scanned, 2);
    assert_eq!(probes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn personal_accounts_watch_repository_runners() {
    let app = Router::new().route(
        "/repos/{owner}/{repo}/actions/runners",
        get(|Path((owner, repo)): Path<(String, String)>| async move {
            assert_eq!((owner.as_str(), repo.as_str()), ("me", "app"));
            let runners = json!([{"id": 5, "name": "desk", "status": "online", "busy": false, "version": "2.328.0"}]);
            Json(json!({"total_count": 1, "runners": runners}))
        }),
    );
    let base = serve(app).await;
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::load(dir.path().join("config.json"), Arc::new(MemorySecrets::default())).unwrap();
    let setup = |runner_repos: Vec<String>| AccountSetup {
        kind: AccountKind::User,
        repo_scope: RepoScope::None,
        runner_repos,
    };
    let err = engine
        .add_account("me", setup(Vec::new()), OrgAuth::Token, Some("t"), Some(base.clone()))
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("A personal account has no account-wide runner list.")
    );
    let err = engine
        .add_account(
            "me",
            setup(vec!["other/app".into()]),
            OrgAuth::Token,
            Some("t"),
            Some(base.clone()),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().starts_with("other/app belongs to other, not me."),
        "{err}"
    );

    engine
        .add_account(
            "me",
            setup(vec!["me/app".into(), "APP".into()]),
            OrgAuth::Token,
            Some("t"),
            Some(base),
        )
        .await
        .unwrap();
    assert_eq!(engine.config().orgs[0].runner_repos, vec!["app"]);
    engine.poll_github_runners().await;
    let view = engine.view();
    let org = &view.orgs[0];
    assert_eq!((org.kind, org.error.as_deref()), (AccountKind::User, None));
    let runner = &view.unmatched_runners[0];
    assert_eq!(runner.github.as_ref().unwrap().repo.as_deref(), Some("me/app"));
    assert_eq!(runner.org.as_deref(), Some("me"));
}

#[tokio::test]
async fn a_poll_started_before_a_rate_limit_does_not_lift_it() {
    let limited = Arc::new(tokio::sync::Notify::new());
    let (wait, signal) = (limited.clone(), limited.clone());
    let app = Router::new()
        .route(
            "/orgs/{org}/actions/runners",
            get(move || {
                let wait = wait.clone();
                async move {
                    // Answer only after the job scan has been rate-limited.
                    wait.notified().await;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    Json(json!({"total_count": 0, "runners": []}))
                }
            }),
        )
        .route(
            "/repos/{org}/{repo}/actions/runs",
            get(move || {
                let signal = signal.clone();
                async move {
                    signal.notify_one();
                    rate_limited()
                }
            }),
        );
    let base = serve(app).await;
    let dir = tempfile::tempdir().unwrap();
    let scope = RepoScope::Selected {
        repos: vec!["web".into()],
    };
    let engine = engine(dir.path(), &base, scope, Arc::new(MemorySecrets::default()));
    tokio::join!(engine.poll_github_runners(), engine.poll_github_jobs());
    let view = engine.view();
    assert!(view.orgs[0].runners_as_of.is_some(), "the runner poll succeeded");
    assert!(
        view.orgs[0].rate_limited_until.is_some_and(|until| until > Utc::now()),
        "the job scan's pause survives the runner poll that started earlier"
    );
}

#[tokio::test]
async fn the_probe_moves_on_only_after_its_page_was_scanned() {
    let pages = Arc::new(Mutex::new(Vec::new()));
    let log = pages.clone();
    let runner_repos: Vec<String> = (0..fleet::github::MAX_EXTRA_REPOS).map(|i| format!("r{i}")).collect();
    let app = Router::new()
        .route(
            "/orgs/{org}/actions/runners",
            get(|| async { Json(json!({"total_count": 0, "runners": []})) }),
        )
        .route(
            "/repos/{org}/{repo}/actions/runners",
            get(|Path((_org, repo)): Path<(String, String)>| async move {
                let runners = if repo == "r0" {
                    json!([{"id": 7, "name": "busy", "status": "online", "busy": true}])
                } else {
                    json!([])
                };
                Json(json!({"total_count": runners.as_array().unwrap().len(), "runners": runners}))
            }),
        )
        .route(
            "/orgs/{org}/repos",
            get(move |Query(q): Query<HashMap<String, String>>| {
                let log = log.clone();
                async move {
                    let per_page: usize = q["per_page"].parse().unwrap();
                    let mut h = HeaderMap::new();
                    if q["sort"] == "full_name" {
                        log.locked().push(q["page"].clone());
                        h.insert("link", "<https://example.invalid/next>; rel=\"next\"".parse().unwrap());
                        let repos: Vec<_> = (0..per_page).map(|i| json!({"name": format!("probe{i}")})).collect();
                        return (h, Json(json!(repos)));
                    }
                    let repos: Vec<_> = (0..per_page).map(|i| json!({"name": format!("recent{i}")})).collect();
                    (h, Json(json!(repos)))
                }
            }),
        )
        .route(
            "/repos/{org}/{repo}/actions/runs",
            get(|| async { Json(json!({"total_count": 0, "workflow_runs": []})) }),
        );
    let base = serve(app).await;
    let dir = tempfile::tempdir().unwrap();
    let repos: Vec<&str> = runner_repos.iter().map(String::as_str).collect();
    let engine = engine_with_repos(
        dir.path(),
        &base,
        RepoScope::Recent { max_repos: 50 },
        &repos,
        Arc::new(MemorySecrets::default()),
    );
    engine.poll_github_runners().await;
    engine.poll_github_jobs().await;
    engine.poll_github_jobs().await;
    let view = engine.view();
    // 30 recent repositories and the 20 runner repositories fill the scan; the probe doesn't fit.
    assert_eq!(view.orgs[0].repos_scanned, 50);
    assert!(view.orgs[0].jobs_truncated);
    assert!(!view.queue.complete);
    assert_eq!(
        *pages.locked(),
        vec!["1", "1"],
        "an unscanned probe page is tried again"
    );
}

/// Signals that a read started, waits to be released, then returns a stale value.
type Gate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>, String);

/// A secret store whose next read can be held, to interleave a sign-in with a token re-read.
#[derive(Default)]
struct GatedSecrets {
    inner: MemorySecrets,
    gate: Mutex<Option<Gate>>,
}

impl SecretStore for GatedSecrets {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        let gate = self.gate.locked().take();
        if let Some((entered, release, stale)) = gate {
            entered.send(()).unwrap();
            release.recv().unwrap();
            return Ok(Some(stale));
        }
        self.inner.get(account)
    }
    fn set(&self, account: &str, value: &str) -> Result<(), String> {
        self.inner.set(account, value)
    }
    fn delete(&self, account: &str) -> Result<(), String> {
        self.inner.delete(account)
    }
}

#[tokio::test]
async fn a_token_reread_never_replaces_a_newer_sign_in() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = Router::new().route(
        "/orgs/{org}/actions/runners",
        get(move |headers: HeaderMap| {
            let log = log.clone();
            async move {
                let auth = headers["authorization"].to_str().unwrap().to_string();
                log.locked().push(auth.clone());
                if auth == "Bearer new" {
                    (StatusCode::OK, Json(json!({"total_count": 0, "runners": []})))
                } else {
                    (StatusCode::UNAUTHORIZED, Json(json!({"message": "Bad credentials"})))
                }
            }
        }),
    );
    let base = serve(app).await;
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(GatedSecrets::default());
    let engine = Arc::new(engine_with_repos(
        dir.path(),
        &base,
        RepoScope::None,
        &[],
        secrets.clone(),
    ));
    engine.poll_github_runners().await;
    assert!(engine.view().orgs[0].error.is_some());

    // The next poll re-reads the rejected token, and sees the old value after Settings saved a new one.
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    *secrets.gate.locked() = Some((entered_tx, release_rx, "first".into()));
    let polling = engine.clone();
    let poll = tokio::spawn(async move { polling.poll_github_runners().await });
    tokio::task::spawn_blocking(move || entered.recv().unwrap())
        .await
        .unwrap();
    engine.update_org_token("acme", "new").await.unwrap();
    release.send(()).unwrap();
    poll.await.unwrap();

    let view = engine.view();
    assert!(view.orgs[0].error.is_none(), "{:?}", view.orgs[0].error);
    assert_eq!(seen.locked().last().map(String::as_str), Some("Bearer new"));
    engine.poll_github_runners().await;
    assert_eq!(seen.locked().last().map(String::as_str), Some("Bearer new"));
}

/// Counts token reads, to show which failures re-read the credential.
#[derive(Default)]
struct CountingSecrets {
    inner: MemorySecrets,
    reads: AtomicUsize,
}

impl SecretStore for CountingSecrets {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(account)
    }
    fn set(&self, account: &str, value: &str) -> Result<(), String> {
        self.inner.set(account, value)
    }
    fn delete(&self, account: &str) -> Result<(), String> {
        self.inner.delete(account)
    }
}

#[tokio::test]
async fn a_runner_repository_that_cant_be_listed_only_affects_its_own_runners() {
    let locked = Arc::new(AtomicBool::new(false));
    let lock = locked.clone();
    let app = Router::new()
        .route(
            "/orgs/{org}/actions/runners",
            get(|| async {
                let runners = json!([{"id": 1, "name": "org-runner", "status": "online", "busy": false}]);
                Json(json!({"total_count": 1, "runners": runners}))
            }),
        )
        .route(
            "/repos/{org}/{repo}/actions/runners",
            get(move |Path((_org, repo)): Path<(String, String)>| {
                let lock = lock.clone();
                async move {
                    if repo == "gone" {
                        return (StatusCode::NOT_FOUND, Json(json!({"message": "Not Found"})));
                    }
                    if lock.load(Ordering::SeqCst) {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({"message": "Must have admin rights"})),
                        );
                    }
                    let runners = json!([{"id": 2, "name": "good-runner", "status": "online", "busy": false}]);
                    (StatusCode::OK, Json(json!({"total_count": 1, "runners": runners})))
                }
            }),
        );
    let base = serve(app).await;
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(CountingSecrets::default());
    let engine = engine_with_repos(dir.path(), &base, RepoScope::None, &["good", "gone"], secrets.clone());
    engine.poll_github_runners().await;
    let view = engine.view();
    let org = &view.orgs[0];
    assert_eq!(org.error, None);
    assert_eq!(
        org.runner_repo_errors
            .iter()
            .map(|e| e.repo.as_str())
            .collect::<Vec<_>>(),
        vec!["gone"]
    );
    assert!(org.runner_repo_errors[0].error.contains("renamed or deleted"));
    let names: Vec<&str> = view.unmatched_runners.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["good-runner", "org-runner"]);
    let issue = view.issues.iter().find(|i| i.id == "org:acme:repo:gone").unwrap();
    assert_eq!(issue.title, "GitHub: acme/gone runners");
    let first_read = view.unmatched_runners[0].github.as_ref().unwrap().as_of;

    // The good repository loses access: its runner keeps its last listing and age.
    locked.store(true, Ordering::SeqCst);
    engine.poll_github_runners().await;
    let view = engine.view();
    assert_eq!(view.orgs[0].error, None);
    assert_eq!(view.orgs[0].runner_repo_errors.len(), 2);
    let good = view.unmatched_runners.iter().find(|r| r.name == "good-runner").unwrap();
    assert_eq!(good.github.as_ref().unwrap().as_of, first_read);
    let org_runner = view.unmatched_runners.iter().find(|r| r.name == "org-runner").unwrap();
    assert!(org_runner.github.as_ref().unwrap().as_of > first_read);
    assert_eq!(
        secrets.reads.load(Ordering::SeqCst),
        1,
        "a repository's failure isn't a credential problem"
    );
}
