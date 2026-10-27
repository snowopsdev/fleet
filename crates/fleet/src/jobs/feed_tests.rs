use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::*;

/// A forwarder that plays a script and then keeps running until terminated.
struct FakeProcess {
    events: UnboundedReceiver<ProcessEvent>,
    terminated: Arc<AtomicUsize>,
}

impl ForwardProcess for FakeProcess {
    async fn next_event(&mut self) -> ProcessEvent {
        self.events.recv().await.unwrap_or(ProcessEvent::Exited(None))
    }
    fn terminate(&mut self) {
        self.terminated.fetch_add(1, Ordering::SeqCst);
    }
    fn group(&self) -> Option<u32> {
        None
    }
}

#[derive(Default)]
struct FakeLauncher {
    scripts: Mutex<VecDeque<std::io::Result<Vec<ProcessEvent>>>>,
    launches: Mutex<Vec<Vec<String>>>,
    running: Mutex<Vec<UnboundedSender<ProcessEvent>>>,
    terminated: Arc<AtomicUsize>,
}

impl FakeLauncher {
    fn with(scripts: Vec<std::io::Result<Vec<ProcessEvent>>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
            ..Default::default()
        }
    }
    fn launches(&self) -> usize {
        self.launches.locked().len()
    }
}

impl Launch for FakeLauncher {
    type Process = FakeProcess;
    fn launch(&self, spec: &ForwardSpec) -> std::io::Result<FakeProcess> {
        self.launches.locked().push(spec.args());
        let script = self.scripts.locked().pop_front().unwrap_or_else(|| Ok(Vec::new()))?;
        let (tx, events) = unbounded_channel();
        for event in script {
            tx.send(event).unwrap();
        }
        self.running.locked().push(tx);
        Ok(FakeProcess {
            events,
            terminated: self.terminated.clone(),
        })
    }
}

fn spec() -> ForwardSpec {
    ForwardSpec {
        program: "/fake/gh".into(),
        org: "Acme".into(),
        host: "github.com".into(),
        url: "http://127.0.0.1:5555/github/workflow-job".into(),
        secret: "s3cr3t".into(),
    }
}

fn quick() -> Backoff {
    Backoff {
        first: Duration::from_millis(5),
        max: Duration::from_millis(20),
        healthy_after: Duration::from_secs(60),
        after_healthy: Duration::from_millis(1),
        unavailable: Duration::from_secs(3600),
    }
}

fn line(text: &str) -> ProcessEvent {
    ProcessEvent::Line(text.into())
}

async fn until(what: &str, check: impl Fn() -> bool) {
    for _ in 0..400 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

fn state(registry: &FeedRegistry) -> Option<FleetJobFeedStatus> {
    registry.statuses().into_iter().next()
}

#[test]
fn forward_arguments_name_the_org_event_url_and_secret() {
    assert_eq!(
        spec().args(),
        [
            "webhook",
            "forward",
            "--org=Acme",
            "--events=workflow_job",
            "--url=http://127.0.0.1:5555/github/workflow-job",
            "--secret=s3cr3t"
        ]
    );
    let ghes = ForwardSpec {
        host: "ghe.example".into(),
        ..spec()
    };
    assert_eq!(ghes.args().last().unwrap(), "--github-host=ghe.example");
}

#[test]
fn forwarder_output_explains_what_to_fix() {
    assert_eq!(
        classify("Forwarding Webhook events from GitHub...", "github.com"),
        Some(Signal::Forwarding)
    );
    let missing = classify("unknown command \"webhook\" for \"gh\"", "github.com");
    assert!(matches!(missing, Some(Signal::Unavailable { fix: Some(ref f), .. }) if f == EXTENSION_INSTALL));
    let scope = classify("Error: you do not have access to this feature", "github.com");
    assert!(
        matches!(scope, Some(Signal::Unavailable { fix: Some(ref f), .. }) if f == "gh auth refresh --hostname github.com --scopes admin:org_hook")
    );
    assert!(matches!(
        classify(
            "error creating webhook: HTTP 422: Validation Failed (Hook already exists)",
            "github.com"
        ),
        Some(Signal::Unavailable { fix: None, .. })
    ));
    assert_eq!(classify("warning: error forwarding event: EOF", "github.com"), None);
}

#[test]
fn restart_delays_double_up_to_the_cap() {
    let b = Backoff::default();
    assert_eq!(
        [1, 2, 3, 7, 30].map(|n| b.delay(n)),
        [5, 10, 20, 300, 300].map(Duration::from_secs)
    );
}

#[tokio::test]
async fn a_connected_forwarder_is_forwarding_until_stopped_and_then_terminated() {
    let launcher = Arc::new(FakeLauncher::with(vec![Ok(vec![line(
        "Forwarding Webhook events from GitHub...",
    )])]));
    let registry = Arc::new(FeedRegistry::default());
    let (stop, rx) = watch::channel(false);
    let task = {
        let (launcher, registry) = (launcher.clone(), registry.clone());
        tokio::spawn(async move { supervise(&*launcher, spec(), &registry, rx, quick()).await })
    };
    until("forwarding", || registry.forwarding("acme")).await;
    assert_eq!(state(&registry).unwrap().org, "acme");
    stop.send(true).unwrap();
    task.await.unwrap();
    assert_eq!(launcher.terminated.load(Ordering::SeqCst), 1);
    assert!(registry.statuses().is_empty());
    assert_eq!(launcher.launches(), 1);
}

#[tokio::test]
async fn an_exited_forwarder_restarts_with_backoff_and_keeps_its_last_words() {
    let launcher = Arc::new(FakeLauncher::with(vec![
        Ok(vec![
            line("error dialing to ws server: code: 502 s3cr3t"),
            ProcessEvent::Exited(Some(1)),
        ]),
        Ok(vec![line("Forwarding Webhook events from GitHub...")]),
    ]));
    let registry = Arc::new(FeedRegistry::default());
    let (stop, rx) = watch::channel(false);
    let task = {
        let (launcher, registry) = (launcher.clone(), registry.clone());
        tokio::spawn(async move { supervise(&*launcher, spec(), &registry, rx, quick()).await })
    };
    until("second launch connected", || registry.forwarding("acme")).await;
    let status = state(&registry).unwrap();
    assert_eq!(status.restarts, 1);
    assert_eq!(launcher.launches(), 2);
    assert_eq!(status.detail, None);
    stop.send(true).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn a_missing_extension_is_unavailable_with_the_install_command() {
    let launcher = Arc::new(FakeLauncher::with(vec![Ok(vec![
        line("unknown command \"webhook\" for \"gh\""),
        ProcessEvent::Exited(Some(1)),
    ])]));
    let registry = Arc::new(FeedRegistry::default());
    let (stop, rx) = watch::channel(false);
    let task = {
        let (launcher, registry) = (launcher.clone(), registry.clone());
        tokio::spawn(async move { supervise(&*launcher, spec(), &registry, rx, quick()).await })
    };
    until("unavailable", || {
        state(&registry).is_some_and(|s| s.state == FleetJobFeedState::Unavailable)
    })
    .await;
    let status = state(&registry).unwrap();
    assert_eq!(status.fix.as_deref(), Some(EXTENSION_INSTALL));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(launcher.launches(), 1, "unavailable feeds retry only occasionally");
    stop.send(true).unwrap();
    task.await.unwrap();
    assert!(registry.statuses().is_empty());
}

#[tokio::test]
async fn a_missing_gh_is_unavailable() {
    let launcher = FakeLauncher::with(vec![Err(std::io::ErrorKind::NotFound.into())]);
    let registry = FeedRegistry::default();
    let (stop, rx) = watch::channel(false);
    let run = supervise(&launcher, spec(), &registry, rx, quick());
    let check = async {
        until("unavailable", || {
            state(&registry).is_some_and(|s| s.state == FleetJobFeedState::Unavailable)
        })
        .await;
        assert_eq!(
            state(&registry).unwrap().detail.as_deref(),
            Some("The GitHub CLI (gh) isn't installed.")
        );
        stop.send(true).unwrap();
    };
    tokio::join!(run, check);
}

#[tokio::test]
async fn stopping_the_registry_ends_supervision() {
    let launcher = FakeLauncher::with(vec![Ok(vec![ProcessEvent::Exited(Some(1))])]);
    let registry = FeedRegistry::default();
    registry.stop_all();
    let (_stop, rx) = watch::channel(false);
    supervise(&launcher, spec(), &registry, rx, quick()).await;
    assert_eq!(launcher.launches(), 0);
}
