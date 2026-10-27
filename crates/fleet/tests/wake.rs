#![expect(clippy::unwrap_used, reason = "tests fail at the assertion site")]
//! Reviewed Wake-on-LAN with a fake tailnet and a fake sender; no packet is ever sent.

use std::sync::{Arc, Mutex};

use fleet::desired::model::Host;
use fleet::desired::remote::RemoteError;
use fleet::engine::desired::DesiredIo;
use fleet::engine::{Engine, MemorySecrets};
use fleet::model::{TailnetPeer, TailnetSnapshot};
use fleet::monitoring::FleetEventKind;
use fleet::wake::{MacAddress, WakeSender};

const INVENTORY: &str = r#"
[organization]
name = "acme"
url = "https://github.com/acme"
runner_group = "trusted"

[[hosts]]
id = "mini"
platform = "darwin-arm64"
ssh = "builder@mini.demo.ts.net"
root = "~/gh-runners/acme"
service_mode = "launchagent"
labels = ["fleet", "macos-build"]
mac_address = "02:00:5E:10:00:01"
wake_via = "relay"
runners = [{ name = "mini-01", directory = "runner-01" }]

[[hosts]]
id = "relay"
platform = "linux-arm64"
ssh = "builder@relay.demo.ts.net"
root = "~/gh-runners/acme"
service_mode = "systemd"
labels = ["fleet", "linux-build"]
runners = [{ name = "relay-01", directory = "runner-01" }]
"#;

#[derive(Default)]
struct FakeSender(Mutex<Vec<(String, String)>>);

impl WakeSender for FakeSender {
    fn send(&self, via: &Host, mac: &MacAddress) -> Result<String, RemoteError> {
        self.0.lock().unwrap().push((via.id.clone(), mac.to_string()));
        Ok("sent with python3".into())
    }
}

fn peer(name: &str, online: bool) -> TailnetPeer {
    TailnetPeer {
        node_id: format!("n-{name}"),
        hostname: name.into(),
        dns_name: Some(format!("{name}.demo.ts.net")),
        os: if name == "mini" { "macOS" } else { "linux" }.into(),
        ips: vec![],
        online,
        last_seen: None,
        tags: vec![],
        ssh_host_keys: vec![],
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    inventory: std::path::PathBuf,
    engine: Engine,
    sender: Arc<FakeSender>,
}

async fn fixture(mini_online: bool, relay_online: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let inventory = dir.path().join("fleet.toml");
    std::fs::write(&inventory, INVENTORY).unwrap();
    let sender = Arc::new(FakeSender::default());
    let engine = Engine::load(dir.path().join("config.json"), Arc::new(MemorySecrets::default()))
        .unwrap()
        .with_wake_sender(sender.clone())
        .with_desired_io(DesiredIo {
            boundaries: Arc::new(|_| panic!("waking must not call runner operations or GitHub")),
            sleep: |_| panic!("no operation waits"),
            resolve_ssh_transport: Arc::new(|_| None),
        })
        .with_tailscale_source(move || async move {
            Ok(TailnetSnapshot {
                peers: vec![peer("mini", mini_online), peer("relay", relay_online)],
                ..Default::default()
            })
        });
    engine
        .set_inventory_path(Some(inventory.display().to_string()))
        .unwrap();
    engine.poll_tailscale().await;
    Fixture {
        _dir: dir,
        inventory,
        engine,
        sender,
    }
}

#[tokio::test]
async fn an_offline_machine_is_woken_once_through_its_relay_and_recorded() {
    let f = fixture(false, true).await;
    let review = f.engine.wake_review("ts:n-mini").await.unwrap();
    assert_eq!(
        (
            review.host.as_str(),
            review.via.as_str(),
            review.mac_address.as_str(),
            review.broadcast.as_str(),
            review.ready
        ),
        ("mini", "relay", "02:00:5e:10:00:01", "255.255.255.255:9", true),
        "{:?}",
        review.blockers
    );
    assert!(f.sender.0.lock().unwrap().is_empty(), "a review sends nothing");
    let message = f.engine.wake_execute(&review.id).await.unwrap();
    assert!(message.contains("from relay"), "{message}");
    assert_eq!(
        *f.sender.0.lock().unwrap(),
        [("relay".to_string(), "02:00:5e:10:00:01".to_string())]
    );
    let events = f.engine.monitoring_events(Some("inv:mini"), None);
    assert_eq!(events[0].kind, FleetEventKind::WakeRequested);
    assert_eq!(events[0].detail, message);
    // Reviews are single-use.
    assert!(f.engine.wake_execute(&review.id).await.is_err());
    assert_eq!(f.sender.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn online_targets_and_offline_relays_block_the_wake() {
    let online = fixture(true, true).await;
    let review = online.engine.wake_review("ts:n-mini").await.unwrap();
    assert_eq!(
        review.blockers,
        ["mini is online on the tailnet, so there's nothing to wake."]
    );
    assert!(online.engine.wake_execute(&review.id).await.is_err());

    let relay_down = fixture(false, false).await;
    let review = relay_down.engine.wake_review("ts:n-mini").await.unwrap();
    assert_eq!(
        review.blockers,
        ["relay must be online on the tailnet to send the packet."]
    );
    assert!(!review.ready);
    assert!(relay_down.sender.0.lock().unwrap().is_empty());

    let no_wake = online.engine.wake_review("ts:n-relay").await.unwrap_err();
    assert!(
        no_wake.to_string().contains("Add mac_address and wake_via"),
        "{no_wake}"
    );
}

#[tokio::test]
async fn an_inventory_change_after_review_refuses_to_send() {
    let f = fixture(false, true).await;
    let review = f.engine.wake_review("ts:n-mini").await.unwrap();
    std::fs::write(
        &f.inventory,
        INVENTORY.replace("02:00:5E:10:00:01", "02:00:5e:10:00:02"),
    )
    .unwrap();
    let error = f.engine.wake_execute(&review.id).await.unwrap_err();
    assert!(error.to_string().contains("changed since you reviewed"), "{error}");
    assert!(f.sender.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn demo_data_never_wakes_anything() {
    let f = fixture(false, true).await;
    f.engine.set_demo_mode(true).unwrap();
    assert!(f.engine.wake_review("ts:n-mini").await.is_err());
    assert!(f.sender.0.lock().unwrap().is_empty());
}
