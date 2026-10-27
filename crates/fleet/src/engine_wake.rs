//! Waking an offline machine with Wake-on-LAN, as a reviewed, single-use action.
//!
//! The review names the sleeping machine, the inventory host that broadcasts the packet
//! (`wake_via`, which must be online) and the address. Sending re-reads the inventory and
//! the view, and refuses if anything the review showed changed.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::{Engine, EngineError};
use crate::desired::model::{Host, Inventory};
use crate::model::{FleetView, MachineView, inventory_machine_key};
use crate::monitoring::FleetEventKind;
use crate::sync::MutexExt;
use crate::wake::{BROADCAST, MacAddress, PORT, WakeSender};

/// Reviews expire like change reviews.
const REVIEW_LIFETIME: Duration = Duration::minutes(15);

/// What sending a wake packet would do, shown before it is sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, rename = "FleetWakeReview")]
#[serde(rename_all = "camelCase")]
pub struct WakeReview {
    pub id: String,
    pub machine_id: String,
    /// The inventory host to wake.
    pub host: String,
    /// The inventory host that sends the packet.
    pub via: String,
    pub mac_address: String,
    /// `address:port` the packet is broadcast to.
    pub broadcast: String,
    /// Why it can't be sent as shown. Empty when it can.
    pub blockers: Vec<String>,
    pub ready: bool,
    pub checked_at: DateTime<Utc>,
}

/// Pending reviews, used at most once.
#[derive(Debug, Default)]
pub struct WakeReviews(Vec<WakeReview>);

fn machine_for<'a>(view: &'a FleetView, host: &str) -> Option<&'a MachineView> {
    let key = inventory_machine_key(host);
    view.machines
        .iter()
        .chain(&view.other_machines)
        .find(|m| m.preferences_key() == key)
}

/// The review for waking `machine_id`, from the inventory as it is now. Pure.
pub(crate) fn build_wake_review(
    view: &FleetView,
    inventory: &Inventory,
    machine_id: &str,
    now: DateTime<Utc>,
) -> Result<WakeReview, EngineError> {
    let machine = view
        .machines
        .iter()
        .chain(&view.other_machines)
        .find(|m| m.id == machine_id)
        .ok_or_else(|| EngineError::NotFound("This machine is no longer listed. Refresh and try again.".into()))?;
    let host_id = machine
        .desired
        .as_ref()
        .map(|d| d.host.id.clone())
        .ok_or_else(|| EngineError::Invalid("Only inventory hosts can be woken.".into()))?;
    let host = inventory.find_host(&host_id).map_err(|e| EngineError::NotFound(e.0))?;
    let (Some(mac), Some(via_id)) = (&host.mac_address, &host.wake_via) else {
        return Err(EngineError::Invalid(format!(
            "Add mac_address and wake_via to {host_id} in the inventory to wake it."
        )));
    };
    let mac: MacAddress = mac.parse().map_err(EngineError::Invalid)?;
    let mut blockers = Vec::new();
    if !host.enabled {
        blockers.push(format!("{host_id} is disabled in the inventory."));
    }
    if machine.tailnet.as_ref().is_some_and(|p| p.online) {
        blockers.push(format!(
            "{host_id} is online on the tailnet, so there's nothing to wake."
        ));
    }
    let via: &Host = inventory.find_host(via_id).map_err(|e| EngineError::NotFound(e.0))?;
    if !via.enabled {
        blockers.push(format!(
            "{via_id} is disabled in the inventory, so it can't send the packet."
        ));
    }
    if via.ssh_target.is_none() {
        blockers.push(format!(
            "{via_id}'s SSH target is unset; export the variable referenced by {}.",
            via.ssh_setting
        ));
    }
    if !machine_for(view, via_id)
        .and_then(|m| m.tailnet.as_ref())
        .is_some_and(|p| p.online)
    {
        blockers.push(format!("{via_id} must be online on the tailnet to send the packet."));
    }
    Ok(WakeReview {
        id: format!("wake_{host_id}_{}", now.timestamp_nanos_opt().unwrap_or_default()),
        machine_id: machine.id.clone(),
        host: host_id,
        via: via_id.clone(),
        mac_address: mac.to_string(),
        broadcast: format!("{BROADCAST}:{PORT}"),
        ready: blockers.is_empty(),
        blockers,
        checked_at: now,
    })
}

impl Engine {
    /// Send wake packets through a fake in tests.
    pub fn with_wake_sender(mut self, sender: Arc<dyn WakeSender>) -> Self {
        self.wake_sender = sender;
        self
    }

    fn ensure_live_wake(&self) -> Result<(), EngineError> {
        if self.config().demo_mode {
            return Err(EngineError::Invalid("Turn off demo data to wake real machines.".into()));
        }
        Ok(())
    }

    /// Read-only: what waking this machine would do.
    pub async fn wake_review(&self, machine_id: &str) -> Result<WakeReview, EngineError> {
        self.ensure_live_wake()?;
        let inventory = self.read_inventory().await?;
        let now = Utc::now();
        let review = build_wake_review(&self.view(), &inventory, machine_id, now)?;
        let mut reviews = self.wake_reviews.locked();
        reviews.0.retain(|r| now - r.checked_at < REVIEW_LIFETIME);
        reviews.0.push(review.clone());
        Ok(review)
    }

    /// Sends the reviewed packet from the `wake_via` host if nothing changed since the review,
    /// and records it in the machine's history.
    pub async fn wake_execute(&self, review_id: &str) -> Result<String, EngineError> {
        self.ensure_live_wake()?;
        let reviewed = {
            let mut reviews = self.wake_reviews.locked();
            let index = reviews.0.iter().position(|r| r.id == review_id);
            index
                .map(|i| reviews.0.remove(i))
                .filter(|r| Utc::now() - r.checked_at < REVIEW_LIFETIME)
                .ok_or_else(|| {
                    EngineError::NotFound("That review expired or was already used; review the wake again.".into())
                })?
        };
        if !reviewed.ready {
            return Err(EngineError::Invalid("This wake has blockers and can't be sent.".into()));
        }
        let inventory = self.read_inventory().await?;
        let view = self.view();
        let now = build_wake_review(&view, &inventory, &reviewed.machine_id, Utc::now())?;
        if !now.ready
            || (&now.host, &now.via, &now.mac_address) != (&reviewed.host, &reviewed.via, &reviewed.mac_address)
        {
            return Err(EngineError::Invalid(format!(
                "{} changed since you reviewed the wake. Review it again.",
                reviewed.host
            )));
        }
        let via = inventory
            .find_host(&now.via)
            .map_err(|e| EngineError::NotFound(e.0))?
            .clone();
        let mac: MacAddress = now.mac_address.parse().map_err(EngineError::Invalid)?;
        let sender = self.wake_sender.clone();
        let sent = tokio::task::spawn_blocking(move || sender.send(&via, &mac))
            .await
            .map_err(|e| EngineError::Invalid(format!("Couldn't send the wake packet: {e}")))?
            .map_err(|e| EngineError::Invalid(format!("{} couldn't send the wake packet: {e}", now.via)))?;
        tracing::info!(host = %now.host, via = %now.via, "{sent}");
        let message = format!(
            "Sent a Wake-on-LAN packet for {} ({}) from {}. It shows as online once it wakes.",
            now.host, now.mac_address, now.via
        );
        if let Some(machine) = view
            .machines
            .iter()
            .chain(&view.other_machines)
            .find(|m| m.id == now.machine_id)
        {
            // The packet went out; a history write failure shows as a storage problem instead.
            let _ = self
                .monitoring
                .locked()
                .note(machine, FleetEventKind::WakeRequested, message.clone(), Utc::now());
        }
        self.view_changed();
        Ok(message)
    }
}
