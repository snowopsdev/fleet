//! Bounded SSH polling, keeping the last successful read beside failures.

use std::sync::Arc;

use chrono::Utc;
use futures::{StreamExt, stream};

use super::{Engine, EngineError};
use crate::machine::{MachineSource, MachineTarget};
use crate::model::{MachineObservation, MachinePreferences, Timed, inventory_machine_key};
use crate::ssh_reuse::ControlDir;
use crate::sync::MutexExt;

/// Shared SSH masters are replaced at least this often, since a master keeps the
/// authentication and configuration it started with.
pub const RECYCLE_MASTERS_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 60);
use crate::sync::RwLockExt;

impl Engine {
    /// Save personal labels without changing machine identity or SSH configuration.
    pub fn set_machine_preferences(&self, id: &str, display_name: &str, description: &str) -> Result<(), EngineError> {
        if self.config().demo_mode {
            return Err(EngineError::Invalid(
                "Turn off demo data to label your machines.".into(),
            ));
        }
        let display_name = display_name.trim();
        let description = description.trim();
        if display_name.chars().count() > 120 || display_name.chars().any(char::is_control) {
            return Err(EngineError::Invalid(
                "Use a display name of up to 120 characters on one line.".into(),
            ));
        }
        if description.chars().count() > 2000 {
            return Err(EngineError::Invalid(
                "Use a description of up to 2,000 characters.".into(),
            ));
        }
        let view = self.view();
        let machine = view
            .machines
            .into_iter()
            .chain(view.other_machines)
            .find(|m| m.id == id)
            .ok_or_else(|| EngineError::NotFound("This machine is no longer listed. Refresh and try again.".into()))?;
        let key = machine.preferences_key();
        let preferences = MachinePreferences {
            display_name: (!display_name.is_empty()).then(|| display_name.to_string()),
            description: (!description.is_empty()).then(|| description.to_string()),
        };
        self.update_config(|config| {
            // A peer may have gained an inventory entry since its first label.
            config.machine_preferences.remove(&machine.id);
            if preferences == MachinePreferences::default() {
                config.machine_preferences.remove(&key);
            } else {
                config.machine_preferences.insert(key, preferences);
            }
        })
    }

    /// Replace SSH collection with a deterministic boundary in tests.
    pub fn with_machine_source(mut self, source: Arc<dyn MachineSource>) -> Self {
        self.machine_source = source;
        self
    }

    /// Keep shared SSH connection sockets somewhere else (tests).
    pub fn with_ssh_control_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.ssh_control_dir = dir.into();
        self
    }

    /// Share one OpenSSH connection per machine across monitoring checks, or stop.
    /// Turning it off removes the sockets, so open connections close within
    /// [`crate::ssh_reuse::CONTROL_PERSIST_SECS`] seconds.
    pub fn set_ssh_connection_reuse(&self, on: bool) -> Result<(), EngineError> {
        if on {
            ControlDir::prepare(&self.ssh_control_dir).map_err(EngineError::Invalid)?;
        }
        self.update_config(|c| c.ssh_connection_reuse = on)?;
        if !on {
            self.close_shared_ssh();
        }
        Ok(())
    }

    /// The verified socket directory when connection reuse is on. If the directory isn't
    /// safe, checks fall back to separate connections. Masters are recycled when the
    /// inventory's SSH targets or their resolved configuration change, after
    /// [`RECYCLE_MASTERS_AFTER`], and at the first use after launch, so changed SSH
    /// configuration, keys and known hosts take effect.
    fn ssh_control(&self, connections: &str) -> Option<ControlDir> {
        if !self.config().ssh_connection_reuse {
            return None;
        }
        let control = ControlDir::prepare(&self.ssh_control_dir)
            .inspect_err(|e| tracing::warn!("{e}"))
            .ok()?;
        let mut epoch = self.ssh_control_epoch.locked();
        let current = epoch
            .as_ref()
            .is_some_and(|(since, seen)| since.elapsed() < RECYCLE_MASTERS_AFTER && seen == connections);
        if !current {
            control.close_all();
            *epoch = Some((std::time::Instant::now(), connections.to_string()));
        }
        Some(control)
    }

    /// Stops sharing SSH connections: removes the sockets so no session joins an open master.
    /// Call when the app quits.
    pub fn close_shared_ssh(&self) {
        if let Ok(control) = ControlDir::prepare(&self.ssh_control_dir) {
            control.close_all();
        }
        *self.ssh_control_epoch.locked() = None;
    }

    /// Read health and runner state, with at most four concurrent SSH sessions.
    /// Concurrent refresh requests coalesce; failures preserve the last good data.
    pub async fn poll_machines(&self) {
        let Ok(_poll) = self.machine_poll.try_lock() else {
            return;
        };
        self.poll_machines_inner(None).await;
    }

    /// Await the requested observation scope even when automatic polling is in progress.
    pub(crate) async fn poll_machines_wait(&self, machine: Option<&str>) {
        let _poll = self.machine_poll.lock().await;
        self.poll_machines_inner(machine).await;
    }

    async fn poll_machines_inner(&self, machine: Option<&str>) {
        if self.config().demo_mode {
            return;
        }

        // What shared SSH masters were opened against: a change recycles them.
        let mut connections = String::new();
        if let Ok(inventory) = self.read_inventory().await {
            let resolved = self.resolve_ssh_hosts(&inventory).await;
            for host in &inventory.hosts {
                connections.push_str(&format!(
                    "{}\t{:?}\t{:?}\n",
                    host.id, host.ssh_target, host.ssh_transport
                ));
            }
            connections.push_str(&format!("{resolved:?}"));
        }
        let view = self.view();
        let targets = view
            .machines
            .into_iter()
            .chain(view.other_machines)
            .filter(|m| machine.is_none_or(|id| m.id == id || m.preferences_key() == id))
            .filter(|m| m.tailnet.as_ref().is_none_or(|p| p.online))
            .filter_map(|m| {
                if let Some(d) = m.desired {
                    d.host
                        .enabled
                        .then(|| (inventory_machine_key(&d.host.id), MachineTarget::Inventory(d.host)))
                } else {
                    m.tailnet.map(|p| (m.id, MachineTarget::discovered(&p)))
                }
            })
            .collect::<Vec<_>>();
        let source = &self.machine_source;
        let control = self.ssh_control(&connections);
        let mut reads = stream::iter(targets.into_iter().map(|(id, target)| {
            let source = source.clone();
            let control = control.clone();
            async move {
                let inspected = target.clone();
                let result = tokio::task::spawn_blocking(move || match &control {
                    Some(control) => source.inspect_shared(&inspected, control),
                    None => source.inspect(&inspected),
                })
                .await
                .map_err(|e| format!("The SSH check stopped unexpectedly: {e}"))
                .and_then(|r| r.map_err(|e| e.to_string()));
                (id, target, result)
            }
        }))
        .buffer_unordered(4);
        while let Some((id, target, result)) = reads.next().await {
            let now = Utc::now();
            {
                let mut observations = self.obs.write_locked();
                let entry = observations.machines.entry(id).or_insert_with(|| MachineObservation {
                    target: target.clone(),
                    report: None,
                    last_error: None,
                });
                if entry.target != target {
                    *entry = MachineObservation {
                        target,
                        report: None,
                        last_error: None,
                    };
                }
                match result {
                    Ok(report) => {
                        entry.report = Some(Timed::new(now, report));
                        entry.last_error = None;
                    }
                    Err(error) => entry.last_error = Some(Timed::new(now, error)),
                }
            }
            self.view_changed();
        }
    }
}
