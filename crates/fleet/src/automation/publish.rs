//! Publishing fleet availability as an organization Actions variable, so
//! workflows can fall back to GitHub-hosted runners when self-hosted capacity
//! is down. Off by default. The value is compact JSON:
//!
//! ```json
//! {"updatedAt":"2026-10-03T12:00:00Z","online":{"linux-build":1,"macos-build":2},"healthy":true}
//! ```
//!
//! `online` counts inventory runners GitHub reports online, per capability
//! label (the inventory's labels except `fleet` and `host-*`). `healthy` means
//! every label has at least one online runner. It is written only from fresh
//! GitHub data, only when the counts change, and at most every N minutes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::derive::GITHUB_STALE_AFTER;
use crate::desired::process::{CommandRunner, NonZeroExit};
use crate::fleet_plan::DesiredRunnerRole;
use crate::model::FleetView;

pub const DEFAULT_VARIABLE: &str = "RUNNER_FLEET_STATUS";
pub const DEFAULT_INTERVAL_MINUTES: u32 = 5;

fn default_variable() -> String {
    DEFAULT_VARIABLE.into()
}

fn default_interval() -> u32 {
    DEFAULT_INTERVAL_MINUTES
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export, rename = "FleetPublishSettings")]
#[serde(rename_all = "camelCase")]
pub struct PublishSettings {
    #[serde(default)]
    pub enabled: bool,
    /// The organization variable's name.
    #[serde(default = "default_variable")]
    pub variable: String,
    /// The shortest time between two writes.
    #[serde(default = "default_interval")]
    pub interval_minutes: u32,
}

impl Default for PublishSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            variable: default_variable(),
            interval_minutes: default_interval(),
        }
    }
}

/// GitHub's rules for variable names: letters, digits and underscores, not
/// starting with a digit or `GITHUB_`.
pub fn check_settings(settings: &PublishSettings) -> Result<(), String> {
    let name = settings.variable.as_str();
    let valid = !name.is_empty()
        && name.len() <= 100
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && !name.to_ascii_uppercase().starts_with("GITHUB_");
    if !valid {
        return Err(
            "Use a variable name with only letters, digits and underscores, not starting with a digit or GITHUB_."
                .into(),
        );
    }
    if !(1..=1440).contains(&settings.interval_minutes) {
        return Err("Publish at most every 1 to 1440 minutes.".into());
    }
    Ok(())
}

/// Availability per capability label, from one fleet view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Availability {
    pub online: BTreeMap<String, u32>,
    pub healthy: bool,
}

impl Availability {
    /// The value without its timestamp, to tell whether anything changed.
    pub fn fingerprint(&self) -> String {
        serde_json::json!({ "online": self.online, "healthy": self.healthy }).to_string()
    }

    /// The variable's value.
    pub fn value(&self, now: DateTime<Utc>) -> String {
        serde_json::json!({
            "updatedAt": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "online": self.online,
            "healthy": self.healthy,
        })
        .to_string()
    }
}

/// Capability labels and online counts, or why they can't be trusted now.
pub fn availability(view: &FleetView, now: DateTime<Utc>) -> Result<Availability, String> {
    let Some(organization) = view.plan.organization.as_ref() else {
        return Err("The inventory isn't loaded.".into());
    };
    let org = view
        .orgs
        .iter()
        .find(|o| o.login.eq_ignore_ascii_case(&organization.name))
        .ok_or_else(|| {
            format!(
                "Add {} in Settings ▸ GitHub so runner availability can be read.",
                organization.name
            )
        })?;
    if org.error.is_some() || org.runners_as_of.is_none_or(|at| now - at > GITHUB_STALE_AFTER) {
        return Err("Waiting for fresh GitHub runner data.".into());
    }
    let mut labels: BTreeSet<String> = BTreeSet::new();
    let mut online: BTreeMap<String, u32> = BTreeMap::new();
    for machine in view
        .machines
        .iter()
        .filter(|m| m.desired.as_ref().is_some_and(|d| d.host.enabled))
    {
        for runner in &machine.runners {
            let Some(desired) = runner
                .desired
                .as_ref()
                .filter(|d| matches!(d.role, DesiredRunnerRole::Declared))
            else {
                continue;
            };
            let capabilities = desired
                .desired_labels
                .iter()
                .filter(|l| l.as_str() != "fleet" && !l.starts_with("host-"));
            let is_online = runner.github.as_ref().is_some_and(|g| !g.stale && g.status == "online");
            for label in capabilities {
                labels.insert(label.clone());
                let count = online.entry(label.clone()).or_default();
                if is_online {
                    *count = count.saturating_add(1);
                }
            }
        }
    }
    if labels.is_empty() {
        return Err("The inventory declares no capability labels.".into());
    }
    let healthy = online.values().all(|count| *count > 0);
    Ok(Availability { online, healthy })
}

/// Writes one organization Actions variable. `GhVariables` is the real one.
pub trait ActionsVariables: Send + Sync {
    fn write(&self, organization: &str, name: &str, value: &str) -> Result<(), String>;
}

/// Writes variables through `gh api`, with the `gh` login (which has
/// organization admin for changes). The value travels on stdin.
pub struct GhVariables {
    runner: Arc<dyn CommandRunner>,
    gh: String,
    timeout: Duration,
}

impl std::fmt::Debug for GhVariables {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GhVariables")
            .field("gh", &self.gh)
            .finish_non_exhaustive()
    }
}

impl GhVariables {
    pub fn new(runner: Arc<dyn CommandRunner>, gh: &str, timeout: Duration) -> Self {
        Self {
            runner,
            gh: gh.into(),
            timeout,
        }
    }

    /// The installed `gh`, located like the rest of the app finds it.
    pub fn installed() -> Self {
        let gh = crate::gh::find()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "gh".into());
        Self::new(
            Arc::new(crate::desired::process::SubprocessRunner),
            &gh,
            Duration::from_secs(20),
        )
    }

    fn api(&self, method: &str, endpoint: &str, body: &serde_json::Value) -> Result<(i32, String), String> {
        let command: Vec<String> = [self.gh.as_str(), "api", "-X", method, endpoint, "--input", "-"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let result = self
            .runner
            .run(&command, Some(&body.to_string()), self.timeout, NonZeroExit::Return)
            .map_err(|e| e.to_string())?;
        Ok((result.returncode, format!("{}{}", result.stdout, result.stderr)))
    }
}

impl ActionsVariables for GhVariables {
    fn write(&self, organization: &str, name: &str, value: &str) -> Result<(), String> {
        let update = serde_json::json!({ "name": name, "value": value });
        let (code, output) = self.api(
            "PATCH",
            &format!("orgs/{organization}/actions/variables/{name}"),
            &update,
        )?;
        if code == 0 {
            return Ok(());
        }
        if !output.contains("404") && !output.contains("Not Found") {
            return Err(output.trim().to_string());
        }
        // Private: every private repository can read it, like the runner group.
        let create = serde_json::json!({ "name": name, "value": value, "visibility": "private" });
        let (code, output) = self.api("POST", &format!("orgs/{organization}/actions/variables"), &create)?;
        if code == 0 {
            Ok(())
        } else {
            Err(output.trim().to_string())
        }
    }
}
