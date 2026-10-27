//! Bounded local persistence and projection into the fleet view.

use super::state::{MachineState, target};
use super::{FleetEventKind, FleetMachineEvent, FleetMachineMonitoring};
use crate::model::{DataSource, FleetView, MachineConnection, MachineView, tailnet_machine_key};
use crate::outbound::{AlertDestination, FleetAlertDestinationView};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

pub(crate) const EVENT_RETENTION: Duration = Duration::days(30);
pub(crate) const MAX_EVENTS: usize = 5_000;
/// Events per page, and the latest events shown with each machine.
pub(crate) const EVENT_PAGE_SIZE: usize = 50;
// Bound delayed delivery independently of history; retain the newest requests.
const MAX_NOTIFICATIONS: usize = 256;
/// Undelivered alerts kept per destination; older ones are dropped with an event.
pub(crate) const MAX_OUTBOUND_PER_DESTINATION: usize = 100;
const RETRY_INTERVAL: Duration = Duration::minutes(1);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Journal {
    version: u32,
    next_id: u64,
    machines: BTreeMap<String, MachineState>,
    events: VecDeque<FleetMachineEvent>,
    #[serde(default)]
    pending_notifications: VecDeque<FleetMachineEvent>,
    #[serde(default)]
    notification_retry_at: Option<DateTime<Utc>>,
    /// Alerts saved for the alert destinations, delivered and retried by the engine.
    #[serde(default)]
    outbound: VecDeque<OutboundDelivery>,
    /// The last delivery outcome per destination.
    #[serde(default)]
    outbound_status: BTreeMap<String, DeliveryStatus>,
}

/// One alert waiting for one destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutboundDelivery {
    pub destination: String,
    pub event: FleetMachineEvent,
    pub attempts: u32,
    /// Not before: a claimed delivery is pushed out so a restart retries it later.
    pub next_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct DeliveryStatus {
    delivered_at: Option<DateTime<Utc>>,
    error: Option<String>,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            version: 1,
            next_id: 1,
            machines: BTreeMap::new(),
            events: VecDeque::new(),
            pending_notifications: VecDeque::new(),
            notification_retry_at: None,
            outbound: VecDeque::new(),
            outbound_status: BTreeMap::new(),
        }
    }
}

/// Writes only this dashboard's local file; malformed existing files are preserved.
#[derive(Debug)]
pub(crate) struct MonitorStore {
    path: PathBuf,
    journal: Journal,
    pub storage_error: Option<String>,
    writable: bool,
    saved_at: Option<DateTime<Utc>>,
}

impl MonitorStore {
    pub fn load(path: PathBuf) -> Self {
        let result = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Journal>(&bytes)
                .map_err(|e| e.to_string())
                .and_then(|j| {
                    if j.version == 1 {
                        Ok(j)
                    } else {
                        Err(format!("Unsupported history version {}.", j.version))
                    }
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Journal::default()),
            Err(e) => Err(e.to_string()),
        };
        let writable = result.is_ok();
        let (journal, storage_error) = match result {
            Ok(journal) => (journal, None),
            Err(e) => (
                Journal::default(),
                Some(format!(
                    "Couldn't read machine history: {e} The existing file has been preserved; new events cannot be saved."
                )),
            ),
        };
        Self {
            path,
            journal,
            storage_error,
            writable,
            saved_at: None,
        }
    }

    pub fn record(&mut self, view: &FleetView, now: DateTime<Utc>) -> Vec<FleetMachineEvent> {
        if view.source == DataSource::Demo {
            return Vec::new();
        }
        let network_at = fresh_network(view, now);
        let mut changed = false;
        let pending_before = self.journal.pending_notifications.len();
        let eligible: BTreeSet<_> = view
            .machines
            .iter()
            .chain(&view.other_machines)
            .filter(|m| !matches!(m.connection, MachineConnection::Disabled))
            .flat_map(|m| [m.preferences_key(), m.id.clone()])
            .collect();
        let disabled: BTreeSet<_> = view
            .machines
            .iter()
            .chain(&view.other_machines)
            .filter(|m| matches!(m.connection, MachineConnection::Disabled))
            .flat_map(|m| [m.preferences_key(), m.id.clone()])
            .chain(view.ignored.iter().map(|p| tailnet_machine_key(&p.peer.node_id)))
            .collect();
        // Absence is authoritative only with a current discovery and a loaded inventory.
        // Failed or pending inputs can temporarily hide machines without removing them.
        let complete = network_at.is_some() && view.plan.organization.is_some() && view.plan.inventory_error.is_none();
        self.journal
            .pending_notifications
            .retain(|e| !disabled.contains(&e.machine_key) && (!complete || eligible.contains(&e.machine_key)));
        let destinations = enabled_destinations(view);
        let outbound_before = self.journal.outbound.len();
        self.journal.outbound.retain(|d| {
            destinations.contains(&d.destination)
                && !disabled.contains(&d.event.machine_key)
                && (!complete || eligible.contains(&d.event.machine_key))
        });
        changed |= self.journal.outbound.len() != outbound_before;
        if !view.monitoring.desktop_notifications && !self.journal.pending_notifications.is_empty() {
            self.journal.pending_notifications.clear();
            self.journal.notification_retry_at = None;
            changed = true;
        }
        for machine in view.machines.iter().chain(&view.other_machines) {
            if matches!(machine.connection, MachineConnection::Disabled) {
                continue;
            }
            // Wait for evidence. Loading an inventory is not a machine observation.
            let key = self.key(machine);
            if (network_at.is_none() || machine.tailnet.is_none())
                && machine.health.as_ref().is_none_or(|h| h.stale)
                && !self.journal.machines.contains_key(&key)
            {
                continue;
            }
            let target = target(machine);
            let mut state = self.journal.machines.remove(&key).unwrap_or_default();
            if state.target.is_empty() || state.target != target {
                self.journal.pending_notifications.retain(|e| e.machine_key != key);
                let kind = if state.target.is_empty() {
                    FleetEventKind::MonitoringStarted
                } else {
                    FleetEventKind::TargetChanged
                };
                let detail = if state.target.is_empty() {
                    "Started observing this machine. Earlier events are not known."
                } else {
                    "The SSH target changed. Alert and boot baselines have been reset."
                };
                state = MachineState {
                    target,
                    maintenance_until: state.maintenance_until,
                    ..Default::default()
                };
                self.event(&key, &machine.display_name, now, kind, detail.into());
                changed = true;
            }
            for change in state.observe(machine, network_at, now) {
                let event = self.event(&key, &machine.display_name, now, change.kind, change.detail);
                if change.notify {
                    self.queue_outbound(&destinations, &event, now);
                }
                if change.notify && view.monitoring.desktop_notifications {
                    self.journal.pending_notifications.push_back(event);
                }
                changed = true;
            }
            self.journal.machines.insert(key, state);
        }
        let cutoff = now - EVENT_RETENTION;
        self.journal.events.retain(|e| e.at >= cutoff);
        while self.journal.events.len() > MAX_EVENTS {
            self.journal.events.pop_front();
        }
        self.journal.pending_notifications.retain(|e| e.at >= cutoff);
        while self.journal.pending_notifications.len() > MAX_NOTIFICATIONS {
            self.journal.pending_notifications.pop_front();
        }
        changed |= self.cap_outbound(now);
        changed |= self.journal.pending_notifications.len() != pending_before;
        if self.journal.pending_notifications.is_empty() {
            changed |= self.journal.notification_retry_at.is_some();
            self.journal.notification_retry_at = None;
        }
        self.journal
            .machines
            .retain(|_, m| m.last_seen.is_none_or(|at| at >= cutoff));
        let dispatch = self
            .journal
            .pending_notifications
            .iter()
            .any(|e| eligible.contains(&e.machine_key))
            && self.journal.notification_retry_at.is_none_or(|at| now >= at);
        let retry_at = self.journal.notification_retry_at;
        if dispatch {
            // Persist the throttle before asking the native API, including across restarts.
            self.journal.notification_retry_at = Some(now + RETRY_INTERVAL);
        }
        // Checkpoint conditions once a minute; transitions and requests are durable before notification.
        if changed
            || dispatch
            || self.storage_error.is_some()
            || self.saved_at.is_none_or(|at| now - at >= Duration::minutes(1))
        {
            if let Err(e) = self.save() {
                self.journal.notification_retry_at = retry_at;
                self.storage_error = Some(e);
                return Vec::new();
            }
            self.saved_at = Some(now);
            self.storage_error = None;
            if dispatch {
                return self
                    .journal
                    .pending_notifications
                    .iter()
                    .filter(|e| eligible.contains(&e.machine_key))
                    .cloned()
                    .collect();
            }
        }
        Vec::new()
    }

    /// Acknowledge acceptance by the native request API, not display by the OS.
    pub fn acknowledge_notifications(&mut self, events: &[FleetMachineEvent]) -> Result<(), String> {
        let original = self.journal.clone();
        self.journal
            .pending_notifications
            .retain(|pending| !events.iter().any(|e| e.id == pending.id));
        if self.journal.pending_notifications.is_empty() {
            self.journal.notification_retry_at = None;
        }
        if let Err(error) = self.save() {
            self.journal = original;
            self.storage_error = Some(error.clone());
            return Err(error);
        }
        self.storage_error = None;
        Ok(())
    }

    pub fn discard_notifications(&mut self) -> Result<(), String> {
        let events: Vec<_> = self.journal.pending_notifications.iter().cloned().collect();
        if events.is_empty() && self.storage_error.is_none() {
            return Ok(());
        }
        self.acknowledge_notifications(&events)
    }

    fn key(&mut self, machine: &MachineView) -> String {
        let key = machine.preferences_key();
        if key != machine.id
            && !self.journal.machines.contains_key(&key)
            && let Some(state) = self.journal.machines.remove(&machine.id)
        {
            self.journal.machines.insert(key.clone(), state);
            for event in &mut self.journal.events {
                if event.machine_key == machine.id {
                    event.machine_key.clone_from(&key);
                }
            }
            for event in &mut self.journal.pending_notifications {
                if event.machine_key == machine.id {
                    event.machine_key.clone_from(&key);
                }
            }
        }
        key
    }

    fn event(
        &mut self,
        key: &str,
        name: &str,
        now: DateTime<Utc>,
        kind: FleetEventKind,
        detail: String,
    ) -> FleetMachineEvent {
        let event = FleetMachineEvent {
            id: format!("{:020}", self.journal.next_id),
            machine_key: key.into(),
            machine_name: name.into(),
            at: now,
            kind,
            detail,
        };
        self.journal.next_id = self.journal.next_id.saturating_add(1);
        self.journal.events.push_back(event.clone());
        event
    }

    pub fn attach(&self, view: &mut FleetView, now: DateTime<Utc>, notifications: bool) {
        let network_fresh = fresh_network(view, now).is_some();
        view.monitoring.desktop_notifications = notifications;
        view.monitoring.storage_error.clone_from(&self.storage_error);
        view.monitoring.events = self.events(None, None, now);
        view.monitoring.alerts.clear();
        for machine in view.machines.iter_mut().chain(&mut view.other_machines) {
            let key = machine.preferences_key();
            let state = self
                .journal
                .machines
                .get(&key)
                .or_else(|| self.journal.machines.get(&machine.id));
            let disabled = matches!(machine.connection, MachineConnection::Disabled);
            let alerts = state
                .filter(|s| !disabled && s.target == target(machine))
                .map(|s| s.alerts(&key, machine, network_fresh, now))
                .unwrap_or_default();
            let maintenance_until = state.and_then(|s| s.maintenance_until).filter(|at| *at > now);
            machine.monitoring = FleetMachineMonitoring {
                machine_key: key.clone(),
                maintenance_until,
                alerts: alerts.clone(),
                events: self.events(Some(&key), None, now),
            };
            view.monitoring.alerts.extend(alerts);
        }
    }

    pub fn events(
        &self,
        machine_key: Option<&str>,
        before: Option<&str>,
        now: DateTime<Utc>,
    ) -> Vec<FleetMachineEvent> {
        self.event_page(machine_key, before, EVENT_PAGE_SIZE, now)
    }

    pub fn event_page(
        &self,
        machine_key: Option<&str>,
        before: Option<&str>,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Vec<FleetMachineEvent> {
        self.journal
            .events
            .iter()
            .rev()
            .filter(|e| {
                e.at >= now - EVENT_RETENTION
                    && machine_key.is_none_or(|k| k == e.machine_key)
                    && before.is_none_or(|id| e.id.as_str() < id)
            })
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn maintenance(
        &mut self,
        machine: &MachineView,
        until: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let original = self.journal.clone();
        let key = self.key(machine);
        let state = self.journal.machines.entry(key.clone()).or_default();
        state.target = target(machine);
        state.name.clone_from(&machine.display_name);
        state.last_seen = Some(now);
        state.maintenance_until = until;
        if until.is_some() {
            self.journal.outbound.retain(|d| d.event.machine_key != key);
            self.journal.pending_notifications.retain(|e| e.machine_key != key);
            if self.journal.pending_notifications.is_empty() {
                self.journal.notification_retry_at = None;
            }
        }
        self.event(
            &key,
            &machine.display_name,
            now,
            if until.is_some() {
                FleetEventKind::MaintenanceStarted
            } else {
                FleetEventKind::MaintenanceEnded
            },
            if until.is_some() {
                "Notifications paused for one hour. Monitoring and history continue."
            } else {
                "Maintenance ended. Alerts are active again."
            }
            .into(),
        );
        if let Err(e) = self.save() {
            self.journal = original;
            self.storage_error = Some(e.clone());
            return Err(e);
        }
        self.storage_error = None;
        self.saved_at = Some(now);
        Ok(())
    }

    /// Keeps at most [`MAX_OUTBOUND_PER_DESTINATION`] undelivered alerts per
    /// destination, dropping the oldest. Each drop is recorded as an event on
    /// the affected machine and as the destination's error, never silently.
    fn cap_outbound(&mut self, now: DateTime<Utc>) -> bool {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for d in &self.journal.outbound {
            *counts.entry(d.destination.clone()).or_default() += 1;
        }
        let mut excess: BTreeMap<String, usize> = counts
            .into_iter()
            .filter(|(_, n)| *n > MAX_OUTBOUND_PER_DESTINATION)
            .map(|(d, n)| (d, n - MAX_OUTBOUND_PER_DESTINATION))
            .collect();
        if excess.is_empty() {
            return false;
        }
        // Oldest first: the queue is in the order alerts were raised.
        let mut dropped: BTreeMap<(String, String, String), usize> = BTreeMap::new();
        self.journal.outbound.retain(|d| match excess.get_mut(&d.destination) {
            Some(left) if *left > 0 => {
                *left -= 1;
                let key = (
                    d.destination.clone(),
                    d.event.machine_key.clone(),
                    d.event.machine_name.clone(),
                );
                *dropped.entry(key).or_default() += 1;
                false
            }
            _ => true,
        });
        for ((destination, key, name), count) in dropped {
            self.journal.outbound_status.entry(destination).or_default().error = Some(format!(
                "Dropped {count} undelivered alert(s) for {name}: the queue holds at most {MAX_OUTBOUND_PER_DESTINATION}."
            ));
            self.event(
                &key,
                &name,
                now,
                FleetEventKind::AlertsDropped,
                format!(
                    "Dropped {count} undelivered alert(s) for an alert destination whose queue was full; see Settings ▸ Notifications."
                ),
            );
        }
        true
    }

    fn queue_outbound(&mut self, destinations: &BTreeSet<String>, event: &FleetMachineEvent, now: DateTime<Utc>) {
        for destination in destinations {
            self.journal.outbound.push_back(OutboundDelivery {
                destination: destination.clone(),
                event: event.clone(),
                attempts: 0,
                next_at: now,
            });
        }
    }

    /// Records an event the app caused (a drain, resume or watchdog action) in
    /// the machine's history, and queues it for notification when `notify` is
    /// set and the machine isn't in maintenance. Saved before returning.
    #[expect(clippy::too_many_arguments, reason = "one event with its delivery preferences")]
    pub fn record_external(
        &mut self,
        machine_key: &str,
        machine_name: &str,
        kind: FleetEventKind,
        detail: String,
        notify: bool,
        desktop: bool,
        destinations: &BTreeSet<String>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let original = self.journal.clone();
        let muted = self
            .journal
            .machines
            .get(machine_key)
            .and_then(|m| m.maintenance_until)
            .is_some_and(|until| until > now);
        let event = self.event(machine_key, machine_name, now, kind, detail);
        if notify && !muted {
            self.queue_outbound(destinations, &event, now);
            self.cap_outbound(now);
            if desktop {
                self.journal.pending_notifications.push_back(event);
                self.journal.notification_retry_at = None;
            }
        }
        if let Err(e) = self.save() {
            self.journal = original;
            self.storage_error = Some(e.clone());
            return Err(e);
        }
        self.storage_error = None;
        self.saved_at = Some(now);
        Ok(())
    }

    /// Records something the user did to a machine, such as sending a wake packet.
    pub fn note(
        &mut self,
        machine: &MachineView,
        kind: FleetEventKind,
        detail: String,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let original = self.journal.clone();
        let key = self.key(machine);
        self.event(&key, &machine.display_name, now, kind, detail);
        if let Err(e) = self.save() {
            self.journal = original;
            self.storage_error = Some(e.clone());
            return Err(e);
        }
        self.storage_error = None;
        self.saved_at = Some(now);
        Ok(())
    }

    /// Up to `limit` deliveries due now. Each is pushed out by a few minutes
    /// and saved before it is returned, so a crash mid-send retries it later
    /// rather than immediately (it can be delivered twice, never lost).
    pub fn claim_outbound(&mut self, now: DateTime<Utc>, limit: usize) -> Result<Vec<OutboundDelivery>, String> {
        let due: Vec<usize> = self
            .journal
            .outbound
            .iter()
            .enumerate()
            .filter(|(_, d)| d.next_at <= now)
            .map(|(i, _)| i)
            .take(limit)
            .collect();
        if due.is_empty() {
            return Ok(Vec::new());
        }
        let original = self.journal.clone();
        let mut claimed = Vec::new();
        for index in due {
            if let Some(delivery) = self.journal.outbound.get_mut(index) {
                claimed.push(delivery.clone());
                delivery.next_at = now + crate::outbound::retry_after(delivery.attempts.saturating_add(1));
            }
        }
        if let Err(e) = self.save() {
            self.journal = original;
            self.storage_error = Some(e.clone());
            return Err(e);
        }
        Ok(claimed)
    }

    /// Records how claimed deliveries went: accepted ones are removed, failed
    /// ones retried with backoff until they are a day old.
    pub fn finish_outbound(
        &mut self,
        results: &[(OutboundDelivery, Result<(), String>)],
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let original = self.journal.clone();
        let same =
            |a: &OutboundDelivery, b: &OutboundDelivery| a.destination == b.destination && a.event.id == b.event.id;
        for (delivery, result) in results {
            let status = self
                .journal
                .outbound_status
                .entry(delivery.destination.clone())
                .or_default();
            match result {
                Ok(()) => {
                    status.delivered_at = Some(now);
                    status.error = None;
                    self.journal.outbound.retain(|d| !same(d, delivery));
                }
                Err(error) => {
                    let expired = now - delivery.event.at > Duration::hours(crate::outbound::GIVE_UP_AFTER_HOURS);
                    status.error = Some(if expired {
                        format!("Gave up on an alert after a day of failures: {error}")
                    } else {
                        error.clone()
                    });
                    if expired {
                        self.journal.outbound.retain(|d| !same(d, delivery));
                    } else if let Some(pending) = self.journal.outbound.iter_mut().find(|d| same(d, delivery)) {
                        pending.attempts = pending.attempts.saturating_add(1);
                        pending.next_at = now + crate::outbound::retry_after(pending.attempts);
                    }
                }
            }
        }
        if let Err(e) = self.save() {
            self.journal = original;
            self.storage_error = Some(e.clone());
            return Err(e);
        }
        Ok(())
    }

    /// Forgets a removed destination's queue and status.
    pub fn discard_outbound(&mut self, destination: &str) -> Result<(), String> {
        let original = self.journal.clone();
        self.journal.outbound.retain(|d| d.destination != destination);
        self.journal.outbound_status.remove(destination);
        if self.journal == original {
            return Ok(());
        }
        if let Err(e) = self.save() {
            self.journal = original;
            self.storage_error = Some(e.clone());
            return Err(e);
        }
        Ok(())
    }

    /// A destination with its queue length and last outcome, for Settings.
    pub fn destination_view(&self, destination: &AlertDestination) -> FleetAlertDestinationView {
        let status = self.journal.outbound_status.get(&destination.id);
        FleetAlertDestinationView {
            id: destination.id.clone(),
            name: destination.name.clone(),
            kind: destination.kind,
            enabled: destination.enabled,
            summary: destination.summary.clone(),
            pending: crate::model::count_u32(
                self.journal
                    .outbound
                    .iter()
                    .filter(|d| d.destination == destination.id)
                    .count(),
            ),
            delivered_at: status.and_then(|s| s.delivered_at),
            error: status.and_then(|s| s.error.clone()),
        }
    }

    fn save(&self) -> Result<(), String> {
        if !self.writable {
            return Err(self
                .storage_error
                .clone()
                .unwrap_or_else(|| "Machine history cannot be saved.".into()));
        }
        let save = || -> Result<(), Box<dyn std::error::Error>> {
            crate::atomic_write::atomic_write(&self.path, &serde_json::to_vec(&self.journal)?)?;
            Ok(())
        };
        save().map_err(|e| format!("Couldn't save machine history: {e}"))
    }
}

/// Destinations alerts are queued for: enabled ones, as the view lists them.
fn enabled_destinations(view: &FleetView) -> BTreeSet<String> {
    view.automation
        .destinations
        .iter()
        .filter(|d| d.enabled)
        .map(|d| d.id.clone())
        .collect()
}

fn fresh_network(view: &FleetView, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    view.tailnet.as_of.filter(|at| {
        view.tailnet.available
            && view.tailnet.error.is_none()
            && view.tailnet.local_online != Some(false)
            && now - *at <= Duration::seconds(45)
            && *at <= now + Duration::seconds(5)
    })
}
