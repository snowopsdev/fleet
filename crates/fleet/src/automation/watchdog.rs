//! The opt-in watchdog: which runners it may restart and when. Pure, so the
//! backoff and the attempt limit are tested without a clock or a machine.
//!
//! A runner qualifies only when it is an inventory runner (not legacy, not
//! being decommissioned), on an enabled host with a fresh SSH read showing its
//! service stopped or failed, with fresh GitHub data showing it offline and
//! not busy, and no drain holds it stopped. The first attempt follows two
//! minutes after the service was first seen stopped, then 5, 15 and 60
//! minutes after the previous attempt. At most three attempts are made per
//! runner in six hours; when a fourth would be due the watchdog gives up,
//! raises an alert, and waits until the service is seen running again. A
//! runner held stopped by a drain, or by a failed upgrade or cleanup, is never
//! restarted.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};

use super::store::{Journal, WatchdogRunner};
use crate::desired::model::is_active;
use crate::fleet_plan::DesiredRunnerRole;
use crate::model::{DataSource, FleetView, MachineConnection, MachineView, RunnerView};

/// Minutes to wait before attempt 1, 2, 3, and before giving up.
pub(crate) const DELAYS_MINUTES: [i64; 4] = [2, 5, 15, 60];
/// Attempts allowed per runner within [`WINDOW_HOURS`].
pub(crate) const MAX_ATTEMPTS: usize = 3;
pub(crate) const WINDOW_HOURS: i64 = 6;

/// Service states the watchdog restarts. Anything else (unknown, missing,
/// uninstalled, a disabled logon task, starting or stopping) is left alone.
const RESTARTABLE: [&str; 3] = ["inactive", "failed", "stopped"];

/// A runner and the machine it is on, as history records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Subject {
    pub runner: String,
    pub host: String,
    pub machine_key: String,
    pub machine_name: String,
    pub service_state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Restart now; `attempt` counts from 1 within the window.
    Restart { subject: Subject, attempt: usize },
    /// The attempt limit is reached; alert and stop trying.
    GiveUp { subject: Subject },
    /// The service runs again after the watchdog acted.
    Recovered { subject: Subject, gave_up: bool },
}

fn window() -> Duration {
    Duration::hours(WINDOW_HOURS)
}

/// When the next attempt (or the give-up) is due, while the service is down.
pub(crate) fn next_attempt(state: &WatchdogRunner) -> Option<DateTime<Utc>> {
    if state.gave_up_at.is_some() {
        return None;
    }
    let down = state.down_since?;
    let index = state.attempts.len().min(DELAYS_MINUTES.len() - 1);
    let base = state.attempts.last().copied().filter(|at| *at > down).unwrap_or(down);
    Some(base + Duration::minutes(DELAYS_MINUTES[index]))
}

fn subject(machine: &MachineView, runner: &RunnerView, host: &str, state: &str) -> Subject {
    Subject {
        runner: runner.name.clone(),
        host: host.to_string(),
        machine_key: machine.preferences_key(),
        machine_name: machine.display_name.clone(),
        service_state: state.to_string(),
    }
}

/// What a fresh observation says about one inventory runner's service.
enum Observed {
    /// Stopped while GitHub shows it offline and idle: the watchdog may act.
    Stopped(Subject),
    /// Running again.
    Running(Subject),
    /// Unknown, stale, busy, or not the watchdog's to restart.
    Other,
}

fn observe(machine: &MachineView, runner: &RunnerView) -> Observed {
    let Some(desired) = machine.desired.as_ref().filter(|d| d.host.enabled) else {
        return Observed::Other;
    };
    if !matches!(
        runner.desired.as_ref().map(|d| &d.role),
        Some(DesiredRunnerRole::Declared)
    ) {
        return Observed::Other;
    }
    let host = &desired.host.id;
    let Some(local) = runner.local.as_ref().filter(|l| !l.stale && l.configured) else {
        return Observed::Other;
    };
    if is_active(&local.service_state) {
        return Observed::Running(subject(machine, runner, host, &local.service_state));
    }
    let connected = matches!(machine.connection, MachineConnection::Connected { .. });
    let offline_idle = runner
        .github
        .as_ref()
        .is_some_and(|g| !g.stale && g.status == "offline" && !g.busy);
    if connected && offline_idle && RESTARTABLE.contains(&local.service_state.as_str()) {
        Observed::Stopped(subject(machine, runner, host, &local.service_state))
    } else {
        Observed::Other
    }
}

/// Updates the watchdog's state from `view` and returns what to do now.
/// Attempts are recorded here, before they are made, so a restart of the app
/// can't repeat one.
pub(crate) fn plan(view: &FleetView, journal: &mut Journal, now: DateTime<Utc>) -> Vec<Decision> {
    if view.source != DataSource::Live {
        return Vec::new();
    }
    let mut decisions = Vec::new();
    let mut seen = BTreeSet::new();
    for machine in &view.machines {
        for runner in &machine.runners {
            seen.insert(runner.name.clone());
            if journal.drains.contains_key(&runner.name) || journal.holds.contains_key(&runner.name) {
                continue;
            }
            match observe(machine, runner) {
                Observed::Stopped(subject) => {
                    let state = journal.watchdog.entry(subject.runner.clone()).or_default();
                    state.host.clone_from(&subject.host);
                    state.attempts.retain(|at| now - *at < window());
                    if state.gave_up_at.is_some() {
                        continue;
                    }
                    state.down_since.get_or_insert(now);
                    let Some(due) = next_attempt(state) else { continue };
                    if now < due {
                        continue;
                    }
                    if state.attempts.len() >= MAX_ATTEMPTS {
                        state.gave_up_at = Some(now);
                        decisions.push(Decision::GiveUp { subject });
                    } else {
                        state.attempts.push(now);
                        decisions.push(Decision::Restart {
                            attempt: state.attempts.len(),
                            subject,
                        });
                    }
                }
                Observed::Running(subject) => {
                    let Some(state) = journal.watchdog.get_mut(&subject.runner) else {
                        continue;
                    };
                    let acted = state.down_since.is_some()
                        && state
                            .attempts
                            .last()
                            .is_some_and(|at| state.down_since.is_some_and(|d| *at >= d));
                    let gave_up = state.gave_up_at.is_some();
                    if acted || gave_up {
                        decisions.push(Decision::Recovered { subject, gave_up });
                    }
                    state.down_since = None;
                    state.gave_up_at = None;
                    state.last_error = None;
                    state.attempts.retain(|at| now - *at < window());
                }
                Observed::Other => {}
            }
        }
    }
    // Forget runners that left the inventory, and history older than the window.
    journal.watchdog.retain(|name, state| {
        seen.contains(name) && (state.down_since.is_some() || state.gave_up_at.is_some() || !state.attempts.is_empty())
    });
    decisions
}
