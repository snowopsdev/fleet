//! Polling behaviour against a local fake GitHub: rate-limit backoff, token re-reads that keep
//! the conditional cache, job-scan widening for busy runners, and personal accounts.
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
    secrets.set("acme", "first").unwrap();
    let config = AppConfig {
        orgs: vec![OrgConfig {
            login: "acme".into(),
            api_base: base.into(),
            repo_scope: scope,
            added_at: Utc::now(),
            auth: OrgAuth::Token,
            kind: AccountKind::Organization,
            runner_repos: Vec::new(),
        }],
        ..Default::default()
    };
    let path = dir.join("config.json");
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    Engine::load(path, secrets).unwrap()
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
