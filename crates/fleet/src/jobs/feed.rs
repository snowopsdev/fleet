//! Supervises one `gh webhook forward` child per organization for the optional job feed.
//!
//! `gh webhook forward --org=ORG --events=workflow_job --url=URL --secret=SECRET` (the
//! `cli/gh-webhook` extension) creates a temporary `cli` webhook on the organization, which
//! needs the `admin:org_hook` scope, and relays deliveries over a websocket to the local
//! receiver. The extension is meant for development and exits when GitHub closes the
//! websocket, so it's restarted with backoff. A child that ran healthily restarts quickly.
//! Children run in their own process group, which is terminated on stop.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use ts_rs::TS;

use crate::sync::MutexExt;

/// The event the feed asks GitHub for.
pub const FEED_EVENTS: &str = "workflow_job";
/// Installs the forwarding extension.
pub const EXTENSION_INSTALL: &str = "gh extension install cli/gh-webhook";
/// The scope `gh webhook forward --org` needs to create the organization webhook.
pub const HOOK_SCOPE: &str = "admin:org_hook";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub enum FleetJobFeedState {
    /// The forwarder is starting and hasn't connected yet.
    Starting,
    /// Deliveries are being forwarded.
    Forwarding,
    /// The forwarder exited and is restarting after a delay.
    Restarting,
    /// The forwarder can't run until something is fixed; retried occasionally.
    Unavailable,
}

/// The real-time job feed for one organization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FleetJobFeedStatus {
    pub org: String,
    pub state: FleetJobFeedState,
    /// The last thing the forwarder said, or why it isn't running.
    pub detail: Option<String>,
    /// A command that fixes an unavailable feed.
    pub fix: Option<String>,
    /// When the current state began.
    pub since: DateTime<Utc>,
    pub last_delivery_at: Option<DateTime<Utc>>,
    pub restarts: u32,
}

/// What the supervisor learns from one line of the forwarder's output.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Signal {
    Forwarding,
    Unavailable { detail: String, fix: Option<String> },
}

fn classify(line: &str, host: &str) -> Option<Signal> {
    let lower = line.to_lowercase();
    let unavailable = |detail: &str, fix: Option<String>| {
        Some(Signal::Unavailable {
            detail: detail.into(),
            fix,
        })
    };
    if lower.contains("forwarding webhook events from github") {
        Some(Signal::Forwarding)
    } else if lower.contains("unknown command") && lower.contains("webhook") {
        unavailable(
            "The gh webhook extension isn't installed.",
            Some(EXTENSION_INSTALL.into()),
        )
    } else if lower.contains("hook already exists") {
        unavailable(
            "Another webhook forwarder is already running for this organization. GitHub allows one at a time.",
            None,
        )
    } else if lower.contains("you do not have access to this feature")
        || lower.contains("admin:org_hook")
        || lower.contains("http 403")
        || lower.contains("http 404")
    {
        unavailable(
            "Your gh login can't create organization webhooks. It needs the admin:org_hook scope and organization owner rights.",
            Some(format!("gh auth refresh --hostname {host} --scopes {HOOK_SCOPE}")),
        )
    } else if lower.contains("gh auth token not found") || lower.contains("gh auth login") {
        unavailable(
            "The GitHub CLI isn't logged in.",
            Some(format!("gh auth login --hostname {host}")),
        )
    } else {
        None
    }
}

/// How one forwarder is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardSpec {
    pub program: PathBuf,
    pub org: String,
    /// GitHub host, `github.com` unless GitHub Enterprise Server.
    pub host: String,
    pub url: String,
    pub secret: String,
}

impl ForwardSpec {
    /// Arguments after the program. The secret is in argv: the extension can't read it elsewhere.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            "webhook".to_string(),
            "forward".into(),
            format!("--org={}", self.org),
            format!("--events={FEED_EVENTS}"),
            format!("--url={}", self.url),
            format!("--secret={}", self.secret),
        ];
        if self.host != "github.com" {
            args.push(format!("--github-host={}", self.host));
        }
        args
    }
}

/// Output from, or the end of, a forwarder process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessEvent {
    Line(String),
    Exited(Option<i32>),
}

/// A running forwarder.
pub trait ForwardProcess: Send {
    /// The next output line or the exit. Cancellation-safe.
    fn next_event(&mut self) -> impl Future<Output = ProcessEvent> + Send;
    /// Stops the process and everything it started.
    fn terminate(&mut self);
    /// The process group to signal when the app quits.
    fn group(&self) -> Option<u32>;
}

/// Starts forwarders. Tests use a fake; the app uses [`GhLauncher`].
pub trait Launch: Send + Sync + 'static {
    type Process: ForwardProcess + 'static;
    fn launch(&self, spec: &ForwardSpec) -> std::io::Result<Self::Process>;
}

/// Restart timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub first: Duration,
    pub max: Duration,
    /// After forwarding this long, an exit is GitHub's routine disconnect, not a failure.
    pub healthy_after: Duration,
    pub after_healthy: Duration,
    pub unavailable: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            first: Duration::from_secs(5),
            max: Duration::from_secs(300),
            healthy_after: Duration::from_secs(60),
            after_healthy: Duration::from_secs(2),
            unavailable: Duration::from_secs(600),
        }
    }
}

impl Backoff {
    fn delay(&self, failures: u32) -> Duration {
        let factor = 2u32.saturating_pow(failures.saturating_sub(1).min(16));
        self.first.saturating_mul(factor).min(self.max)
    }
}

/// Current feed states and the process groups to stop when the app quits.
///
/// Each supervisor has a generation, so after a quick off-and-on the old supervisor's cleanup
/// can't remove the new one's status or process group.
#[derive(Debug, Default)]
pub struct FeedRegistry {
    statuses: Mutex<BTreeMap<String, (u64, FleetJobFeedStatus)>>,
    groups: Mutex<BTreeMap<(String, u64), u32>>,
    generations: std::sync::atomic::AtomicU64,
    stopped: std::sync::atomic::AtomicBool,
}

impl FeedRegistry {
    pub fn statuses(&self) -> Vec<FleetJobFeedStatus> {
        self.statuses.locked().values().map(|(_, s)| s.clone()).collect()
    }

    /// Whether `org`'s forwarder is connected, so polling can slow down.
    pub fn forwarding(&self, org: &str) -> bool {
        self.statuses
            .locked()
            .get(&org.to_lowercase())
            .is_some_and(|(_, s)| s.state == FleetJobFeedState::Forwarding)
    }

    pub fn delivered(&self, org: &str, at: DateTime<Utc>) {
        if let Some((_, status)) = self.statuses.locked().get_mut(&org.to_lowercase()) {
            status.last_delivery_at = Some(at);
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Terminates every forwarder's process group now, without the async runtime.
    pub fn stop_all(&self) {
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        for ((org, _), group) in std::mem::take(&mut *self.groups.locked()) {
            tracing::info!(%org, "stopping job feed forwarder");
            terminate_group(group);
        }
    }

    fn next_generation(&self) -> u64 {
        self.generations.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }

    /// Updates `org`'s status for supervisor `generation`; an older generation's update is ignored.
    fn set(&self, org: &str, generation: u64, update: impl FnOnce(&mut FleetJobFeedStatus)) {
        let mut statuses = self.statuses.locked();
        let fresh = || FleetJobFeedStatus {
            org: org.to_string(),
            state: FleetJobFeedState::Starting,
            detail: None,
            fix: None,
            since: Utc::now(),
            last_delivery_at: None,
            restarts: 0,
        };
        let entry = statuses.entry(org.to_string()).or_insert_with(|| (generation, fresh()));
        if entry.0 > generation {
            return;
        }
        if entry.0 < generation {
            *entry = (generation, fresh());
        }
        let status = &mut entry.1;
        let before = status.state;
        update(status);
        if status.state != before {
            status.since = Utc::now();
        }
    }

    fn remove(&self, org: &str, generation: u64) {
        let mut statuses = self.statuses.locked();
        if statuses.get(org).is_some_and(|(g, _)| *g == generation) {
            statuses.remove(org);
        }
        self.groups.locked().remove(&(org.to_string(), generation));
    }
}

fn terminate_group(group: u32) {
    #[cfg(unix)]
    if let Some(pid) = i32::try_from(group).ok().and_then(rustix::process::Pid::from_raw) {
        // Best effort: the group may already have exited.
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::TERM);
    }
    #[cfg(not(unix))]
    let _ = group;
}

/// Runs one organization's forwarder until `stop` turns true, restarting it as it exits.
pub async fn supervise<L: Launch>(
    launcher: &L,
    spec: ForwardSpec,
    registry: &FeedRegistry,
    mut stop: watch::Receiver<bool>,
    backoff: Backoff,
) {
    let org = spec.org.to_lowercase();
    let generation = registry.next_generation();
    let mut failures = 0u32;
    while !*stop.borrow() && !registry.is_stopped() {
        registry.set(&org, generation, |s| {
            s.state = FleetJobFeedState::Starting;
            s.fix = None;
        });
        let (unavailable, healthy) = match launcher.launch(&spec) {
            Ok(mut process) => {
                if let Some(group) = process.group() {
                    registry.groups.locked().insert((org.clone(), generation), group);
                }
                let outcome =
                    watch_process(&mut process, &spec, (&org, generation), registry, &mut stop, backoff).await;
                registry.groups.locked().remove(&(org.clone(), generation));
                match outcome {
                    Some(outcome) => outcome,
                    None => {
                        process.terminate();
                        registry.remove(&org, generation);
                        return;
                    }
                }
            }
            Err(e) => {
                let detail = if e.kind() == std::io::ErrorKind::NotFound {
                    "The GitHub CLI (gh) isn't installed.".to_string()
                } else {
                    format!("Couldn't start gh webhook forward: {e}.")
                };
                (Some((detail, None)), false)
            }
        };
        failures = if healthy { 0 } else { failures.saturating_add(1) };
        let delay = match &unavailable {
            Some(_) => backoff.unavailable,
            None if healthy => backoff.after_healthy,
            None => backoff.delay(failures),
        };
        registry.set(&org, generation, |s| {
            s.restarts = s.restarts.saturating_add(1);
            match unavailable {
                Some((detail, fix)) => {
                    s.state = FleetJobFeedState::Unavailable;
                    s.detail = Some(detail);
                    s.fix = fix;
                }
                None => s.state = FleetJobFeedState::Restarting,
            }
        });
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            _ = stop.changed() => {}
        }
    }
    registry.remove(&org, generation);
}

type Unavailable = Option<(String, Option<String>)>;

/// Follows one process to its exit. `None` means `stop` was requested.
async fn watch_process<P: ForwardProcess>(
    process: &mut P,
    spec: &ForwardSpec,
    (org, generation): (&str, u64),
    registry: &FeedRegistry,
    stop: &mut watch::Receiver<bool>,
    backoff: Backoff,
) -> Option<(Unavailable, bool)> {
    let mut forwarding_since: Option<tokio::time::Instant> = None;
    let mut unavailable: Unavailable = None;
    loop {
        let event = tokio::select! {
            event = process.next_event() => event,
            _ = stop.changed() => return None,
        };
        match event {
            ProcessEvent::Line(line) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                match classify(&line, &spec.host) {
                    Some(Signal::Forwarding) => {
                        forwarding_since = Some(tokio::time::Instant::now());
                        registry.set(org, generation, |s| {
                            s.state = FleetJobFeedState::Forwarding;
                            s.detail = None;
                            s.fix = None;
                        });
                    }
                    Some(Signal::Unavailable { detail, fix }) => unavailable = Some((detail, fix)),
                    None => {
                        // Keep the forwarder's own words for the status, minus the secret.
                        let line = line.replace(&spec.secret, "…");
                        registry.set(org, generation, |s| s.detail = Some(line));
                    }
                }
            }
            ProcessEvent::Exited(code) => {
                tracing::info!(%org, ?code, "job feed forwarder exited");
                let healthy = forwarding_since.is_some_and(|at| at.elapsed() >= backoff.healthy_after);
                return Some((unavailable, healthy && code != Some(127)));
            }
        }
    }
}

/// Runs `gh webhook forward` in its own process group, merging stdout and stderr lines.
#[derive(Debug, Default, Clone, Copy)]
pub struct GhLauncher;

#[derive(Debug)]
pub struct GhProcess {
    child: tokio::process::Child,
    lines: tokio::sync::mpsc::Receiver<String>,
    group: Option<u32>,
}

impl Launch for GhLauncher {
    type Process = GhProcess;

    fn launch(&self, spec: &ForwardSpec) -> std::io::Result<GhProcess> {
        use tokio::io::AsyncBufReadExt;
        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(spec.args())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // gh runs the extension as its own child; a group lets both be stopped together.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn()?;
        let group = child.id();
        let (tx, lines) = tokio::sync::mpsc::channel(64);
        let stdout = child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>);
        let stderr = child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>);
        for pipe in [stdout, stderr].into_iter().flatten() {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(pipe).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    // A slow consumer only loses output lines, never blocks the child.
                    let _ = tx.try_send(line);
                }
            });
        }
        Ok(GhProcess { child, lines, group })
    }
}

impl ForwardProcess for GhProcess {
    async fn next_event(&mut self) -> ProcessEvent {
        if let Some(line) = self.lines.recv().await {
            return ProcessEvent::Line(line);
        }
        ProcessEvent::Exited(self.child.wait().await.ok().and_then(|s| s.code()))
    }

    fn terminate(&mut self) {
        if let Some(group) = self.group.take() {
            terminate_group(group);
        }
        // Best effort: the child may already have exited.
        let _ = self.child.start_kill();
    }

    fn group(&self) -> Option<u32> {
        self.group
    }
}

impl Drop for GhProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(test)]
#[path = "feed_tests.rs"]
mod tests;
