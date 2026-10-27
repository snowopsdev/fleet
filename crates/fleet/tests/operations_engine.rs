//! Maintenance changes, drains and alert destinations through the engine.
//! GitHub, the hosts and webhook endpoints are faked; nothing here contacts a
//! real machine, organization or notification service.

#![expect(clippy::unwrap_used, reason = "test fixtures fail the test loudly")]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::routing::post;
use fleet::automation::{ActionsVariables, FleetDrainState, PublishSettings};
use fleet::desired::maintenance::{CleanupKind, CleanupOptions, CleanupTarget};
use fleet::desired::model::{FleetPlanAction, GithubRunner, LocalRunner};
use fleet::desired::testing::{FakeFleet, no_sleep};
use fleet::engine::desired::{ChangeSecrets, DesiredIo, SshTransportIdentity};
use fleet::engine::{Engine, MemorySecrets, SecretStore};
use fleet::fleet_plan::{ChangeRequest, OperationStatus, OperationView};
use fleet::monitoring::FleetEventKind;
use fleet::outbound::{AlertDestinationInput, AlertDestinationKind};
use fleet::sync::MutexExt;

const INVENTORY: &str = r#"
[organization]
name = "acme"
url = "https://github.com/acme"
runner_group = "trusted"

[[hosts]]
id = "mini"
platform = "darwin-arm64"
ssh = "mini"
root = "~/gh-runners/acme"
service_mode = "launchagent"
labels = ["fleet", "macos-build"]
runners = [
  { name = "mini-01", directory = "runner-01" },
  { name = "mini-02", directory = "runner-02" },
]
"#;

struct Fixture {
    tmp: tempfile::TempDir,
    engine: Arc<Engine>,
    fleet: FakeFleet,
    secrets: Arc<MemorySecrets>,
}

#[derive(Default)]
struct RecordedVariables(Mutex<Vec<String>>);

impl ActionsVariables for RecordedVariables {
    fn write(&self, organization: &str, name: &str, value: &str) -> Result<(), String> {
        self.0.locked().push(format!("{organization}/{name}={value}"));
        Ok(())
    }
}

fn fixture() -> Fixture {
    fixture_with(None)
}

/// A fixture whose `automation.json` already holds `automation` when the app starts.
fn fixture_with(automation: Option<serde_json::Value>) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    if let Some(journal) = automation {
        std::fs::write(tmp.path().join("automation.json"), journal.to_string()).unwrap();
    }
    let path = tmp.path().join("fleet.toml");
    std::fs::write(&path, INVENTORY).unwrap();
    let fleet = FakeFleet::default();
    for (id, name) in [(1, "mini-01"), (2, "mini-02")] {
        fleet.world().github.insert(
            name.into(),
            GithubRunner::new(id, name, "online", false, &["fleet", "macos-build"], Some(true)),
        );
        fleet.world().local.insert(
            name.into(),
            LocalRunner::new(
                name,
                &format!("~/gh-runners/acme/runner-0{id}"),
                true,
                "active",
                Some("2.329.0"),
            ),
        );
    }
    let boundaries = fleet.clone();
    let secrets = Arc::new(MemorySecrets::default());
    let engine = Engine::load(tmp.path().join("config.json"), secrets.clone())
        .unwrap()
        .with_desired_io(DesiredIo {
            boundaries: Arc::new(move |_| (Arc::new(boundaries.clone()) as _, Arc::new(boundaries.clone()) as _)),
            sleep: no_sleep,
            resolve_ssh_transport: Arc::new(|_: &str| {
                SshTransportIdentity::from_config(
                    "hostname mac-mini.example.ts.net\nport 22\nuser fake-ci\nproxycommand none\nproxyjump none\n",
                )
            }),
        })
        .with_actions_variables(Arc::new(RecordedVariables::default()));
    engine.set_inventory_path(Some(path.display().to_string())).unwrap();
    Fixture {
        tmp,
        engine: Arc::new(engine),
        fleet,
        secrets,
    }
}

async fn wait_done(engine: &Engine) -> OperationView {
    for _ in 0..300 {
        let plan = engine.view().plan;
        if let Some(op) = plan.operation.filter(|o| o.status != OperationStatus::Running)
            && !plan.refreshing
        {
            return op;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("operation didn't finish");
}

/// A fresh check of `mini`, after any check still running from a change.
async fn check_again(engine: &Arc<Engine>) {
    let before = engine.view().plan.checked_at;
    for _ in 0..300 {
        engine.desired_refresh(Some("mini".into())).await.unwrap();
        if engine.view().plan.checked_at > before {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the check didn't run");
}

fn target(kind: CleanupKind, runner: Option<&str>, path: Option<&str>, kib: u64) -> CleanupTarget {
    CleanupTarget {
        kind,
        runner: runner.map(String::from),
        path: path.map(String::from),
        kib: Some(kib),
        count: None,
        detail: None,
        files: Vec::new(),
    }
}

fn history(engine: &Engine) -> Vec<(FleetEventKind, String)> {
    engine
        .monitoring_events(Some("inv:mini"), None)
        .into_iter()
        .map(|e| (e.kind, e.detail))
        .collect()
}

#[tokio::test]
async fn cleanup_review_shows_sizes_and_runs_only_idle_targets() {
    let f = fixture();
    f.fleet.world().cleanup_targets = vec![
        target(
            CleanupKind::WorkTemp,
            Some("mini-01"),
            Some("/Users/ci/gh-runners/acme/runner-01/_work/_temp"),
            2_097_152,
        ),
        target(
            CleanupKind::WorkTemp,
            Some("mini-02"),
            Some("/Users/ci/gh-runners/acme/runner-02/_work/_temp"),
            1024,
        ),
        target(
            CleanupKind::XcodeDerivedData,
            None,
            Some("/Users/ci/Library/Developer/Xcode/DerivedData"),
            9,
        ),
    ];
    f.fleet.world().github.get_mut("mini-02").unwrap().busy = true;
    let request = ChangeRequest::Cleanup {
        host: "mini".into(),
        options: CleanupOptions::default(),
    };
    let review = f.engine.desired_review(request.clone()).await.unwrap();
    assert!(review.ready, "{:?}", review.blockers);
    assert_eq!(review.cleanup.len(), 3);
    assert_eq!(review.cleanup[0].kib, Some(2_097_152), "sizes are shown in the review");
    let blocked: Vec<(&str, bool)> = review.items.iter().map(|i| (i.runner.as_str(), i.blocked)).collect();
    assert_eq!(
        blocked,
        [("mini-01", false), ("mini-02", true), ("machine", true)],
        "a busy runner's directories and the shared caches wait"
    );
    assert!(review.items.iter().all(|i| i.action == FleetPlanAction::Cleanup));
    assert!(
        review.items[0]
            .reason
            .contains("/Users/ci/gh-runners/acme/runner-01/_work/_temp")
    );
    assert!(
        !review.items[0].reason.contains("GiB"),
        "sizes aren't part of what's compared"
    );
    assert!(f.fleet.world().calls.is_empty(), "reviewing is read-only");

    f.engine
        .desired_execute(&review.id, ChangeSecrets::default(), None)
        .await
        .unwrap();
    let op = wait_done(&f.engine).await;
    assert_eq!(op.status, OperationStatus::Succeeded, "{:?}", op.error);
    // mini-01 is stopped for the removal and started again; busy mini-02 is never touched.
    assert_eq!(
        f.fleet.world().calls,
        [
            "stop mini mini-01",
            "cleanup mini work-temp /Users/ci/gh-runners/acme/runner-01/_work/_temp",
            "start mini mini-01"
        ]
    );
}

#[tokio::test]
async fn cleanup_refuses_when_the_runner_became_busy_since_review() {
    let f = fixture();
    f.fleet.world().cleanup_targets = vec![target(
        CleanupKind::WorkActions,
        Some("mini-01"),
        Some("/Users/ci/gh-runners/acme/runner-01/_work/_actions"),
        10,
    )];
    let review = f
        .engine
        .desired_review(ChangeRequest::Cleanup {
            host: "mini".into(),
            options: CleanupOptions::default(),
        })
        .await
        .unwrap();
    assert!(review.ready);
    f.fleet.world().github.get_mut("mini-01").unwrap().busy = true;
    let e = f
        .engine
        .desired_execute(&review.id, ChangeSecrets::default(), None)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("changed since you reviewed it"), "{e}");
    assert!(f.fleet.world().calls.is_empty());
}

#[tokio::test]
async fn nothing_to_clean_is_not_runnable() {
    let f = fixture();
    let review = f
        .engine
        .desired_review(ChangeRequest::Cleanup {
            host: "mini".into(),
            options: CleanupOptions::default(),
        })
        .await
        .unwrap();
    assert!(!review.ready);
    assert!(
        review.blockers[0].starts_with("Nothing to clean up"),
        "{:?}",
        review.blockers
    );
}

#[tokio::test]
async fn upgrade_all_idle_runners_one_at_a_time() {
    let f = fixture();
    f.fleet.world().latest_version = "2.330.0".into();
    f.fleet.world().github.get_mut("mini-02").unwrap().busy = true;
    let review = f
        .engine
        .desired_review(ChangeRequest::Upgrade {
            host: "mini".into(),
            runner: None,
        })
        .await
        .unwrap();
    assert!(review.ready, "{:?}", review.blockers);
    assert_eq!(review.upgrade_to.as_deref(), Some("2.330.0"));
    let items: Vec<(&str, bool)> = review.items.iter().map(|i| (i.runner.as_str(), i.blocked)).collect();
    assert_eq!(
        items,
        [("mini-01", false), ("mini-02", true)],
        "a busy runner is never upgraded"
    );
    assert!(review.items[0].reason.starts_with("upgrade from 2.329.0 to 2.330.0"));

    f.engine
        .desired_execute(&review.id, ChangeSecrets::default(), None)
        .await
        .unwrap();
    let op = wait_done(&f.engine).await;
    assert_eq!(op.status, OperationStatus::Succeeded, "{:?}", op.error);
    assert_eq!(op.messages, ["mini-01 upgraded to 2.330.0 and is online"]);
    assert_eq!(
        f.fleet.world().calls,
        [
            "stop mini mini-01",
            "upgrade mini mini-01 2.330.0",
            "start mini mini-01"
        ]
    );
}

#[tokio::test]
async fn upgrade_refuses_when_a_newer_release_appears_after_review() {
    let f = fixture();
    f.fleet.world().latest_version = "2.330.0".into();
    let review = f
        .engine
        .desired_review(ChangeRequest::Upgrade {
            host: "mini".into(),
            runner: Some("mini-01".into()),
        })
        .await
        .unwrap();
    assert!(review.ready, "{:?}", review.blockers);
    f.fleet.world().latest_version = "2.331.0".into();
    let e = f
        .engine
        .desired_execute(&review.id, ChangeSecrets::default(), None)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("changed since you reviewed it"), "{e}");
    assert!(f.fleet.world().calls.is_empty());

    f.fleet.world().latest_version = "2.329.0".into();
    let review = f
        .engine
        .desired_review(ChangeRequest::Upgrade {
            host: "mini".into(),
            runner: Some("mini-01".into()),
        })
        .await
        .unwrap();
    assert!(!review.ready);
    assert!(
        review.blockers[0].starts_with("Nothing to upgrade"),
        "{:?}",
        review.blockers
    );
}

async fn drain(f: &Fixture, runner: &str) {
    let review = f
        .engine
        .desired_review(ChangeRequest::Drain { runner: runner.into() })
        .await
        .unwrap();
    assert!(review.ready, "{:?}", review.blockers);
    assert!(review.items[0].reason.contains("GitHub has no way to pause a runner"));
    f.engine
        .desired_execute(&review.id, ChangeSecrets::default(), None)
        .await
        .unwrap();
    let op = wait_done(&f.engine).await;
    assert_eq!(op.status, OperationStatus::Succeeded, "{:?}", op.error);
}

#[tokio::test]
async fn a_busy_runner_drains_after_its_job_and_stays_stopped_until_resumed() {
    let f = fixture();
    f.fleet.world().github.get_mut("mini-01").unwrap().busy = true;
    drain(&f, "mini-01").await;
    let drains = f.engine.view().automation.drains;
    assert_eq!(drains.len(), 1);
    assert_eq!(drains[0].state, FleetDrainState::Draining);
    assert!(f.fleet.world().calls.is_empty(), "nothing stops while the job runs");

    // Still busy: the deferred stop waits.
    f.engine.run_automation().await;
    assert!(f.fleet.world().calls.is_empty());

    // The job finished; the next GitHub read shows it idle, and the reviewed drain stops it.
    f.fleet.world().github.get_mut("mini-01").unwrap().busy = false;
    check_again(&f.engine).await;
    f.engine.run_automation().await;
    assert_eq!(f.fleet.world().calls, ["stop mini mini-01"]);
    assert_eq!(f.engine.view().automation.drains[0].state, FleetDrainState::Stopped);
    let kinds: Vec<FleetEventKind> = history(&f.engine).into_iter().map(|(k, _)| k).collect();
    assert!(kinds.contains(&FleetEventKind::DrainRequested));
    assert!(kinds.contains(&FleetEventKind::Drained));

    // A second drain is refused while one holds it; resume starts it again.
    let again = f
        .engine
        .desired_review(ChangeRequest::Drain {
            runner: "mini-01".into(),
        })
        .await
        .unwrap();
    assert!(!again.ready);
    let review = f
        .engine
        .desired_review(ChangeRequest::Resume {
            runner: "mini-01".into(),
        })
        .await
        .unwrap();
    assert!(review.ready, "{:?}", review.blockers);
    f.engine
        .desired_execute(&review.id, ChangeSecrets::default(), None)
        .await
        .unwrap();
    wait_done(&f.engine).await;
    assert_eq!(f.fleet.world().calls, ["stop mini mini-01", "start mini mini-01"]);
    assert!(f.engine.view().automation.drains.is_empty());
}

#[tokio::test]
async fn an_idle_runner_drains_at_once_and_a_cancelled_drain_never_stops() {
    let f = fixture();
    drain(&f, "mini-01").await;
    assert_eq!(f.fleet.world().calls, ["stop mini mini-01"]);
    assert_eq!(f.engine.view().automation.drains[0].state, FleetDrainState::Stopped);
    assert!(
        f.engine.cancel_drain("mini-01").is_err(),
        "a stopped runner is resumed, not cancelled"
    );

    f.fleet.world().github.get_mut("mini-02").unwrap().busy = true;
    drain(&f, "mini-02").await;
    f.engine.cancel_drain("mini-02").unwrap();
    f.fleet.world().github.get_mut("mini-02").unwrap().busy = false;
    check_again(&f.engine).await;
    f.engine.run_automation().await;
    assert_eq!(
        f.fleet.world().calls,
        ["stop mini mini-01"],
        "the cancelled drain did nothing"
    );
    assert!(
        history(&f.engine)
            .iter()
            .any(|(k, d)| *k == FleetEventKind::DrainCancelled && d.contains("mini-02"))
    );
}

#[tokio::test]
async fn a_drain_is_cancelled_when_the_runner_is_registered_again() {
    let f = fixture();
    f.fleet.world().github.get_mut("mini-01").unwrap().busy = true;
    drain(&f, "mini-01").await;
    // Re-registered with a new id, and idle.
    f.fleet.world().github.insert(
        "mini-01".into(),
        GithubRunner::new(77, "mini-01", "online", false, &[], Some(true)),
    );
    check_again(&f.engine).await;
    f.engine.run_automation().await;
    assert!(f.fleet.world().calls.is_empty());
    assert!(f.engine.view().automation.drains.is_empty());
}

#[tokio::test]
async fn demo_reviews_are_fictional_and_never_run() {
    let f = fixture();
    f.engine.set_demo_mode(true).unwrap();
    let review = f
        .engine
        .desired_review(ChangeRequest::Cleanup {
            host: "studio".into(),
            options: CleanupOptions::default(),
        })
        .await
        .unwrap();
    assert!(!review.ready);
    assert!(review.blockers.iter().any(|b| b.starts_with("Demo data")));
    assert!(review.cleanup.iter().any(|t| t.kib.is_some()));
    let e = f
        .engine
        .desired_execute(&review.id, ChangeSecrets::default(), None)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("demo data"), "{e}");
    let upgrade = f
        .engine
        .desired_review(ChangeRequest::Upgrade {
            host: "build-linux-01".into(),
            runner: None,
        })
        .await
        .unwrap();
    assert_eq!(upgrade.upgrade_to.as_deref(), Some("2.329.0"));
    assert_eq!(upgrade.items[0].runner, "linux-01-a");
    assert!(f.engine.set_watchdog(true).is_err());
    f.engine.run_automation().await;
    assert!(f.fleet.world().calls.is_empty());
    assert!(f.fleet.world().reads.is_empty(), "demo reviews never read a host");
}

#[tokio::test]
async fn destination_secrets_live_in_the_secret_store_and_tests_reach_the_endpoint() {
    let f = fixture();
    let hits: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = hits.clone();
    let base = common::serve(axum::Router::new().route(
        "/services/{*rest}",
        post(move |body: String| {
            let log = log.clone();
            async move {
                log.locked().push(body);
                "ok"
            }
        }),
    ))
    .await;
    let url = format!("{base}/services/T0/B0/very-secret");
    let id = f
        .engine
        .save_alert_destination(AlertDestinationInput {
            name: "#ci-alerts".into(),
            kind: Some(AlertDestinationKind::Slack),
            enabled: true,
            url: Some(url.clone()),
            ..AlertDestinationInput::default()
        })
        .await
        .unwrap();
    let config = std::fs::read_to_string(f.tmp.path().join("config.json")).unwrap();
    assert!(!config.contains("very-secret"), "secrets never reach the settings file");
    let stored = f.secrets.get(&format!("alert-destination:{id}")).unwrap().unwrap();
    assert!(stored.contains("very-secret"));
    let view = f.engine.view();
    let shown = serde_json::to_string(&view.automation.destinations).unwrap();
    assert!(!shown.contains("very-secret"), "the view shows only the host");
    assert_eq!(view.automation.destinations[0].summary, "127.0.0.1");

    f.engine.test_alert_destination(&id).await.unwrap();
    assert!(hits.locked()[0].contains("Runner Dashboard test"));

    // Editing without a URL keeps the saved one.
    f.engine
        .save_alert_destination(AlertDestinationInput {
            id: Some(id.clone()),
            name: "#ops".into(),
            enabled: false,
            ..AlertDestinationInput::default()
        })
        .await
        .unwrap();
    assert!(
        f.secrets
            .get(&format!("alert-destination:{id}"))
            .unwrap()
            .unwrap()
            .contains("very-secret")
    );
    f.engine.remove_alert_destination(&id).await.unwrap();
    assert_eq!(f.secrets.get(&format!("alert-destination:{id}")).unwrap(), None);
    assert!(f.engine.view().automation.destinations.is_empty());

    let bad = f
        .engine
        .save_alert_destination(AlertDestinationInput {
            name: "plain".into(),
            kind: Some(AlertDestinationKind::Webhook),
            url: Some("http://hooks.example.com/x".into()),
            ..AlertDestinationInput::default()
        })
        .await
        .unwrap_err();
    assert!(bad.to_string().contains("https://"), "{bad}");
}

#[tokio::test]
async fn publishing_is_off_by_default_and_validates_its_settings() {
    let f = fixture();
    f.engine.run_automation().await;
    assert!(!f.engine.view().automation.publish.enabled);
    let e = f
        .engine
        .set_publish_settings(PublishSettings {
            enabled: true,
            variable: "GITHUB_FLEET".into(),
            interval_minutes: 5,
        })
        .unwrap_err();
    assert!(e.to_string().contains("GITHUB_"), "{e}");
    f.engine
        .set_publish_settings(PublishSettings {
            enabled: true,
            variable: "RUNNER_FLEET_STATUS".into(),
            interval_minutes: 5,
        })
        .unwrap();
    // No organization is monitored in this fixture, so there's nothing trustworthy to write.
    f.engine.run_automation().await;
    let status = f.engine.view().automation.publish;
    assert!(status.value.is_none());
    assert!(status.waiting.unwrap().contains("Settings ▸ GitHub"));
}

#[tokio::test]
async fn a_drained_runner_is_not_upgraded_restarted_or_drained_again() {
    let f = fixture();
    f.fleet.world().latest_version = "2.330.0".into();
    drain(&f, "mini-01").await;
    let one = f
        .engine
        .desired_review(ChangeRequest::Upgrade {
            host: "mini".into(),
            runner: Some("mini-01".into()),
        })
        .await
        .unwrap();
    assert!(!one.ready);
    assert!(
        one.blockers.iter().any(|b| b.contains("held by a drain")),
        "{:?}",
        one.blockers
    );
    let all = f
        .engine
        .desired_review(ChangeRequest::Upgrade {
            host: "mini".into(),
            runner: None,
        })
        .await
        .unwrap();
    assert!(all.ready, "{:?}", all.blockers);
    let items: Vec<(&str, bool)> = all.items.iter().map(|i| (i.runner.as_str(), i.blocked)).collect();
    assert_eq!(items, [("mini-01", true), ("mini-02", false)]);
    let restart = f
        .engine
        .desired_review(ChangeRequest::Restart {
            runner: "mini-01".into(),
        })
        .await
        .unwrap();
    assert!(!restart.ready);
    let apply = f
        .engine
        .desired_review(ChangeRequest::Apply {
            host: "mini".into(),
            runner: Some("mini-01".into()),
        })
        .await
        .unwrap();
    assert!(
        apply.blockers.iter().any(|b| b.contains("held by a drain")),
        "apply would start the drained service: {:?}",
        apply.blockers
    );
}

#[tokio::test]
async fn a_second_drain_reviewed_before_the_first_ran_is_refused() {
    let f = fixture();
    f.fleet.world().github.get_mut("mini-01").unwrap().busy = true;
    let first = f
        .engine
        .desired_review(ChangeRequest::Drain {
            runner: "mini-01".into(),
        })
        .await
        .unwrap();
    let second = f
        .engine
        .desired_review(ChangeRequest::Drain {
            runner: "mini-01".into(),
        })
        .await
        .unwrap();
    assert!(first.ready && second.ready);
    f.engine
        .desired_execute(&first.id, ChangeSecrets::default(), None)
        .await
        .unwrap();
    wait_done(&f.engine).await;
    let e = f
        .engine
        .desired_execute(&second.id, ChangeSecrets::default(), None)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("changed since you reviewed it"), "{e}");
}

#[tokio::test]
async fn a_failed_upgrade_holds_the_runner_until_a_reviewed_resume() {
    let f = fixture();
    f.fleet.world().latest_version = "2.330.0".into();
    f.fleet.world().upgrade_failures.push("mini-01".into());
    let review = f
        .engine
        .desired_review(ChangeRequest::Upgrade {
            host: "mini".into(),
            runner: Some("mini-01".into()),
        })
        .await
        .unwrap();
    f.engine
        .desired_execute(&review.id, ChangeSecrets::default(), None)
        .await
        .unwrap();
    let op = wait_done(&f.engine).await;
    assert_eq!(op.status, OperationStatus::Failed);
    let holds = f.engine.view().automation.holds;
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].runner, "mini-01");
    assert!(
        history(&f.engine)
            .iter()
            .any(|(k, _)| *k == FleetEventKind::HeldStopped)
    );

    let resume = f
        .engine
        .desired_review(ChangeRequest::Resume {
            runner: "mini-01".into(),
        })
        .await
        .unwrap();
    assert!(resume.ready, "{:?}", resume.blockers);
    f.engine
        .desired_execute(&resume.id, ChangeSecrets::default(), None)
        .await
        .unwrap();
    wait_done(&f.engine).await;
    assert!(f.engine.view().automation.holds.is_empty());
}

#[tokio::test]
async fn a_drain_that_never_finds_its_runner_idle_expires() {
    let requested = chrono::Utc::now() - chrono::Duration::hours(25);
    let f = fixture_with(Some(serde_json::json!({
        "version": 1,
        "drains": { "mini-01": {
            "runner": "mini-01", "host": "mini", "path": "~/gh-runners/acme/runner-01",
            "registration": 1, "review": "review_mini_0", "requestedAt": requested,
            "state": "draining", "stoppedAt": null, "lastError": null, "checkedAt": null,
        }},
    })));
    assert_eq!(f.engine.view().automation.drains.len(), 1);
    f.engine.run_automation().await;
    assert!(f.engine.view().automation.drains.is_empty());
    assert!(f.fleet.world().calls.is_empty(), "an expired drain stops nothing");
    assert!(
        history(&f.engine)
            .iter()
            .any(|(k, d)| *k == FleetEventKind::DrainCancelled && d.contains("24 hours"))
    );
}
