//! Alert destinations against a fake loopback server, and the durable outbox.
//! Nothing here contacts a real webhook or ntfy server.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use chrono::{Duration, TimeZone, Utc};

use super::*;
use crate::monitoring::{FleetEventKind, MonitorStore};
use crate::sync::MutexExt;

/// Requests the fake server received: path, selected headers, body.
type Received = Arc<Mutex<Vec<(String, Vec<(String, String)>, String)>>>;

async fn fake_server(status: StatusCode) -> (String, Received) {
    let received: Received = Arc::default();
    let log = received.clone();
    let router = axum::Router::new().route(
        "/{*path}",
        post(move |uri: axum::http::Uri, headers: HeaderMap, body: String| {
            let log = log.clone();
            async move {
                let picked = ["content-type", "title", "priority", "tags", "authorization"]
                    .iter()
                    .filter_map(|h| {
                        headers
                            .get(*h)
                            .map(|v| (h.to_string(), v.to_str().unwrap().to_string()))
                    })
                    .collect();
                log.locked().push((uri.path().to_string(), picked, body));
                status
            }
        }),
    );
    (crate::test_server::serve(router).await, received)
}

fn destination(kind: AlertDestinationKind, base: &str) -> (AlertDestination, DestinationSecret) {
    let ntfy = kind == AlertDestinationKind::Ntfy;
    (
        AlertDestination {
            id: "d1".into(),
            name: "Test".into(),
            kind,
            enabled: true,
            ntfy_server: ntfy.then(|| base.to_string()),
            ntfy_topic: ntfy.then(|| "fleet-alerts".to_string()),
            summary: "127.0.0.1".into(),
        },
        DestinationSecret {
            url: (!ntfy).then(|| format!("{base}/services/T000/B000/secret-path")),
            token: ntfy.then(|| "tk_secret".to_string()),
        },
    )
}

fn event(kind: FleetEventKind) -> FleetMachineEvent {
    FleetMachineEvent {
        id: "00000000000000000001".into(),
        machine_key: "inv:studio".into(),
        machine_name: "studio".into(),
        at: Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap(),
        kind,
        detail: "Offline for at least three minutes across fresh tailnet checks.".into(),
    }
}

#[tokio::test]
async fn each_destination_kind_posts_its_own_format() {
    let (base, received) = fake_server(StatusCode::OK).await;
    let client = client().unwrap();
    let alert = notice(&event(FleetEventKind::OfflineAlert));
    for kind in [
        AlertDestinationKind::Slack,
        AlertDestinationKind::Discord,
        AlertDestinationKind::Ntfy,
        AlertDestinationKind::Webhook,
    ] {
        let (d, secret) = destination(kind, &base);
        send(&client, request(&d, &secret, &alert).unwrap()).await.unwrap();
    }
    let received = received.locked().clone();
    let text = "Alert: studio\nOffline for at least three minutes across fresh tailnet checks.";
    let slack: serde_json::Value = serde_json::from_str(&received[0].2).unwrap();
    assert_eq!(slack, serde_json::json!({ "text": text }));
    let discord: serde_json::Value = serde_json::from_str(&received[1].2).unwrap();
    assert_eq!(discord, serde_json::json!({ "content": text }));
    let (path, headers, body) = &received[2];
    assert_eq!(path, "/fleet-alerts");
    assert_eq!(body, "Offline for at least three minutes across fresh tailnet checks.");
    assert!(headers.contains(&("priority".into(), "high".into())));
    assert!(headers.contains(&("authorization".into(), "Bearer tk_secret".into())));
    assert!(headers.contains(&("title".into(), "Alert: studio".into())));
    let webhook: serde_json::Value = serde_json::from_str(&received[3].2).unwrap();
    assert_eq!(webhook["source"], "runner-dashboard");
    assert_eq!(webhook["event"]["kind"], "offlineAlert");
    assert_eq!(webhook["recovery"], false);
}

#[tokio::test]
async fn failures_and_redirects_never_repeat_the_secret_url() {
    let (base, _) = fake_server(StatusCode::FOUND).await;
    let (d, secret) = destination(AlertDestinationKind::Slack, &base);
    let error = send(&client().unwrap(), request(&d, &secret, &test_notice()).unwrap())
        .await
        .unwrap_err();
    assert_eq!(error, "The destination answered HTTP 302.", "redirects aren't followed");
    // Nothing listens on this port.
    let (d, mut secret) = destination(AlertDestinationKind::Slack, "http://127.0.0.1:9");
    secret.url = Some("http://127.0.0.1:9/services/secret-path".into());
    let error = send(&client().unwrap(), request(&d, &secret, &test_notice()).unwrap())
        .await
        .unwrap_err();
    assert!(!error.contains("secret-path"), "{error}");
    assert!(!format!("{secret:?}").contains("secret-path"));
}

#[test]
fn only_https_or_this_machine_is_accepted() {
    assert!(check_url("https://hooks.slack.com/services/x").is_ok());
    assert!(check_url("http://127.0.0.1:8080/hook").is_ok());
    for bad in [
        "http://hooks.slack.com/services/x",
        "ftp://example.com",
        "hooks.slack.com",
        "",
    ] {
        assert!(check_url(bad).is_err(), "{bad}");
    }
    assert!(check_topic("fleet-alerts_1").is_ok());
    assert!(check_topic("../x").is_err());
}

#[test]
fn ntfy_servers_carry_no_credentials_or_queries() {
    assert!(check_ntfy_server("https://ntfy.sh").is_ok());
    for bad in [
        "https://user:pass@ntfy.example.com",
        "https://ntfy.sh/?auth=x",
        "https://ntfy.sh/#x",
    ] {
        assert!(check_ntfy_server(bad).is_err(), "{bad}");
    }
}

#[test]
fn a_full_queue_drops_its_oldest_alerts_visibly_per_destination() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();
    let mut store = MonitorStore::load(dir.path().join("monitoring.json"));
    let max = crate::monitoring::MAX_OUTBOUND_PER_DESTINATION;
    for n in 0..max + 5 {
        store
            .record_external(
                "inv:studio",
                "studio",
                FleetEventKind::OfflineAlert,
                format!("Alert {n}."),
                true,
                false,
                &ids(&["slack"]),
                now,
            )
            .unwrap();
    }
    // Another destination's queue is unaffected.
    store
        .record_external(
            "inv:studio",
            "studio",
            FleetEventKind::OfflineAlert,
            "Last.".into(),
            true,
            false,
            &ids(&["ntfy"]),
            now,
        )
        .unwrap();
    let d = |id: &str| AlertDestination {
        id: id.into(),
        name: id.into(),
        kind: AlertDestinationKind::Slack,
        enabled: true,
        ntfy_server: None,
        ntfy_topic: None,
        summary: String::new(),
    };
    let slack = store.destination_view(&d("slack"));
    assert_eq!(slack.pending as usize, max);
    assert!(slack.error.unwrap().starts_with("Dropped 1 undelivered alert(s)"));
    assert_eq!(store.destination_view(&d("ntfy")).pending, 1);
    let dropped = store
        .events(Some("inv:studio"), None, now)
        .into_iter()
        .filter(|e| e.kind == FleetEventKind::AlertsDropped)
        .count();
    assert_eq!(dropped, 5, "every drop is recorded in history");
    // The oldest went first: the earliest alert still queued is the sixth.
    let first = store.claim_outbound(now, 1).unwrap();
    assert_eq!(first[0].event.detail, "Alert 5.");
}

#[test]
fn retries_back_off_up_to_an_hour() {
    let minutes: Vec<i64> = (1..=8).map(|n| retry_after(n).num_minutes()).collect();
    assert_eq!(minutes, [1, 2, 5, 15, 30, 60, 60, 60]);
}

fn ids(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(ToString::to_string).collect()
}

#[test]
fn outbox_is_saved_before_sending_and_not_duplicated_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("monitoring.json");
    let now = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();
    let mut store = MonitorStore::load(path.clone());
    store
        .record_external(
            "inv:studio",
            "studio",
            FleetEventKind::WatchdogGaveUp,
            "Stopped restarting studio-1.".into(),
            true,
            false,
            &ids(&["slack", "ntfy"]),
            now,
        )
        .unwrap();
    // Not notifying records history only.
    store
        .record_external(
            "inv:studio",
            "studio",
            FleetEventKind::DrainRequested,
            "Draining studio-1.".into(),
            false,
            false,
            &ids(&["slack"]),
            now,
        )
        .unwrap();

    let claimed = store.claim_outbound(now, 20).unwrap();
    assert_eq!(claimed.len(), 2, "one delivery per destination");
    // A restart while sending: the claim was saved, so nothing is due again yet.
    let mut store = MonitorStore::load(path.clone());
    assert!(
        store
            .claim_outbound(now + Duration::seconds(30), 20)
            .unwrap()
            .is_empty()
    );
    let slack = claimed.iter().find(|d| d.destination == "slack").unwrap().clone();
    let ntfy = claimed.iter().find(|d| d.destination == "ntfy").unwrap().clone();
    store
        .finish_outbound(
            &[
                (slack, Ok(())),
                (ntfy, Err("The destination answered HTTP 500.".into())),
            ],
            now,
        )
        .unwrap();

    let mut store = MonitorStore::load(path);
    assert!(
        store
            .claim_outbound(now + Duration::seconds(59), 20)
            .unwrap()
            .is_empty()
    );
    let retry = store.claim_outbound(now + Duration::minutes(2), 20).unwrap();
    assert_eq!(retry.len(), 1);
    assert_eq!((retry[0].destination.as_str(), retry[0].attempts), ("ntfy", 1));
    let d = |id: &str| AlertDestination {
        id: id.into(),
        name: id.into(),
        kind: AlertDestinationKind::Slack,
        enabled: true,
        ntfy_server: None,
        ntfy_topic: None,
        summary: String::new(),
    };
    let slack_view = store.destination_view(&d("slack"));
    assert_eq!((slack_view.pending, slack_view.error), (0, None));
    let ntfy_view = store.destination_view(&d("ntfy"));
    assert_eq!(ntfy_view.pending, 1);
    assert_eq!(ntfy_view.error.as_deref(), Some("The destination answered HTTP 500."));
    // Removing a destination drops its queue.
    store.discard_outbound("ntfy").unwrap();
    assert_eq!(store.destination_view(&d("ntfy")).pending, 0);
}

#[test]
fn undelivered_alerts_are_dropped_after_a_day() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();
    let mut store = MonitorStore::load(dir.path().join("monitoring.json"));
    store
        .record_external(
            "inv:studio",
            "studio",
            FleetEventKind::OfflineAlert,
            "Offline.".into(),
            true,
            false,
            &ids(&["slack"]),
            now,
        )
        .unwrap();
    let later = now + Duration::hours(25);
    let claimed = store.claim_outbound(later, 20).unwrap();
    store
        .finish_outbound(&[(claimed[0].clone(), Err("timeout".into()))], later)
        .unwrap();
    assert!(store.claim_outbound(later + Duration::hours(2), 20).unwrap().is_empty());
    let view = store.destination_view(&AlertDestination {
        id: "slack".into(),
        name: "slack".into(),
        kind: AlertDestinationKind::Slack,
        enabled: true,
        ntfy_server: None,
        ntfy_topic: None,
        summary: String::new(),
    });
    assert!(view.error.unwrap().starts_with("Gave up on an alert after a day"));
}
