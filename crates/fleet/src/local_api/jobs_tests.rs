use super::*;
use crate::engine::MemorySecrets;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn engine_in(dir: &std::path::Path, demo: bool) -> Arc<Engine> {
    let e = Engine::load(dir.join("config.json"), Arc::new(MemorySecrets::default())).unwrap();
    e.set_inventory_path(Some(dir.join("missing.toml").display().to_string()))
        .unwrap();
    e.set_demo_mode(demo).unwrap();
    Arc::new(e)
}

async fn call(e: &Arc<Engine>, path: &str) -> (StatusCode, Value) {
    let mut req = Request::get(path)
        .header("host", "127.0.0.1:47622")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 50000))));
    let resp = router(e.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 16 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn jobs(body: &Value) -> &Vec<Value> {
    body["jobs"].as_array().unwrap()
}

#[tokio::test]
async fn demo_history_pages_and_filters_without_touching_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine_in(dir.path(), true);
    let (status, first) = call(&e, "/v1/jobs/history?limit=50").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["source"], "demo");
    assert_eq!(first["order"], "newestFirst");
    assert_eq!(jobs(&first).len(), 50);
    assert_eq!(first["hasMore"], true);
    let cursor = first["nextCursor"].as_str().unwrap();
    let (_, second) = call(&e, &format!("/v1/jobs/history?limit=50&before={cursor}")).await;
    let last_of_first = jobs(&first).last().unwrap()["completedAt"].as_str().unwrap().to_owned();
    assert!(jobs(&second)[0]["completedAt"].as_str().unwrap() <= last_of_first.as_str());

    let (_, studio) = call(&e, "/v1/jobs/history?machine=studio&limit=200").await;
    assert!(!jobs(&studio).is_empty());
    assert!(jobs(&studio).iter().all(|j| j["machineKey"] == "inv:studio"));
    let (_, runner) = call(&e, "/v1/jobs/history?runner=studio-1&conclusion=failure").await;
    assert!(!jobs(&runner).is_empty());
    assert!(
        jobs(&runner)
            .iter()
            .all(|j| j["runnerKey"] == "gh:acme-demo:101" && j["conclusion"] == "failure")
    );
    let (_, repo) = call(&e, "/v1/jobs/history?repo=ios-app").await;
    assert!(jobs(&repo).iter().all(|j| j["repo"] == "acme-demo/ios-app"));
    assert!(
        !dir.path().join("job-history.json").exists(),
        "demo history is never saved"
    );
}

#[tokio::test]
async fn demo_analytics_cover_each_window() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine_in(dir.path(), true);
    let (status, default) = call(&e, "/v1/analytics").await;
    assert_eq!(status, StatusCode::OK, "{default}");
    assert_eq!(default["analytics"]["window"], "24h");
    for (window, buckets) in [("24h", 24), ("7d", 28), ("30d", 30)] {
        let (_, body) = call(&e, &format!("/v1/analytics?window={window}")).await;
        let a = &body["analytics"];
        assert_eq!(a["window"], window);
        assert_eq!(a["buckets"].as_array().unwrap().len(), buckets);
        assert!(a["totals"]["jobs"].as_u64().unwrap() > 0);
        assert!(
            a["machines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["machineKey"] == "inv:studio")
        );
        assert_eq!(a["hostedCost"]["rates"]["asOf"], "2026-10-26");
    }
}

#[tokio::test]
async fn invalid_job_queries_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine_in(dir.path(), true);
    for path in [
        "/v1/jobs/history?limit=0",
        "/v1/jobs/history?limit=201",
        "/v1/jobs/history?before=00000000000000000001",
        "/v1/jobs/history?after=garbage",
        "/v1/jobs/history?conclusion=maybe",
        "/v1/jobs/history?unknown=1",
        "/v1/analytics?window=1y",
        "/v1/analytics?machine=studio",
    ] {
        let (status, body) = call(&e, path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert_eq!(body["code"], "invalid_request");
    }
}

#[tokio::test]
async fn live_history_reads_the_journal_including_removed_machines() {
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let job = |id: u64, minutes: i64| {
        json!({ "id": id, "runId": id, "org": "acme", "repo": "acme/web", "name": "build", "workflowName": "CI",
            "branch": "main", "labels": ["self-hosted", "Linux"], "conclusion": "success",
            "runnerKey": "gh:acme:7", "runnerName": "private-runner", "machineKey": "inv:removed",
            "machineName": "Private live machine", "os": "linux",
            "createdAt": now - chrono::Duration::minutes(minutes + 2), "startedAt": now - chrono::Duration::minutes(minutes + 1),
            "completedAt": now - chrono::Duration::minutes(minutes), "queueSecs": 60, "runSecs": 60,
            "htmlUrl": null, "source": "poll" })
    };
    std::fs::write(
        dir.path().join("job-history.json"),
        json!({ "version": 1, "started_at": now - chrono::Duration::days(2), "jobs": [job(1, 30), job(2, 20), job(3, 10)] })
            .to_string(),
    )
    .unwrap();
    let e = engine_in(dir.path(), false);
    let (_, all) = call(&e, "/v1/jobs/history").await;
    assert_eq!(all["source"], "live");
    assert_eq!(
        jobs(&all).iter().map(|j| j["id"].as_u64().unwrap()).collect::<Vec<_>>(),
        [3, 2, 1]
    );
    let (_, removed) = call(&e, "/v1/jobs/history?machine=inv:removed&runner=private-runner").await;
    assert_eq!(jobs(&removed).len(), 3);
    let oldest = jobs(&all)[2]["id"].as_u64().unwrap();
    assert_eq!(oldest, 1);
    let (_, page) = call(&e, "/v1/jobs/history?limit=1").await;
    let (_, newer) = call(&e, &format!("/v1/jobs/history?after={}&limit=2", "0".repeat(41))).await;
    assert_eq!(newer["code"], "invalid_request", "41 digits is not a cursor");
    let cursor = page["nextCursor"].as_str().unwrap();
    let (_, older) = call(&e, &format!("/v1/jobs/history?before={cursor}")).await;
    assert_eq!(
        jobs(&older)
            .iter()
            .map(|j| j["id"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [2, 1]
    );
    let (_, analytics) = call(&e, "/v1/analytics?window=24h").await;
    assert_eq!(analytics["analytics"]["totals"]["jobs"], 3);
    assert_eq!(analytics["analytics"]["hostedCost"]["minutes"], 3);

    e.set_demo_mode(true).unwrap();
    let (_, demo) = call(&e, "/v1/jobs/history?limit=200").await;
    assert_eq!(demo["source"], "demo");
    assert!(!demo.to_string().contains("Private live machine"));
}
