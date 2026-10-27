//! Changes the app makes later or on its own, each authorised in advance:
//!
//! - a reviewed drain's deferred stop, carried out once the runner is idle;
//! - the opt-in watchdog, whose enablement is the standing approval to restart
//!   stopped runner services with backoff and a hard attempt limit;
//! - the opt-in fleet availability variable written to the organization.
//!
//! Their durable state lives in `automation.json` beside the app settings.
//! Every action is recorded in machine history. Demo data never runs any of
//! them; `demo` attaches fictional state for previews.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub(crate) mod demo;
pub mod publish;
mod store;
pub(crate) mod watchdog;

pub use publish::{ActionsVariables, GhVariables, PublishSettings};
pub(crate) use store::{AutomationStore, DrainMark, DrainState, Journal as AutomationJournal};

/// Settings for automatic behaviour, saved with the app settings. Secrets
/// (webhook URLs, tokens) are kept in the Keychain, never here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export, rename = "FleetAutomationSettings")]
#[serde(rename_all = "camelCase")]
pub struct AutomationSettings {
    /// Restart inventory runner services observed stopped while GitHub shows
    /// them offline and idle. Off by default; turning it on is the standing
    /// approval for these restarts.
    #[serde(default)]
    pub watchdog: bool,
    /// Where alerts are also sent, besides desktop notifications.
    #[serde(default)]
    pub alert_destinations: Vec<crate::outbound::AlertDestination>,
    /// Publishing fleet availability to an organization Actions variable.
    #[serde(default)]
    pub publish: PublishSettings,
}

/// Automatic and deferred work, as the UI and local API see it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export, rename = "FleetAutomation")]
#[serde(rename_all = "camelCase")]
pub struct FleetAutomation {
    /// Runners being drained or held stopped by a reviewed drain.
    pub drains: Vec<FleetDrain>,
    pub watchdog: FleetWatchdog,
    /// Alert destinations (no secrets), with their delivery state.
    pub destinations: Vec<crate::outbound::FleetAlertDestinationView>,
    pub publish: FleetPublishStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export, rename = "FleetDrainState")]
#[serde(rename_all = "camelCase")]
pub enum FleetDrainState {
    /// Waiting for the current job to finish before stopping the service.
    Draining,
    /// The service was stopped by the drain and stays stopped until resumed.
    Stopped,
}

/// A reviewed drain: GitHub has no API to pause a runner, so draining stops
/// its service on the machine once it is idle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export, rename = "FleetDrain")]
#[serde(rename_all = "camelCase")]
pub struct FleetDrain {
    pub runner: String,
    pub host_id: String,
    pub state: FleetDrainState,
    pub requested_at: DateTime<Utc>,
    pub stopped_at: Option<DateTime<Utc>>,
    /// Why the last attempt to stop it didn't succeed.
    pub error: Option<String>,
}

/// The watchdog's settings and its per-runner restart history.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export, rename = "FleetWatchdog")]
#[serde(rename_all = "camelCase")]
pub struct FleetWatchdog {
    pub enabled: bool,
    pub runners: Vec<FleetWatchdogRunner>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export, rename = "FleetWatchdogRunner")]
#[serde(rename_all = "camelCase")]
pub struct FleetWatchdogRunner {
    pub runner: String,
    pub host_id: String,
    /// Restart attempts in the last six hours, oldest first.
    pub attempts: Vec<DateTime<Utc>>,
    /// When the service was first observed stopped in the current outage.
    pub down_since: Option<DateTime<Utc>>,
    /// The next attempt, if the service is still stopped then.
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// Set after the attempt limit; no more restarts until it runs again.
    pub gave_up_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

/// The organization Actions variable describing fleet availability.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export, rename = "FleetPublishStatus")]
#[serde(rename_all = "camelCase")]
pub struct FleetPublishStatus {
    pub enabled: bool,
    pub organization: Option<String>,
    pub variable: String,
    pub interval_minutes: u32,
    /// The value last written, and when.
    pub value: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    /// Why the last write failed.
    pub error: Option<String>,
    /// Why nothing is published right now (such as stale GitHub data).
    pub waiting: Option<String>,
}

#[cfg(test)]
mod tests;
