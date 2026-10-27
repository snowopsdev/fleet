//! GitHub client tests against a local fake server.

use super::*;
use axum::extract::{Path, Query};
use axum::http::{HeaderMap as AxHeaders, StatusCode as AxStatus};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use std::sync::Arc;

use crate::test_server::serve;

fn runner(id: u64, name: &str, busy: bool) -> Value {
    json!({"id": id, "name": name, "os": "macOS", "status": "online", "busy": busy,
           "labels": [{"id": 1, "name": "self-hosted", "type": "read-only"}, {"id": 2, "name": "macOS", "type": "read-only"}],
           "ephemeral": false, "runner_group_id": 1})
}

#[tokio::test]
async fn paginates_runners_and_sends_version_header() {
    let router = Router::new().route(
        "/orgs/{org}/actions/runners",
        get(
            |headers: AxHeaders, Query(q): Query<HashMap<String, String>>| async move {
                assert_eq!(headers.get("x-github-api-version").unwrap(), API_VERSION);
                assert_eq!(headers.get("authorization").unwrap(), "Bearer t0k");
                let page: u64 = q["page"].parse().unwrap();
                let runners = if page == 1 {
                    (1..=100)
                        .map(|i| runner(i, &format!("r{i}"), false))
                        .collect::<Vec<_>>()
                } else {
                    vec![runner(101, "r101", true)]
                };
                let mut h = AxHeaders::new();
                h.insert("x-ratelimit-limit", "5000".parse().unwrap());
                h.insert("x-ratelimit-remaining", "4990".parse().unwrap());
                (h, Json(json!({"total_count": 101, "runners": runners})))
            },
        ),
    );
    let base = serve(router).await;
    let client = GithubClient::new(&base, "t0k");
    let runners = client.org_runners("acme").await.unwrap();
    assert_eq!(runners.len(), 101);
    assert!(runners[100].busy);
    assert_eq!(runners[0].labels, vec!["self-hosted", "macOS"]);
    assert_eq!(client.rate_limit().unwrap().remaining, 4990);
}

#[tokio::test]
async fn reads_runner_group_repository_policy() {
    let router = Router::new().route(
        "/orgs/{org}/actions/runner-groups",
        get(|Path(org): Path<String>| async move {
            assert_eq!(org, "acme");
            Json(json!({"total_count": 3, "runner_groups": [
                {"id": 1, "name": "Default", "visibility": "all", "allows_public_repositories": false},
                {"id": 4, "name": "Open source", "visibility": "all", "allows_public_repositories": true},
                {"id": 5, "name": "Enterprise", "visibility": "all"},
            ]}))
        }),
    );
    let base = serve(router).await;
    let groups = GithubClient::new(&base, "t0k").runner_groups("acme").await.unwrap();
    assert_eq!(
        groups,
        [
            GhRunnerGroup {
                id: 1,
                name: "Default".into(),
                allows_public_repositories: Some(false)
            },
            GhRunnerGroup {
                id: 4,
                name: "Open source".into(),
                allows_public_repositories: Some(true)
            },
            GhRunnerGroup {
                id: 5,
                name: "Enterprise".into(),
                allows_public_repositories: None
            },
        ]
    );
}

#[test]
fn etag_cache_evicts_the_least_recently_used_page() {
    let page = || ApiPage {
        body: Value::Null,
        has_next: false,
    };
    let mut cache = EtagCache::default();
    for i in 0..MAX_CACHED_PAGES {
        cache.insert(format!("/page/{i}"), format!("\"{i}\""), page());
    }
    assert!(cache.get("/page/0").is_some());
    cache.insert("/page/new".into(), "\"new\"".into(), page());
    assert_eq!(cache.pages.len(), MAX_CACHED_PAGES);
    assert_eq!(cache.order.len(), MAX_CACHED_PAGES);
    assert!(cache.get("/page/0").is_some(), "recently used pages stay");
    assert!(cache.get("/page/1").is_none(), "the least recently used page goes");
    assert!(cache.get("/page/new").is_some());
}

#[tokio::test]
async fn forbidden_runner_list_explains_permission() {
    let router = Router::new().route(
        "/orgs/{org}/actions/runners",
        get(|| async {
            (
                AxStatus::FORBIDDEN,
                Json(json!({"message": "Resource not accessible by personal access token"})),
            )
        }),
    );
    let base = serve(router).await;
    let err = GithubClient::new(&base, "t").org_runners("acme").await.unwrap_err();
    let GithubError::Forbidden(msg) = err else {
        panic!("{err:?}")
    };
    assert!(msg.contains("Self-hosted runners: read"));
}

#[test]
fn rate_limits_are_told_apart_from_permission_problems() {
    let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
    let headers = |pairs: &[(&'static str, &str)]| {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.insert(*name, value.parse().unwrap());
        }
        h
    };
    let reset = (now + chrono::Duration::minutes(20)).timestamp().to_string();
    let primary = headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", &reset)]);
    assert_eq!(
        rate_limit_reset(403, &primary, "API rate limit exceeded", now),
        Some(now + chrono::Duration::minutes(20))
    );
    let secondary = headers(&[("retry-after", "90"), ("x-ratelimit-remaining", "4000")]);
    assert_eq!(
        rate_limit_reset(403, &secondary, "Forbidden", now),
        Some(now + chrono::Duration::seconds(90))
    );
    let none = HeaderMap::new();
    let wait = Some(now + SECONDARY_RATE_LIMIT_WAIT);
    assert_eq!(rate_limit_reset(429, &none, "", now), wait);
    assert_eq!(
        rate_limit_reset(403, &none, "You have exceeded a secondary rate limit.", now),
        wait
    );
    assert_eq!(
        rate_limit_reset(403, &none, "Resource not accessible by integration", now),
        None
    );
    assert_eq!(rate_limit_reset(500, &secondary, "", now), None);
}

#[tokio::test]
async fn secondary_rate_limit_is_not_forbidden() {
    let router = Router::new().route(
        "/orgs/{org}/actions/runners",
        get(|| async {
            let mut h = AxHeaders::new();
            h.insert("retry-after", "30".parse().unwrap());
            (
                AxStatus::FORBIDDEN,
                h,
                Json(json!({"message": "You have exceeded a secondary rate limit."})),
            )
        }),
    );
    let base = serve(router).await;
    let before = Utc::now();
    let err = GithubClient::new(&base, "t").org_runners("acme").await.unwrap_err();
    let GithubError::RateLimited { resets_at } = err else {
        panic!("{err:?}")
    };
    assert!(resets_at >= before + chrono::Duration::seconds(30));
    assert!(
        err.to_string()
            .starts_with("GitHub rate limit reached; checks resume at ")
    );
}

#[tokio::test]
async fn a_new_token_keeps_conditional_requests() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let router = Router::new().route(
        "/orgs/{org}/actions/runners",
        get(move |headers: AxHeaders| {
            let log = log.clone();
            async move {
                let header = |name: &str| headers.get(name).map(|v| v.to_str().unwrap().to_string());
                log.locked().push((header("authorization"), header("if-none-match")));
                if headers.contains_key("if-none-match") {
                    return AxStatus::NOT_MODIFIED.into_response();
                }
                let mut h = AxHeaders::new();
                h.insert("etag", "\"v1\"".parse().unwrap());
                (h, Json(json!({"total_count": 1, "runners": [runner(1, "r1", false)]}))).into_response()
            }
        }),
    );
    let base = serve(router).await;
    let client = GithubClient::new(&base, "old");
    let first = client.org_runners("acme").await.unwrap();
    client.set_token("new");
    assert_eq!(client.org_runners("acme").await.unwrap(), first);
    assert_eq!(
        *seen.locked(),
        vec![
            (Some("Bearer old".into()), None),
            (Some("Bearer new".into()), Some("\"v1\"".into()))
        ]
    );
}

#[tokio::test]
async fn personal_accounts_list_repository_runners() {
    let router = Router::new().route(
        "/repos/{owner}/{repo}/actions/runners",
        get(|Path((owner, repo)): Path<(String, String)>| async move {
            match repo.as_str() {
                "app" => (
                    AxStatus::OK,
                    Json(json!({"total_count": 2, "runners": [runner(5, "box", true), runner(6, "box-2", false)]})),
                ),
                "site" => (AxStatus::OK, Json(json!({"total_count": 0, "runners": []}))),
                "locked" => (AxStatus::UNAUTHORIZED, Json(json!({"message": "Bad credentials"}))),
                _ => {
                    assert_eq!(owner, "me");
                    (AxStatus::NOT_FOUND, Json(json!({"message": "Not Found"})))
                }
            }
        }),
    );
    let base = serve(router).await;
    let client = GithubClient::new(&base, "t");
    let mut account = OrgConfig {
        login: "me".into(),
        api_base: base.clone(),
        repo_scope: RepoScope::None,
        added_at: Utc::now(),
        auth: crate::model::OrgAuth::Token,
        kind: AccountKind::User,
        runner_repos: vec!["app".into(), "site".into()],
    };
    account.runner_repos.push("private".into());
    let listing = client.account_runners(&account).await.unwrap();
    assert!(listing.org.is_empty());
    let [(app, Ok(app_runners)), (site, Ok(site_runners)), (private, Err(err))] = listing.repos.as_slice() else {
        panic!("{:?}", listing.repos)
    };
    assert_eq!(
        (app.as_str(), site.as_str(), private.as_str()),
        ("app", "site", "private")
    );
    assert_eq!(app_runners.iter().map(|r| r.id).collect::<Vec<_>>(), vec![5, 6]);
    assert!(app_runners.iter().all(|r| r.repo.as_deref() == Some("me/app")));
    assert!(site_runners.is_empty());
    // A repository that can't be listed stays beside the others, with the fix in its message.
    let GithubError::RepoRunners(msg) = err else {
        panic!("{err:?}")
    };
    assert!(
        msg.starts_with("Couldn't list me/private's runners: Not Found."),
        "{msg}"
    );
    assert!(msg.contains("Administration: read") && msg.contains("renamed or deleted"));

    // A rejected token isn't about one repository: the whole read fails.
    account.runner_repos.push("locked".into());
    assert_eq!(
        client.account_runners(&account).await.unwrap_err(),
        GithubError::Unauthorized
    );
}

#[tokio::test]
async fn reads_published_runner_releases() {
    let router = Router::new().route(
        "/repos/actions/runner/releases",
        get(|Query(q): Query<HashMap<String, String>>| async move {
            assert_eq!(q["per_page"], "30");
            Json(json!([
                {"tag_name": "v2.331.0", "published_at": null, "draft": true},
                {"tag_name": "v2.330.0-beta", "published_at": "2026-10-02T09:00:00Z", "prerelease": true},
                {"tag_name": "v2.329.0", "published_at": "2026-10-01T09:00:00Z", "draft": false, "prerelease": false},
                {"tag_name": "v2.328.0", "published_at": "2026-08-01T09:00:00Z"}
            ]))
        }),
    );
    let base = serve(router).await;
    let releases = GithubClient::new(&base, "t").runner_releases().await.unwrap();
    assert_eq!(
        releases,
        vec![
            RunnerRelease {
                version: "2.329.0".into(),
                published_at: Some("2026-10-01T09:00:00Z".parse().unwrap()),
            },
            RunnerRelease {
                version: "2.328.0".into(),
                published_at: Some("2026-08-01T09:00:00Z".parse().unwrap()),
            },
        ]
    );
}

#[tokio::test]
async fn recent_scope_adds_extra_repos_and_keeps_held_jobs() {
    let router = Router::new()
        .route(
            "/user/repos",
            get(|Query(q): Query<HashMap<String, String>>| async move {
                assert_eq!(q["sort"], "pushed");
                Json(json!([
                    {"name": "app", "owner": {"login": "Me"}},
                    {"name": "theirs", "owner": {"login": "someone-else"}}
                ]))
            }),
        )
        .route(
            "/repos/{owner}/{repo}/actions/runs",
            get(|Path((_owner, repo)): Path<(String, String)>, Query(q): Query<HashMap<String, String>>| async move {
                let runs = match (repo.as_str(), q["status"].as_str()) {
                    ("nightly", "in_progress") => json!([{"id": 31}]),
                    ("app", "waiting") => json!([{"id": 41}]),
                    // Blocked by a concurrency group, and not yet started.
                    ("app", "pending") => json!([{"id": 51}]),
                    ("nightly", "requested") => json!([{"id": 61}]),
                    _ => json!([]),
                };
                Json(json!({"total_count": 0, "workflow_runs": runs}))
            }),
        )
        .route(
            "/repos/{owner}/{repo}/actions/runs/{run}/jobs",
            get(|Path((_o, _r, run)): Path<(String, String, u64)>| async move {
                let jobs = if run == 31 {
                    json!([{"id": 311, "run_id": 31, "name": "cron", "status": "in_progress", "labels": ["self-hosted"], "runner_id": 5}])
                } else if run == 51 || run == 61 {
                    json!([{"id": run * 10 + 1, "run_id": run, "name": "held", "status": if run == 51 {"pending"} else {"requested"}, "labels": ["self-hosted"]}])
                } else {
                    json!([
                        {"id": 411, "run_id": 41, "name": "deploy", "status": "waiting", "labels": ["self-hosted"]},
                        {"id": 412, "run_id": 41, "name": "build", "status": "completed", "labels": ["self-hosted"]}
                    ])
                };
                Json(json!({"total_count": 0, "jobs": jobs}))
            }),
        );
    let base = serve(router).await;
    let account = OrgConfig {
        login: "me".into(),
        api_base: base.clone(),
        repo_scope: RepoScope::Recent { max_repos: 5 },
        added_at: Utc::now(),
        auth: crate::model::OrgAuth::Token,
        kind: AccountKind::User,
        runner_repos: Vec::new(),
    };
    let scan = GithubClient::new(&base, "t")
        .scan_account_jobs(&account, &["APP".into(), "nightly".into()])
        .await
        .unwrap();
    assert_eq!(scan.repos_scanned, vec!["app", "nightly"]);
    assert_eq!(
        scan.jobs.iter().map(|j| (j.id, j.status.as_str())).collect::<Vec<_>>(),
        vec![
            (311, "in_progress"),
            (411, "waiting"),
            (511, "pending"),
            (611, "requested")
        ]
    );
}

#[tokio::test]
async fn a_full_recent_scope_keeps_room_for_extra_repos() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    let router = Router::new()
        .route(
            "/orgs/{org}/repos",
            get(move |Query(q): Query<HashMap<String, String>>| {
                let log = log.clone();
                async move {
                    let per_page: usize = q["per_page"].parse().unwrap();
                    log.locked().push(per_page);
                    Json(Value::Array(
                        (0..per_page).map(|i| json!({"name": format!("recent{i}")})).collect(),
                    ))
                }
            }),
        )
        .route(
            "/repos/{org}/{repo}/actions/runs",
            get(|| async { Json(json!({"total_count": 0, "workflow_runs": []})) }),
        );
    let base = serve(router).await;
    let client = GithubClient::new(&base, "t");
    let mut account = OrgConfig {
        login: "acme".into(),
        api_base: base.clone(),
        repo_scope: RepoScope::Recent { max_repos: 50 },
        added_at: Utc::now(),
        auth: crate::model::OrgAuth::Token,
        kind: AccountKind::Organization,
        runner_repos: Vec::new(),
    };
    let extras: Vec<String> = ["gpu", "nightly", "GPU"].map(String::from).to_vec();
    let scan = client.scan_account_jobs(&account, &extras).await.unwrap();
    assert_eq!(scan.repos_scanned.len(), MAX_REPOS);
    assert_eq!(&scan.repos_scanned[48..], ["gpu", "nightly"]);
    assert!(scan.truncated, "two of the 50 requested recent repositories gave way");

    // Up to MAX_EXTRA_REPOS extras are reserved; any beyond don't fit once recent ones fill the rest.
    let many: Vec<String> = (0..MAX_EXTRA_REPOS + 5).map(|i| format!("extra{i}")).collect();
    let scan = client.scan_account_jobs(&account, &many).await.unwrap();
    assert_eq!(scan.repos_scanned.len(), MAX_REPOS);
    assert!(scan.repos_scanned.contains(&format!("extra{}", MAX_EXTRA_REPOS - 1)));
    assert!(!scan.repos_scanned.contains(&format!("extra{MAX_EXTRA_REPOS}")));
    assert!(scan.truncated);

    // A scope with room to spare isn't truncated by extras.
    account.repo_scope = RepoScope::Recent { max_repos: 10 };
    let scan = client.scan_account_jobs(&account, &extras).await.unwrap();
    assert_eq!(scan.repos_scanned.len(), 12);
    assert!(!scan.truncated);
    assert_eq!(*asked.locked(), vec![48, MAX_REPOS - MAX_EXTRA_REPOS, 10]);
}

#[tokio::test]
async fn scans_jobs_across_recent_repos() {
    let router = Router::new()
        .route(
            "/orgs/{org}/repos",
            get(|| async {
                Json(json!([{"name": "web", "archived": false}, {"name": "old", "archived": true}, {"name": "api"}]))
            }),
        )
        .route(
            "/repos/{org}/{repo}/actions/runs",
            get(
                |Path((_org, repo)): Path<(String, String)>, Query(q): Query<HashMap<String, String>>| async move {
                    let runs = match (repo.as_str(), q["status"].as_str()) {
                        ("web", "in_progress") => json!([{"id": 11}]),
                        ("web", "queued") => json!([{"id": 11}, {"id": 12}]),
                        _ => json!([]),
                    };
                    Json(json!({"total_count": 0, "workflow_runs": runs}))
                },
            ),
        )
        .route(
            "/repos/{org}/{repo}/actions/runs/{run}/jobs",
            get(|Path((_o, _r, run)): Path<(String, String, u64)>| async move {
                let jobs = if run == 11 {
                    json!([
                        {"id": 111, "run_id": 11, "name": "build", "workflow_name": "CI", "status": "in_progress",
                         "labels": ["self-hosted", "macOS"], "runner_id": 7, "runner_name": "mini",
                         "created_at": "2026-10-03T10:00:00Z", "started_at": "2026-10-03T10:01:00Z"},
                        {"id": 112, "run_id": 11, "name": "lint", "status": "completed", "labels": ["ubuntu-latest"]}
                    ])
                } else {
                    json!([{"id": 121, "run_id": 12, "name": "test", "status": "queued",
                            "labels": ["self-hosted", "linux"], "runner_id": null, "runner_name": null,
                            "created_at": "2026-10-03T10:02:00Z"}])
                };
                Json(json!({"total_count": 0, "jobs": jobs}))
            }),
        );
    let base = serve(router).await;
    let scan = GithubClient::new(&base, "t")
        .scan_jobs("acme", &RepoScope::Recent { max_repos: 10 })
        .await
        .unwrap();
    assert_eq!(scan.repos_scanned, vec!["web", "api"]);
    assert_eq!(scan.jobs.iter().map(|j| j.id).collect::<Vec<_>>(), vec![111, 121]);
    assert_eq!(scan.jobs[0].runner_id, Some(7));
    assert_eq!(scan.jobs[0].repo, "acme/web");
    assert_eq!(scan.jobs[1].runner_id, None);
    assert!(!scan.truncated);
}
