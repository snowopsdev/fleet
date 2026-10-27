//! Engine side of automation: deferred drain stops, the opt-in watchdog, the
//! availability variable, and alert destinations with their delivery.
//!
//! None of it runs in demo mode. Each automatic change takes the same change
//! slot as reviewed changes, so it never overlaps one, and is recorded in
//! machine history before and after it acts.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};

use super::{Engine, EngineError};
use crate::automation::watchdog::{self, Decision, MAX_ATTEMPTS};
use crate::automation::{
    AutomationSettings, DrainMark, DrainState, FleetAutomation, FleetPublishStatus, FleetWatchdog, PublishSettings,
    publish,
};
use crate::desired::maintenance::StopOutcome;
use crate::desired::model::{Inventory, is_active};
use crate::desired::operations::{Ops, Secrets};
use crate::model::{AppConfig, FleetView, MachineView, RunnerView};
use crate::monitoring::{FleetAlertKind, FleetEventKind, FleetMachineAlert};
use crate::outbound::{self, AlertDestination, AlertDestinationInput, AlertDestinationKind, DestinationSecret};
use crate::sync::MutexExt;

/// Runs blocking work off the async runtime.
async fn blocking<T: Send + 'static>(what: &str, work: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| format!("Couldn't {what}: {e}"))
}

/// Holds the change slot for automatic work; released however it ends.
struct Slot<'a>(&'a Engine);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.desired.locked().release();
    }
}

/// The machine and runner view for an inventory runner, by host and name.
fn find_runner<'v>(view: &'v FleetView, host: &str, name: &str) -> Option<(&'v MachineView, &'v RunnerView)> {
    view.machines
        .iter()
        .filter(|m| m.desired.as_ref().is_some_and(|d| d.host.id == host))
        .find_map(|m| m.runners.iter().find(|r| r.name == name).map(|r| (m, r)))
}

fn demo_refused() -> EngineError {
    EngineError::Invalid("Turn off demo data to change automatic actions and alert destinations.".into())
}

static DESTINATION_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Engine {
    /// Replace the organization-variable writer (tests).
    pub fn with_actions_variables(mut self, variables: Arc<dyn publish::ActionsVariables>) -> Self {
        self.variables = variables;
        self
    }

    /// Puts drains, watchdog state, destinations and the publish status on the
    /// live view, with an alert for each runner the watchdog gave up on.
    pub(super) fn attach_automation(&self, view: &mut FleetView, config: &AppConfig, now: DateTime<Utc>) {
        let settings = &config.automation;
        let store = self.automation.locked();
        let journal = store.journal();
        let destinations = {
            let monitoring = self.monitoring.locked();
            settings
                .alert_destinations
                .iter()
                .map(|d| monitoring.destination_view(d))
                .collect()
        };
        let publish = &settings.publish;
        view.automation = FleetAutomation {
            drains: journal.drains.values().map(DrainMark::view).collect(),
            watchdog: FleetWatchdog {
                enabled: settings.watchdog,
                runners: journal.watchdog.iter().map(|(name, s)| s.view(name)).collect(),
            },
            destinations,
            publish: FleetPublishStatus {
                enabled: publish.enabled,
                organization: view.plan.organization.as_ref().map(|o| o.name.clone()),
                variable: publish.variable.clone(),
                interval_minutes: publish.interval_minutes,
                value: journal.publish.published_value.clone(),
                published_at: journal.publish.published_at,
                error: journal.publish.error.clone().or_else(|| store.storage_error.clone()),
                waiting: journal.publish.waiting.clone(),
            },
        };
        let mut alerts = Vec::new();
        for (name, state) in &journal.watchdog {
            let Some(since) = state.gave_up_at else { continue };
            let Some((machine, _)) = find_runner(view, &state.host, name) else {
                continue;
            };
            let key = machine.preferences_key();
            let alert = FleetMachineAlert {
                id: format!("{key}:watchdog:{name}"),
                machine_key: key,
                machine_name: machine.display_name.clone(),
                kind: FleetAlertKind::WatchdogGaveUp,
                since,
                detail: format!(
                    "Automatic restarts of {name} stopped after {MAX_ATTEMPTS} attempts in {} hours. Check its diagnostic log, then restart it from its pane.",
                    watchdog::WINDOW_HOURS
                ),
                stale: false,
                muted: machine.monitoring.maintenance_until.is_some_and(|until| until > now),
            };
            alerts.push((machine.id.clone(), alert));
        }
        for (id, alert) in alerts {
            if let Some(m) = view.machines.iter_mut().find(|m| m.id == id) {
                m.monitoring.alerts.push(alert.clone());
            }
            view.monitoring.alerts.push(alert);
        }
    }

    /// Enabled alert destinations' IDs.
    fn destination_ids(config: &AppConfig) -> BTreeSet<String> {
        config
            .automation
            .alert_destinations
            .iter()
            .filter(|d| d.enabled)
            .map(|d| d.id.clone())
            .collect()
    }

    /// Records an event the app caused in the machine's history, queueing a
    /// notification when `notify` is set. Disk I/O; history failures are logged
    /// and shown as the history storage error.
    fn record_history(&self, machine: &MachineView, kind: FleetEventKind, detail: String, notify: bool) {
        let config = self.config();
        let result = self.monitoring.locked().record_external(
            &machine.preferences_key(),
            &machine.display_name,
            kind,
            detail,
            notify,
            config.desktop_notifications,
            &Self::destination_ids(&config),
            Utc::now(),
        );
        if let Err(e) = result {
            tracing::warn!("recording an automatic change in history failed: {e}");
        }
    }

    /// Records `kind` for the inventory runner `name` on `host`, if it's listed.
    fn record_runner_history(&self, host: &str, name: &str, kind: FleetEventKind, detail: String, notify: bool) {
        let view = self.view();
        if let Some((machine, _)) = find_runner(&view, host, name) {
            self.record_history(machine, kind, detail, notify);
        }
    }

    fn update_automation<T>(
        &self,
        change: impl FnOnce(&mut crate::automation::AutomationJournal) -> T,
    ) -> Result<T, EngineError> {
        let result = self.automation.locked().update(change).map_err(EngineError::Storage);
        self.view_changed();
        result
    }

    /// After a reviewed drain ran: a stopped runner is marked stopped; a busy
    /// one is marked draining, and its stop happens once it is idle.
    pub(super) fn record_drain(&self, host: &str, runner: &str, path: String, review: &str, outcome: &StopOutcome) {
        let now = Utc::now();
        let (registration, state, detail, kind) = match outcome {
            StopOutcome::Stopped { registration, .. } => (
                *registration,
                DrainState::Stopped,
                format!("Stopped {runner}'s service, as reviewed. It stays stopped until it's resumed."),
                FleetEventKind::Drained,
            ),
            StopOutcome::Busy { registration } => (
                *registration,
                DrainState::Draining,
                format!("Draining {runner}: its service stops after the current job, as reviewed."),
                FleetEventKind::DrainRequested,
            ),
        };
        let mark = DrainMark {
            runner: runner.to_string(),
            host: host.to_string(),
            path,
            registration,
            review: review.to_string(),
            requested_at: now,
            state,
            stopped_at: (state == DrainState::Stopped).then_some(now),
            last_error: None,
            checked_at: Some(now),
        };
        if let Err(e) = self.update_automation(|j| j.drains.insert(runner.to_string(), mark)) {
            tracing::warn!("saving the drain of {runner} failed: {e}");
        }
        self.record_runner_history(host, runner, kind, detail, false);
    }

    /// After a reviewed resume: the runner is no longer held stopped.
    pub(super) fn record_resume(&self, host: &str, runner: &str) {
        if let Err(e) = self.update_automation(|j| j.drains.remove(runner)) {
            tracing::warn!("clearing the drain of {runner} failed: {e}");
        }
        self.record_runner_history(
            host,
            runner,
            FleetEventKind::Resumed,
            format!("Started {runner}'s service, as reviewed."),
            false,
        );
    }

    /// Whether a drain holds `runner`, for reviews.
    pub(super) fn drain_state(&self, runner: &str) -> Option<DrainState> {
        self.automation.locked().journal().drains.get(runner).map(|m| m.state)
    }

    /// Cancels a drain that hasn't stopped its runner yet. Nothing on the
    /// machine changes.
    pub fn cancel_drain(&self, runner: &str) -> Result<(), EngineError> {
        if self.config().demo_mode {
            return Err(demo_refused());
        }
        let removed = self.update_automation(|j| {
            if j.drains.get(runner).is_some_and(|m| m.state == DrainState::Draining) {
                j.drains.remove(runner)
            } else {
                None
            }
        })?;
        let Some(mark) = removed else {
            return Err(EngineError::NotFound(format!(
                "{runner} isn't draining. A stopped runner is started again with Resume…."
            )));
        };
        self.record_runner_history(
            &mark.host,
            runner,
            FleetEventKind::DrainCancelled,
            format!("Cancelled the drain of {runner}; its service keeps running."),
            false,
        );
        Ok(())
    }

    /// Turns the watchdog on or off. Either way its history starts afresh.
    pub fn set_watchdog(&self, enabled: bool) -> Result<(), EngineError> {
        if self.config().demo_mode {
            return Err(demo_refused());
        }
        self.update_config(|c| c.automation.watchdog = enabled)?;
        self.update_automation(|j| j.watchdog.clear())?;
        Ok(())
    }

    /// Saves the availability-variable settings; a changed target publishes afresh.
    pub fn set_publish_settings(&self, settings: PublishSettings) -> Result<(), EngineError> {
        if self.config().demo_mode {
            return Err(demo_refused());
        }
        publish::check_settings(&settings).map_err(EngineError::Invalid)?;
        let before = self.config().automation.publish;
        self.update_config(|c| c.automation.publish = settings.clone())?;
        if before.variable != settings.variable || before.enabled != settings.enabled {
            self.update_automation(|j| j.publish = Default::default())?;
        }
        Ok(())
    }

    /// Deferred drains, the watchdog and the availability variable, in that
    /// order. Called periodically by the app; does nothing in demo mode.
    pub async fn run_automation(self: &Arc<Self>) {
        if self.config().demo_mode {
            return;
        }
        self.process_drains().await;
        self.run_watchdog(Utc::now()).await;
        self.publish_availability(Utc::now()).await;
    }

    /// Takes the change slot unless a change is running.
    fn reserve(&self) -> Option<Slot<'_>> {
        self.desired.locked().reserve().then_some(Slot(self))
    }

    fn ops_secrets() -> Secrets {
        // Deferred and automatic changes never have a password: none is stored.
        Secrets::default()
    }

    /// Carries out reviewed drains whose runners are idle now, and forgets
    /// stopped ones whose service is running again.
    async fn process_drains(self: &Arc<Self>) {
        let marks: Vec<DrainMark> = self.automation.locked().journal().drains.values().cloned().collect();
        if marks.is_empty() {
            return;
        }
        let view = self.view();
        for mark in marks {
            let Some((machine, runner)) = find_runner(&view, &mark.host, &mark.runner) else {
                continue;
            };
            let local = runner.local.as_ref().filter(|l| !l.stale && l.configured);
            match mark.state {
                DrainState::Stopped => {
                    // Only a read taken after the stop counts as "running again".
                    let since = mark.stopped_at.unwrap_or(mark.requested_at);
                    if local.is_some_and(|l| l.as_of > since && is_active(&l.service_state))
                        && self.update_automation(|j| j.drains.remove(&mark.runner)).is_ok()
                    {
                        self.record_history(
                            machine,
                            FleetEventKind::DrainCancelled,
                            format!(
                                "{}'s service is running again (the machine may have restarted); it's no longer held stopped.",
                                mark.runner
                            ),
                            false,
                        );
                    }
                }
                DrainState::Draining => {
                    // Fresh GitHub data showing it idle triggers the stop, which reads
                    // GitHub again and refuses a busy runner. Otherwise GitHub is asked
                    // directly at most once a minute.
                    let busy = runner.github.as_ref().is_none_or(|g| g.stale || g.busy);
                    let due = mark
                        .checked_at
                        .is_none_or(|at| Utc::now() - at >= chrono::Duration::minutes(1));
                    if !busy || due {
                        self.finish_drain(machine, &mark).await;
                    }
                }
            }
        }
    }

    /// Stops a draining runner that GitHub shows idle, if it still is the
    /// runner the drain was reviewed for.
    async fn finish_drain(self: &Arc<Self>, machine: &MachineView, mark: &DrainMark) {
        let Some(slot) = self.reserve() else { return };
        let inventory = match self.read_inventory().await {
            Ok(inventory) => inventory,
            Err(e) => {
                self.drain_error(machine, mark, e.to_string());
                return;
            }
        };
        let same = inventory
            .find_host(&mark.host)
            .ok()
            .and_then(|h| {
                h.desired_runners()
                    .find(|r| r.name == mark.runner)
                    .map(|r| h.runner_path(r))
            })
            .is_some_and(|path| path == mark.path);
        if !same {
            self.cancel_changed_drain(machine, mark, "the inventory changed since the drain was reviewed");
            return;
        }
        let (github, remote) = (self.desired_io.boundaries)(&inventory);
        let sleep = self.desired_io.sleep;
        let name = mark.runner.clone();
        let registration = mark.registration;
        let result = blocking("stop the drained runner", move || {
            let ops = Ops {
                inventory: &inventory,
                github: github.as_ref(),
                remote: remote.as_ref(),
                secrets: Self::ops_secrets(),
                sleep: &sleep,
            };
            ops.stop_if_idle(&name, Some(registration)).map_err(|e| e.0)
        })
        .await
        .and_then(|r| r);
        match result {
            Ok(StopOutcome::Busy { .. }) => {
                let runner = mark.runner.clone();
                // Only throttles the next direct check; losing it costs one extra GitHub read.
                let _ = self.update_automation(|j| {
                    if let Some(m) = j.drains.get_mut(&runner) {
                        m.checked_at = Some(Utc::now());
                    }
                });
            }
            Ok(StopOutcome::Stopped { .. }) => {
                let runner = mark.runner.clone();
                let saved = self.update_automation(|j| {
                    if let Some(m) = j.drains.get_mut(&runner) {
                        m.state = DrainState::Stopped;
                        m.stopped_at = Some(Utc::now());
                        m.last_error = None;
                    }
                });
                if saved.is_ok() {
                    self.record_history(
                        machine,
                        FleetEventKind::Drained,
                        format!(
                            "Stopped {}'s service after its job finished, as reviewed. It stays stopped until it's resumed.",
                            mark.runner
                        ),
                        false,
                    );
                }
                drop(slot);
                // Show the stopped service promptly.
                if let Err(e) = self.desired_refresh_wait(Some(mark.host.clone())).await {
                    tracing::info!("check after a drain failed: {e}");
                }
            }
            Err(e) if e.contains("registered again") => self.cancel_changed_drain(machine, mark, &e),
            Err(e) => self.drain_error(machine, mark, e),
        }
    }

    fn cancel_changed_drain(&self, machine: &MachineView, mark: &DrainMark, why: &str) {
        if self.update_automation(|j| j.drains.remove(&mark.runner)).is_ok() {
            self.record_history(
                machine,
                FleetEventKind::DrainCancelled,
                format!("Cancelled the drain of {}: {why}. Nothing was stopped.", mark.runner),
                true,
            );
        }
    }

    /// Keeps the drain and records why it couldn't stop the runner, once per distinct error.
    fn drain_error(&self, machine: &MachineView, mark: &DrainMark, error: String) {
        if mark.last_error.as_deref() == Some(error.as_str()) {
            return;
        }
        let runner = mark.runner.clone();
        let saved = self.update_automation(|j| {
            if let Some(m) = j.drains.get_mut(&runner) {
                m.last_error = Some(error.clone());
            }
        });
        if saved.is_ok() {
            self.record_history(
                machine,
                FleetEventKind::DrainRequested,
                format!("Couldn't stop {} yet: {error} The drain stays in place.", mark.runner),
                false,
            );
        }
    }

    /// One watchdog round: restart qualifying runners, give up at the limit,
    /// and note recoveries. Only when the user turned it on.
    pub(crate) async fn run_watchdog(self: &Arc<Self>, now: DateTime<Utc>) {
        let config = self.config();
        if config.demo_mode || !config.automation.watchdog {
            return;
        }
        let Some(slot) = self.reserve() else { return };
        let view = self.view();
        let decisions = match self.update_automation(|j| watchdog::plan(&view, j, now)) {
            Ok(decisions) => decisions,
            Err(e) => {
                tracing::warn!("watchdog state couldn't be saved; skipping this round: {e}");
                return;
            }
        };
        if decisions.is_empty() {
            return;
        }
        let inventory = self.read_inventory().await;
        for decision in decisions {
            match decision {
                Decision::Restart { subject, attempt } => {
                    let Some((machine, _)) = find_runner(&view, &subject.host, &subject.runner) else {
                        continue;
                    };
                    let result = match &inventory {
                        Ok(inventory) => self.watchdog_restart(inventory, &subject.runner).await,
                        Err(e) => Err(e.to_string()),
                    };
                    match result {
                        Ok(()) => self.record_history(
                            machine,
                            FleetEventKind::WatchdogRestarted,
                            format!(
                                "Automatically restarted {} (attempt {attempt} of {MAX_ATTEMPTS}): its service was {} and GitHub showed it offline and idle.",
                                subject.runner, subject.service_state
                            ),
                            true,
                        ),
                        Err(e) => {
                            let runner = subject.runner.clone();
                            let error = e.clone();
                            // Only the displayed last error; the attempt itself was saved before it ran.
                            let _ = self.update_automation(|j| {
                                if let Some(s) = j.watchdog.get_mut(&runner) {
                                    s.last_error = Some(error);
                                }
                            });
                            self.record_history(
                                machine,
                                FleetEventKind::WatchdogRestartFailed,
                                format!(
                                    "Automatic restart of {} failed (attempt {attempt} of {MAX_ATTEMPTS}): {e}",
                                    subject.runner
                                ),
                                true,
                            );
                        }
                    }
                }
                Decision::GiveUp { subject } => {
                    if let Some((machine, _)) = find_runner(&view, &subject.host, &subject.runner) {
                        self.record_history(
                            machine,
                            FleetEventKind::WatchdogGaveUp,
                            format!(
                                "Stopped restarting {} automatically after {MAX_ATTEMPTS} attempts in {} hours; its service is still {}. Check its diagnostic log, then restart it from its pane.",
                                subject.runner,
                                watchdog::WINDOW_HOURS,
                                subject.service_state
                            ),
                            true,
                        );
                    }
                }
                Decision::Recovered { subject, gave_up } => {
                    if let Some((machine, _)) = find_runner(&view, &subject.host, &subject.runner) {
                        self.record_history(
                            machine,
                            FleetEventKind::WatchdogRecovered,
                            format!("{} is running again after an automatic restart.", subject.runner),
                            gave_up,
                        );
                    }
                }
            }
        }
        drop(slot);
        self.view_changed();
    }

    /// Restarts one runner through the same guarded operation as a reviewed
    /// restart: GitHub is read again and a busy runner is refused.
    async fn watchdog_restart(&self, inventory: &Inventory, runner: &str) -> Result<(), String> {
        let inventory = inventory.clone();
        let (github, remote) = (self.desired_io.boundaries)(&inventory);
        let sleep = self.desired_io.sleep;
        let name = runner.to_string();
        blocking("restart the runner", move || {
            let ops = Ops {
                inventory: &inventory,
                github: github.as_ref(),
                remote: remote.as_ref(),
                secrets: Self::ops_secrets(),
                sleep: &sleep,
            };
            ops.restart_runner(&name).map(drop).map_err(|e| e.0)
        })
        .await?
    }

    /// Writes the availability variable when it changed and the interval passed.
    pub(crate) async fn publish_availability(&self, now: DateTime<Utc>) {
        let config = self.config();
        let settings = config.automation.publish.clone();
        if config.demo_mode || !settings.enabled {
            return;
        }
        let view = self.view();
        let availability = publish::availability(&view, now);
        let Some(organization) = view.plan.organization.as_ref().map(|o| o.name.clone()) else {
            return;
        };
        let target = format!("{organization}/{}", settings.variable);
        let availability = match availability {
            Ok(a) => a,
            Err(waiting) => {
                // Display only; a failed save shows as the automation storage error.
                let _ = self.update_automation(|j| j.publish.waiting = Some(waiting));
                return;
            }
        };
        let fingerprint = availability.fingerprint();
        let due = {
            let store = self.automation.locked();
            let state = &store.journal().publish;
            let changed =
                state.value.as_deref() != Some(fingerprint.as_str()) || state.target.as_deref() != Some(&target);
            let interval = chrono::Duration::minutes(i64::from(settings.interval_minutes));
            changed && state.attempted_at.is_none_or(|at| now - at >= interval)
        };
        if !due {
            // Display only; a failed save shows as the automation storage error.
            let _ = self.update_automation(|j| j.publish.waiting = None);
            return;
        }
        if self
            .update_automation(|j| {
                j.publish.attempted_at = Some(now);
                j.publish.waiting = None;
            })
            .is_err()
        {
            return;
        }
        let variables = self.variables.clone();
        let published = availability.value(now);
        let value = published.clone();
        let name = settings.variable.clone();
        let result = blocking("publish fleet availability", move || {
            variables.write(&organization, &name, &value)
        })
        .await
        .and_then(|r| r);
        // A failed save leaves the old fingerprint, so the next round writes the same value again.
        let _ = self.update_automation(|j| match result {
            Ok(()) => {
                j.publish.value = Some(fingerprint);
                j.publish.published_value = Some(published);
                j.publish.target = Some(target);
                j.publish.published_at = Some(now);
                j.publish.error = None;
            }
            Err(e) => j.publish.error = Some(format!("Couldn't write {target}: {e}")),
        });
    }

    // Alert destinations.

    async fn read_secret(&self, id: &str) -> Result<DestinationSecret, String> {
        let secrets = self.secrets.clone();
        let account = outbound::secret_account(id);
        let stored = blocking("read the Keychain", move || secrets.get(&account)).await??;
        match stored {
            Some(text) => serde_json::from_str(&text)
                .map_err(|_| "The saved destination secret is unreadable; edit the destination.".into()),
            None => Ok(DestinationSecret::default()),
        }
    }

    async fn write_secret(&self, id: &str, secret: &DestinationSecret) -> Result<(), EngineError> {
        let secrets = self.secrets.clone();
        let account = outbound::secret_account(id);
        let text = serde_json::to_string(secret).map_err(|e| EngineError::Storage(e.to_string()))?;
        blocking("save to the Keychain", move || secrets.set(&account, &text))
            .await
            .map_err(EngineError::Storage)?
            .map_err(EngineError::Storage)
    }

    /// Adds or edits an alert destination. Its URL and token go to the
    /// Keychain; the settings keep only the kind, name, ntfy server and topic.
    pub async fn save_alert_destination(&self, input: AlertDestinationInput) -> Result<String, EngineError> {
        if self.config().demo_mode {
            return Err(demo_refused());
        }
        let name = input.name.trim().to_string();
        if name.is_empty() || name.chars().count() > 60 {
            return Err(EngineError::Invalid(
                "Give the destination a name of up to 60 characters.".into(),
            ));
        }
        let existing = input
            .id
            .as_ref()
            .map(|id| {
                self.config()
                    .automation
                    .alert_destinations
                    .into_iter()
                    .find(|d| &d.id == id)
                    .ok_or_else(|| EngineError::NotFound("That destination was removed. Add it again.".into()))
            })
            .transpose()?;
        let kind = existing
            .as_ref()
            .map(|d| d.kind)
            .or(input.kind)
            .ok_or_else(|| EngineError::Invalid("Choose a destination type.".into()))?;
        let id = existing.as_ref().map_or_else(
            || {
                format!(
                    "dest-{}-{}",
                    Utc::now().timestamp_millis(),
                    DESTINATION_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                )
            },
            |d| d.id.clone(),
        );
        let mut secret = match &existing {
            Some(_) => self.read_secret(&id).await.unwrap_or_default(),
            None => DestinationSecret::default(),
        };
        let (summary, ntfy_server, ntfy_topic) = match kind {
            AlertDestinationKind::Ntfy => {
                // When editing, an empty server or topic keeps the saved one.
                let given = |value: &Option<String>| {
                    value
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                };
                let server = given(&input.ntfy_server)
                    .or_else(|| existing.as_ref().and_then(|d| d.ntfy_server.clone()))
                    .unwrap_or_else(|| "https://ntfy.sh".into());
                let url = outbound::check_url(&server).map_err(EngineError::Invalid)?;
                let topic = given(&input.ntfy_topic)
                    .or_else(|| existing.as_ref().and_then(|d| d.ntfy_topic.clone()))
                    .unwrap_or_default();
                outbound::check_topic(&topic).map_err(EngineError::Invalid)?;
                if let Some(token) = &input.token {
                    secret.token = Some(token.trim().to_string()).filter(|t| !t.is_empty());
                }
                (
                    outbound::summary(kind, Some(&url), Some(&topic)),
                    Some(url.as_str().trim_end_matches('/').to_string()),
                    Some(topic),
                )
            }
            AlertDestinationKind::Slack | AlertDestinationKind::Discord | AlertDestinationKind::Webhook => {
                let url = match input.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
                    Some(url) => {
                        let parsed = outbound::check_url(url).map_err(EngineError::Invalid)?;
                        secret.url = Some(parsed.to_string());
                        parsed
                    }
                    None => secret
                        .url
                        .as_deref()
                        .and_then(|u| outbound::check_url(u).ok())
                        .ok_or_else(|| EngineError::Invalid("Paste the webhook URL.".into()))?,
                };
                (outbound::summary(kind, Some(&url), None), None, None)
            }
        };
        self.write_secret(&id, &secret).await?;
        let destination = AlertDestination {
            id: id.clone(),
            name,
            kind,
            enabled: input.enabled,
            ntfy_server,
            ntfy_topic,
            summary,
        };
        let saved = self.update_config(|c| {
            let list = &mut c.automation.alert_destinations;
            match list.iter_mut().find(|d| d.id == destination.id) {
                Some(d) => *d = destination,
                None => list.push(destination),
            }
        });
        if let Err(e) = saved {
            if existing.is_none() {
                let secrets = self.secrets.clone();
                let account = outbound::secret_account(&id);
                // The new destination wasn't saved, so its secret is orphaned; removing it is best effort.
                let _ = blocking("remove from the Keychain", move || secrets.delete(&account)).await;
            }
            return Err(e);
        }
        self.view_changed();
        Ok(id)
    }

    /// Removes a destination, its Keychain secret and its undelivered alerts.
    pub async fn remove_alert_destination(self: &Arc<Self>, id: &str) -> Result<(), EngineError> {
        if self.config().demo_mode {
            return Err(demo_refused());
        }
        self.update_config(|c| c.automation.alert_destinations.retain(|d| d.id != id))?;
        let secrets = self.secrets.clone();
        let account = outbound::secret_account(id);
        blocking("remove from the Keychain", move || secrets.delete(&account))
            .await
            .map_err(EngineError::Storage)?
            .map_err(EngineError::Storage)?;
        let engine = self.clone();
        let destination = id.to_string();
        blocking("update the alert queue", move || {
            engine.monitoring.locked().discard_outbound(&destination)
        })
        .await
        .map_err(EngineError::Storage)?
        .map_err(EngineError::Storage)?;
        self.view_changed();
        Ok(())
    }

    /// Sends a test message to one destination now, outside the queue.
    pub async fn test_alert_destination(&self, id: &str) -> Result<(), EngineError> {
        if self.config().demo_mode {
            return Err(EngineError::Invalid(
                "Turn off demo data to test a real destination; demo destinations are fictional.".into(),
            ));
        }
        let destination = self
            .config()
            .automation
            .alert_destinations
            .into_iter()
            .find(|d| d.id == id)
            .ok_or_else(|| EngineError::NotFound("That destination was removed.".into()))?;
        let secret = self.read_secret(id).await.map_err(EngineError::Invalid)?;
        let request =
            outbound::request(&destination, &secret, &outbound::test_notice()).map_err(EngineError::Invalid)?;
        let client = outbound::client().map_err(EngineError::Invalid)?;
        outbound::send(&client, request).await.map_err(EngineError::Invalid)
    }

    /// Delivers queued alerts that are due to their destinations, and records
    /// the outcome. Called periodically by the app; never in demo mode.
    pub async fn deliver_alerts(self: &Arc<Self>) {
        let config = self.config();
        if config.demo_mode {
            return;
        }
        let now = Utc::now();
        let destinations: BTreeMap<String, AlertDestination> = config
            .automation
            .alert_destinations
            .into_iter()
            .filter(|d| d.enabled)
            .map(|d| (d.id.clone(), d))
            .collect();
        if destinations.is_empty() {
            return;
        }
        let engine = self.clone();
        let claimed = match blocking("read the alert queue", move || {
            engine.monitoring.locked().claim_outbound(now, 20)
        })
        .await
        {
            Ok(Ok(claimed)) => claimed,
            Ok(Err(e)) | Err(e) => {
                tracing::warn!("claiming queued alerts failed: {e}");
                return;
            }
        };
        if claimed.is_empty() {
            return;
        }
        let client = match outbound::client() {
            Ok(client) => client,
            Err(e) => {
                tracing::warn!("{e}");
                return;
            }
        };
        let mut results = Vec::new();
        for delivery in claimed {
            let result = match destinations.get(&delivery.destination) {
                None => Err("The destination is turned off.".to_string()),
                Some(destination) => match self.read_secret(&destination.id).await {
                    Ok(secret) => match outbound::request(destination, &secret, &outbound::notice(&delivery.event)) {
                        Ok(request) => outbound::send(&client, request).await,
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                },
            };
            results.push((delivery, result));
        }
        let engine = self.clone();
        match blocking("save alert deliveries", move || {
            engine.monitoring.locked().finish_outbound(&results, Utc::now())
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) | Err(e) => tracing::warn!("saving alert deliveries failed: {e}"),
        }
        self.view_changed();
    }

    /// Settings for automatic behaviour, as saved.
    pub fn automation_settings(&self) -> AutomationSettings {
        self.config().automation
    }
}
