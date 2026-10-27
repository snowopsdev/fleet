//! Reviewed maintenance on inventory hosts: disk cleanup, runner upgrades, and
//! stopping (drain) or starting (resume) one runner's service.
//!
//! These extend Runner Fleet's operations with the same guards: every change
//! re-reads GitHub right before it acts, a busy runner is never stopped,
//! upgraded or cleaned, and the host scripts re-check that no job is running
//! in the runner directory immediately before they change anything.
//!
//! Cleanup is the one place that removes files recursively. It only touches
//! the exact targets listed by [`CleanupKind`], resolved on the host and
//! checked to sit inside the expected directory with no symlink on the way.
//! It never removes `.runner`, `.credentials*`, config files, `bin` or
//! `externals`, and never uses globs or removes a computed parent.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::github::ReleaseAsset;
use super::model::{GithubRunner, Host, LocalRunner, Runner, RunnerRole, ServiceMode};
use super::operations::{OperationError, Ops, check_enabled, inventory_runner};

type Result<T> = std::result::Result<T, OperationError>;

fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(OperationError(message.into()))
}

/// Diagnostic logs are kept for this many days unless the review asks otherwise.
pub const DEFAULT_DIAG_DAYS: u32 = 14;

/// What a cleanup review covers, chosen in the review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export, rename = "FleetCleanupOptions")]
#[serde(rename_all = "camelCase")]
pub struct CleanupOptions {
    /// Empty each idle runner's whole `_work` instead of only `_temp` and `_actions`.
    pub whole_work: bool,
    /// Remove runner and worker diagnostic logs older than this many days.
    pub diag_older_than_days: u32,
}

impl Default for CleanupOptions {
    fn default() -> Self {
        Self {
            whole_work: false,
            diag_older_than_days: DEFAULT_DIAG_DAYS,
        }
    }
}

/// One kind of cleanup target. Each maps to one fixed path or command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[ts(export, rename = "FleetCleanupKind")]
#[serde(rename_all = "kebab-case")]
pub enum CleanupKind {
    /// `RUNNER/_work/_temp`: contents removed.
    WorkTemp,
    /// `RUNNER/_work/_actions`: downloaded actions, contents removed.
    WorkActions,
    /// `RUNNER/_work`: every workspace, tool cache and temp file (opt-in).
    Work,
    /// Old `Runner_*.log` and `Worker_*.log` files directly in `RUNNER/_diag`.
    Diag,
    /// `RUNNER/bin.VERSION` folders that `bin` doesn't point at: previous
    /// versions kept by an upgrade or the runner's own self-update.
    PreviousBin,
    /// `RUNNER/externals.VERSION` folders that `externals` doesn't point at.
    PreviousExternals,
    /// macOS: the contents of `~/Library/Developer/Xcode/DerivedData`.
    XcodeDerivedData,
    /// macOS: `xcrun simctl delete unavailable`.
    #[serde(rename = "simulators")]
    UnavailableSimulators,
    /// `docker system prune -f`: stopped containers, unused networks, dangling
    /// images and build cache. Never volumes.
    Docker,
}

impl CleanupKind {
    /// The spelling the host scripts use.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkTemp => "work-temp",
            Self::WorkActions => "work-actions",
            Self::Work => "work",
            Self::Diag => "diag",
            Self::PreviousBin => "previous-bin",
            Self::PreviousExternals => "previous-externals",
            Self::XcodeDerivedData => "xcode-derived-data",
            Self::UnavailableSimulators => "simulators",
            Self::Docker => "docker",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::WorkTemp,
            Self::WorkActions,
            Self::Work,
            Self::Diag,
            Self::PreviousBin,
            Self::PreviousExternals,
            Self::XcodeDerivedData,
            Self::UnavailableSimulators,
            Self::Docker,
        ]
        .into_iter()
        .find(|k| k.as_str() == value)
    }

    /// Targets inside one runner's directory, as opposed to machine-wide ones.
    pub const fn is_runner_target(self) -> bool {
        match self {
            Self::WorkTemp
            | Self::WorkActions
            | Self::Work
            | Self::Diag
            | Self::PreviousBin
            | Self::PreviousExternals => true,
            Self::XcodeDerivedData | Self::UnavailableSimulators | Self::Docker => false,
        }
    }
}

/// What the host should look for: a runner target is resolved from that
/// runner's exact inventory directory; a machine target needs no path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupCandidate {
    pub kind: CleanupKind,
    /// The inventory runner and its exact directory, for runner targets.
    pub runner: Option<(String, String)>,
}

/// One target found on the host, with what it measured at review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, rename = "FleetCleanupTarget")]
#[serde(rename_all = "camelCase")]
pub struct CleanupTarget {
    pub kind: CleanupKind,
    /// The runner whose directory this is in; `None` for machine-wide targets.
    pub runner: Option<String>,
    /// The path as resolved on the host (no symlinks), when there is one.
    pub path: Option<String>,
    /// Measured size in KiB, when it could be measured.
    #[ts(type = "number | null")]
    pub kib: Option<u64>,
    /// Files (diagnostic logs) or simulators found.
    #[ts(type = "number | null")]
    pub count: Option<u64>,
    /// What the host reported, such as Docker's reclaimable space.
    pub detail: Option<String>,
    /// Diagnostic logs only: the exact file names that will be removed.
    pub files: Vec<String>,
}

impl CleanupTarget {
    /// The plan-item reason for this target. It names the exact path, never
    /// a size, so a re-check compares what will be removed rather than how
    /// big it has become.
    pub fn reason(&self, options: &CleanupOptions) -> String {
        let path = self.path.as_deref().unwrap_or("");
        match self.kind {
            CleanupKind::WorkTemp | CleanupKind::WorkActions | CleanupKind::Work | CleanupKind::XcodeDerivedData => {
                format!("remove everything inside {path}")
            }
            CleanupKind::Diag => format!(
                "remove {} diagnostic log(s) older than {} days from {path}",
                self.files.len(),
                options.diag_older_than_days
            ),
            CleanupKind::PreviousBin | CleanupKind::PreviousExternals => format!(
                "remove previous runner version folder(s) from {path}: {}",
                self.files.join(", ")
            ),
            CleanupKind::UnavailableSimulators => "run xcrun simctl delete unavailable".into(),
            CleanupKind::Docker => "run docker system prune -f (volumes are kept)".into(),
        }
    }

    /// The plan item's runner column: the runner, or `machine`.
    pub fn item_runner(&self) -> &str {
        self.runner.as_deref().unwrap_or(MACHINE)
    }
}

/// The plan-item runner column for machine-wide targets.
pub const MACHINE: &str = "machine";

/// Inventory runners whose directories a cleanup looks at: declared,
/// configured runners that aren't being decommissioned.
fn cleanup_runners(host: &Host) -> impl Iterator<Item = &Runner> {
    host.desired_runners()
}

/// Everything a cleanup on `host` looks for, by exact location.
pub fn cleanup_candidates(host: &Host, options: &CleanupOptions) -> Vec<CleanupCandidate> {
    let mut out = Vec::new();
    for runner in cleanup_runners(host) {
        let at = Some((runner.name.clone(), host.runner_path(runner)));
        let kinds: &[CleanupKind] = if options.whole_work {
            &[CleanupKind::Work]
        } else {
            &[CleanupKind::WorkTemp, CleanupKind::WorkActions]
        };
        for kind in kinds.iter().chain(&[
            CleanupKind::Diag,
            CleanupKind::PreviousBin,
            CleanupKind::PreviousExternals,
        ]) {
            out.push(CleanupCandidate {
                kind: *kind,
                runner: at.clone(),
            });
        }
    }
    if host.service_mode == ServiceMode::Launchagent {
        for kind in [CleanupKind::XcodeDerivedData, CleanupKind::UnavailableSimulators] {
            out.push(CleanupCandidate { kind, runner: None });
        }
    }
    if !host.is_windows() {
        out.push(CleanupCandidate {
            kind: CleanupKind::Docker,
            runner: None,
        });
    }
    out
}

/// Diagnostic-log retention must leave at least a day of logs.
pub fn check_cleanup_options(options: &CleanupOptions) -> Result<()> {
    if !(1..=365).contains(&options.diag_older_than_days) {
        return fail("Keep diagnostic logs for between 1 and 365 days.");
    }
    Ok(())
}

/// Why `target` can't be cleaned now, given GitHub's runners. A runner target
/// needs that runner registered and idle; a machine-wide target needs every
/// runner on the host idle, since they share the account's caches.
pub fn cleanup_refusal(host: &Host, target: &CleanupTarget, github: &BTreeMap<String, GithubRunner>) -> Option<String> {
    match &target.runner {
        Some(name) => match github.get(name) {
            None => Some(format!("{name} is missing on GitHub; cannot verify it is idle")),
            Some(r) if r.busy => Some(format!("{name} is busy; its directories are left alone")),
            Some(_) => None,
        },
        None => host
            .runner_names()
            .find(|name| github.get(*name).is_some_and(|r| r.busy))
            .map(|name| format!("{name} is busy; machine-wide caches are left alone until every runner is idle")),
    }
}

/// `a.b.c` version parts, for comparing runner versions.
fn version_parts(version: &str) -> Option<Vec<u64>> {
    let version = version.trim().trim_start_matches('v');
    if version.is_empty() {
        return None;
    }
    version.split('.').map(|p| p.parse().ok()).collect()
}

/// Whether `current` is an older runner version than `latest`. Unknown or
/// unparsable versions are never "behind".
pub fn version_behind(current: &str, latest: &str) -> bool {
    match (version_parts(current), version_parts(latest)) {
        (Some(current), Some(latest)) => current < latest,
        _ => false,
    }
}

/// Drain, resume and upgrade apply to declared runners still in the inventory.
pub fn check_service_change(name: &str, runner: &Runner, role: RunnerRole) -> Result<()> {
    if role != RunnerRole::Declared {
        return fail(format!(
            "{name} is a legacy runner; retire it instead of changing its service"
        ));
    }
    if runner.decommission {
        return fail(format!("{name} is declared for decommission; refusing"));
    }
    Ok(())
}

/// Runners a cleanup must treat specially: drained ones (never stopped or
/// started by it) and ones held stopped after a failed change (their
/// previous versions, the rollback copy, are kept).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeldRunners {
    pub drained: BTreeSet<String>,
    pub held: BTreeSet<String>,
}

/// What a cleanup or upgrade did to runner services, for the engine's holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeReport {
    /// Runners the change left stopped on purpose (or couldn't start again).
    pub left_stopped: Vec<String>,
    /// Runners an upgrade finished, running the new version.
    pub upgraded: Vec<String>,
}

/// Service states that mean the runner can't take a job.
fn definitely_stopped(state: &str) -> bool {
    matches!(
        state,
        "inactive" | "failed" | "stopped" | "disabled" | "uninstalled" | "missing"
    )
}

/// The result of asking an idle runner's service to stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// The service was stopped. `registration` is GitHub's runner id.
    Stopped { message: String, registration: i64 },
    /// GitHub, or the host itself, reports a job running; nothing was changed.
    Busy { registration: i64 },
}

/// One cleanup target's result on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupOutcome {
    pub kind: CleanupKind,
    pub runner: Option<String>,
    /// What happened, as a sentence fragment for the operation messages.
    pub message: String,
    pub ok: bool,
}

impl Ops<'_> {
    fn group_runners(&self) -> Result<BTreeMap<String, GithubRunner>> {
        self.github
            .list_runners(&self.inventory.organization.runner_group)
            .map_err(|e| OperationError(format!("cannot verify runners are idle: {e}")))
    }

    /// Removes exactly the reviewed cleanup targets. GitHub and the host are
    /// read again first, and a target whose runner became busy is skipped.
    ///
    /// A runner's service is stopped (refusing while a job runs) before its
    /// directories are cleaned, so it can't pick up a job mid-removal, and is
    /// started again afterwards if it was running. Machine-wide caches are
    /// shared by every runner, so they are cleaned only when every runner on
    /// the host (legacy and decommission-pending ones too) is definitely
    /// stopped, or is a declared runner this cleanup stops. Drained runners
    /// are never stopped or started here; a held runner keeps its previous
    /// versions (its rollback copy). If the host can't be reached during the
    /// removal, the runners stopped for it stay stopped and are reported held:
    /// the host may still be removing files.
    pub fn cleanup_host(
        &self,
        host_id: &str,
        options: &CleanupOptions,
        targets: &[CleanupTarget],
        held: &HeldRunners,
        report: &mut ChangeReport,
    ) -> Result<Vec<String>> {
        let host = self.inventory.find_host(host_id)?;
        check_enabled(host)?;
        check_cleanup_options(options)?;
        let github = self.group_runners()?;
        let local = self
            .remote
            .probe(host)
            .map_err(|e| OperationError(format!("cleanup preflight failed without changes: {e}")))?;
        let running = |name: &str| local.get(name).is_some_and(LocalRunner::service_active);
        let mut messages = Vec::new();
        let mut runnable = Vec::new();
        for target in targets {
            let refusal = cleanup_refusal(host, target, &github).or_else(|| match &target.runner {
                Some(name) if held.drained.contains(name) && running(name) => Some(format!(
                    "{name} is draining; its directories wait until the drain stops it"
                )),
                Some(name)
                    if held.held.contains(name)
                        && matches!(target.kind, CleanupKind::PreviousBin | CleanupKind::PreviousExternals) =>
                {
                    Some(format!(
                        "{name} is held stopped after a failed change; its previous versions are kept"
                    ))
                }
                _ => None,
            });
            match refusal {
                Some(reason) => messages.push(format!("skipped {}: {reason}", target.reason(options))),
                None => runnable.push(target.clone()),
            }
        }
        // The declared runners this cleanup may stop.
        let stoppable = |name: &str| host.desired_runners().any(|r| r.name == name) && !held.drained.contains(name);
        if runnable.iter().any(|t| t.runner.is_none()) {
            let blocker = host.runner_names().find_map(|name| match local.get(name) {
                None => Some(format!("{name}'s state is unknown")),
                Some(l) if !l.configured || definitely_stopped(&l.service_state) => None,
                Some(l) if l.service_active() && stoppable(name) => None,
                Some(l) => Some(format!("{name} is {} and isn't stopped for a cleanup", l.service_state)),
            });
            if let Some(reason) = blocker {
                runnable.retain(|t| t.runner.is_some());
                messages.push(format!("skipped machine-wide caches: {reason}"));
            }
        }
        let machine_wide = runnable.iter().any(|t| t.runner.is_none());
        let to_stop: Vec<&Runner> = host
            .desired_runners()
            .filter(|r| running(&r.name) && stoppable(&r.name))
            .filter(|r| machine_wide || runnable.iter().any(|t| t.runner.as_deref() == Some(r.name.as_str())))
            .collect();
        let sudo = self.secrets.sudo_password_for(host);
        let mut stopped: Vec<&Runner> = Vec::new();
        let mut stop_error = None;
        for runner in to_stop {
            match self.remote.stop(host, runner, sudo) {
                Ok(_) => stopped.push(runner),
                Err(e) if e.0.contains(JOB_RUNNING) => {
                    messages.push(format!(
                        "skipped {}'s directories and machine-wide caches: a job started",
                        runner.name
                    ));
                    runnable.retain(|t| t.runner.as_deref().is_some_and(|n| n != runner.name));
                }
                Err(e) => {
                    // The stop may have worked despite the error: find out, so
                    // the runner is either started again below or held.
                    match self
                        .remote
                        .probe(host)
                        .map(|l| l.get(&runner.name).map(LocalRunner::service_active))
                    {
                        Ok(Some(true)) => {}
                        Ok(_) => stopped.push(runner),
                        Err(_) => report.left_stopped.push(runner.name.clone()),
                    }
                    stop_error = Some(format!("couldn't stop {} before cleaning: {e}", runner.name));
                    break;
                }
            }
        }
        let outcomes = match (&stop_error, runnable.is_empty()) {
            (None, false) => self.remote.cleanup(host, &runnable, options),
            _ => Ok(Vec::new()),
        };
        let mut failures = Vec::new();
        let outcomes = match outcomes {
            Ok(outcomes) => {
                // Put every service the cleanup stopped back the way it was.
                for runner in stopped {
                    if let Err(e) = self.remote.start(host, runner, sudo) {
                        report.left_stopped.push(runner.name.clone());
                        failures.push(format!(
                            "{} couldn't be started again: {e}; it's held stopped until you resume it",
                            runner.name
                        ));
                    }
                }
                outcomes
            }
            Err(e) => {
                // The removal may still be running on the host: starting the
                // services now could let a job run in a half-emptied directory.
                let names: Vec<String> = stopped.iter().map(|r| r.name.clone()).collect();
                report.left_stopped.extend(names.iter().cloned());
                let held_note = if names.is_empty() {
                    String::new()
                } else {
                    format!(
                        " {} stay stopped and held, since the host may still be removing files; check the machine, then resume them",
                        names.join(", ")
                    )
                };
                return fail(format!(
                    "cleanup on {} lost contact with the machine: {e}.{held_note}",
                    host.id
                ));
            }
        };
        if let Some(error) = stop_error {
            failures.insert(0, format!("{error}; nothing was removed"));
        }
        let mut cleaned = 0;
        for outcome in outcomes {
            if outcome.ok {
                cleaned += 1;
                messages.push(outcome.message);
            } else {
                failures.push(outcome.message);
            }
        }
        if !failures.is_empty() {
            let done = if messages.is_empty() {
                String::new()
            } else {
                format!(" Done: {}.", messages.join("; "))
            };
            return fail(format!(
                "cleanup on {} finished with errors: {}.{done}",
                host.id,
                failures.join("; ")
            ));
        }
        if cleaned == 0 {
            return fail(if messages.is_empty() {
                "nothing to clean up".to_string()
            } else {
                messages.join("; ")
            });
        }
        Ok(messages)
    }

    /// Stops `name`'s service if GitHub says it's idle; a busy runner is left
    /// running so the caller can drain it. `expected` is the registration a
    /// deferred stop was reviewed against: a different one is refused.
    pub fn stop_if_idle(&self, name: &str, expected: Option<i64>) -> Result<StopOutcome> {
        let (host, runner, role) = self.inventory.find_runner(name)?;
        check_service_change(name, runner, role)?;
        let github = self.group_runners()?;
        let Some(current) = github.get(name) else {
            return fail(format!("{name} is missing on GitHub; cannot verify it is idle"));
        };
        if expected.is_some_and(|id| id != current.id) {
            return fail(format!(
                "{name} was registered again since the drain was reviewed; refusing to stop it"
            ));
        }
        let registration = current.id;
        if current.busy {
            return Ok(StopOutcome::Busy { registration });
        }
        match self.remote.stop(host, runner, self.secrets.sudo_password_for(host)) {
            Ok(_) => Ok(StopOutcome::Stopped {
                message: format!("stopped {name} on {}", host.id),
                registration,
            }),
            Err(e) if e.0.contains(JOB_RUNNING) => Ok(StopOutcome::Busy { registration }),
            Err(e) => fail(format!("couldn't stop {name}: {e}")),
        }
    }

    /// Starts one stopped runner's service.
    pub fn resume_runner(&self, name: &str) -> Result<String> {
        let (host, runner, role) = self.inventory.find_runner(name)?;
        check_service_change(name, runner, role)?;
        self.remote
            .start(host, runner, self.secrets.sudo_password_for(host))
            .map_err(|e| OperationError(format!("couldn't start {name}: {e}")))?;
        Ok(format!("started {name} on {}", host.id))
    }

    /// Upgrades the named runners on one host to `asset`, one at a time:
    /// stop the idle service, replace `bin/` and `externals/` in place, start
    /// it and confirm the new version. A runner that became busy is skipped;
    /// a failure stops there and leaves that runner stopped, named in `left_stopped`.
    pub fn upgrade_runners(
        &self,
        host_id: &str,
        names: &[String],
        asset: &ReleaseAsset,
        report: &mut ChangeReport,
    ) -> Result<Vec<String>> {
        let host = self.inventory.find_host(host_id)?;
        check_enabled(host)?;
        if asset.sha256.is_none() {
            return fail(format!(
                "GitHub didn't publish a SHA-256 digest for {}; refusing to upgrade",
                asset.name
            ));
        }
        let mut messages = Vec::new();
        for name in names {
            let runner = inventory_runner(host, name)?;
            check_service_change(name, runner, RunnerRole::Declared)?;
            let github = self.group_runners()?;
            match github.get(name) {
                None => {
                    messages.push(format!(
                        "skipped {name}: missing on GitHub, so it can't be verified idle"
                    ));
                    continue;
                }
                Some(r) if r.busy => {
                    messages.push(format!("skipped {name}: busy"));
                    continue;
                }
                Some(_) => {}
            }
            messages.push(self.upgrade_one(host, runner, asset, report)?);
        }
        Ok(messages)
    }

    fn upgrade_one(
        &self,
        host: &Host,
        runner: &Runner,
        asset: &ReleaseAsset,
        report: &mut ChangeReport,
    ) -> Result<String> {
        let name = &runner.name;
        let sudo = self.secrets.sudo_password_for(host);
        match self.remote.stop(host, runner, sudo) {
            Ok(_) => {}
            Err(e) if e.0.contains(JOB_RUNNING) => return Ok(format!("skipped {name}: a job started")),
            Err(e) => {
                // The stop may have worked despite the error: start it again,
                // or hold it if that can't be confirmed.
                let state = self
                    .remote
                    .probe(host)
                    .map(|l| l.get(name).map(LocalRunner::service_active));
                let note = match state {
                    Ok(Some(true)) => String::new(),
                    Ok(_) => match self.remote.start(host, runner, sudo) {
                        Ok(_) => format!("; {name} had stopped anyway and was started again"),
                        Err(start) => {
                            report.left_stopped.push(name.clone());
                            format!("; {name} is stopped and couldn't be started again ({start}), so it's held")
                        }
                    },
                    Err(_) => {
                        report.left_stopped.push(name.clone());
                        format!("; {name}'s state couldn't be read, so it's held until you check it")
                    }
                };
                return fail(format!("upgrade of {name} stopped before any change: {e}{note}"));
            }
        }
        // Until it's running the new version, a failure leaves it stopped on purpose.
        report.left_stopped.push(name.clone());
        let recovery = |e: &dyn std::fmt::Display| {
            OperationError(format!(
                "upgrade failed for {name}: {e}. {name} is left stopped and held. The upgrade rolls back its \
                 own swap, and an interrupted one is rolled back by the next upgrade; the previous version stays \
                 beside it as bin.VERSION and externals.VERSION. Check its diagnostic log, then Resume… or \
                 Restart… it."
            ))
        };
        let local = self.remote.probe(host).map_err(|e| recovery(&e))?;
        if local.get(name).is_some_and(super::model::LocalRunner::service_active) {
            return Err(recovery(&"its service is still running after the stop"));
        }
        let replaced = self.remote.upgrade(host, runner, asset).map_err(|e| recovery(&e))?;
        self.remote.start(host, runner, sudo).map_err(|e| recovery(&e))?;
        let local = self.remote.probe(host).map_err(|e| recovery(&e))?;
        let Some(after) = local.get(name) else {
            return Err(recovery(&"the host no longer reports it"));
        };
        if after.version.as_deref() != Some(asset.version.as_str()) {
            return Err(recovery(&format!(
                "it reports version {} after the upgrade, not {}",
                after.version.as_deref().unwrap_or("unknown"),
                asset.version
            )));
        }
        if !after.service_active() {
            return Err(recovery(&format!(
                "its service is {} after starting",
                after.service_state
            )));
        }
        report.left_stopped.retain(|n| n != name);
        report.upgraded.push(name.clone());
        let online = self.wait_online(name);
        let note = if replaced.contains("already runs") {
            "already ran"
        } else {
            "upgraded to"
        };
        Ok(if online {
            format!("{name} {note} {} and is online", asset.version)
        } else {
            format!(
                "{name} {note} {} and its service is running; GitHub hasn't reported it online yet",
                asset.version
            )
        })
    }

    fn wait_online(&self, name: &str) -> bool {
        for attempt in 0..10 {
            if let Ok(runners) = self.github.list_runners(&self.inventory.organization.runner_group)
                && runners.get(name).is_some_and(|r| r.status == "online")
            {
                return true;
            }
            if attempt < 9 {
                (self.sleep)(Duration::from_secs(3));
            }
        }
        false
    }
}

/// What the host scripts print (on stderr) when a job is running in the
/// runner directory they were asked to change.
pub const JOB_RUNNING: &str = "a job is running";
