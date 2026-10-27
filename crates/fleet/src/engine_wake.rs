//! Waking an offline machine with Wake-on-LAN, as a reviewed, single-use action.
//!
//! The review names the sleeping machine, the inventory host that broadcasts the packet
//! (`wake_via`, which must be online) and the address. Sending re-reads the inventory and
//! the view, and refuses if anything the review showed changed.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::desired::SshTransportIdentity;
use super::{Engine, EngineError};
use crate::desired::model::{Host, Inventory, SshTransport};
use crate::model::{FleetView, MachineView, inventory_machine_key};
use crate::monitoring::FleetEventKind;
use crate::sync::MutexExt;
use crate::wake::{MacAddress, PORT, WakeSender, broadcast_for};

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

/// A review, with how the sending host was reached when it was made.
#[derive(Debug, Clone, PartialEq)]
struct Pending {
    review: WakeReview,
    via: ViaConnection,
}

/// The sending host's SSH target, transport and effective OpenSSH configuration (`ssh -G`).
#[derive(Debug, Clone, PartialEq)]
struct ViaConnection {
    target: Option<String>,
    transport: SshTransport,
    resolved: Option<SshTransportIdentity>,
}

/// Pending reviews, used at most once and only within [`REVIEW_LIFETIME`].
#[derive(Debug, Default)]
pub struct WakeReviews(Vec<Pending>);

impl WakeReviews {
    fn add(&mut self, pending: Pending, now: DateTime<Utc>) {
        self.0.retain(|p| now - p.review.checked_at < REVIEW_LIFETIME);
        self.0.push(pending);
    }

    /// Removes the review; `None` if it's unknown, used or expired.
    fn take(&mut self, id: &str, now: DateTime<Utc>) -> Option<Pending> {
        let index = self.0.iter().position(|p| p.review.id == id)?;
        Some(self.0.remove(index)).filter(|p| now - p.review.checked_at < REVIEW_LIFETIME)
    }
}

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
    let broadcast = broadcast_for(host).map_err(EngineError::Invalid)?;
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
        broadcast: format!("{broadcast}:{PORT}"),
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

    /// The review for `machine_id` and how its sending host is reached right now.
    async fn current_wake(&self, machine_id: &str) -> Result<(Inventory, FleetView, Pending), EngineError> {
        let inventory = self.read_inventory().await?;
        let resolved = self.resolve_ssh_hosts(&inventory).await;
        let view = self.view();
        let mut review = build_wake_review(&view, &inventory, machine_id, Utc::now())?;
        let via_host = inventory
            .find_host(&review.via)
            .map_err(|e| EngineError::NotFound(e.0))?;
        let via = ViaConnection {
            target: via_host.ssh_target.clone(),
            transport: via_host.ssh_transport,
            resolved: via_host.ssh_target.as_ref().and_then(|t| resolved.get(t)).cloned(),
        };
        if via.transport == SshTransport::Openssh && via.target.is_some() && via.resolved.is_none() {
            review.blockers.push(format!(
                "Couldn't resolve {}'s effective SSH configuration. Check SSH settings and review the wake again.",
                review.via
            ));
            review.ready = false;
        }
        Ok((inventory, view, Pending { review, via }))
    }

    /// Read-only: what waking this machine would do.
    pub async fn wake_review(&self, machine_id: &str) -> Result<WakeReview, EngineError> {
        self.ensure_live_wake()?;
        let (_, _, pending) = self.current_wake(machine_id).await?;
        let review = pending.review.clone();
        self.wake_reviews.locked().add(pending, Utc::now());
        Ok(review)
    }

    /// Sends the reviewed packet from the `wake_via` host if nothing changed since the review,
    /// and records it in the machine's history.
    pub async fn wake_execute(&self, review_id: &str) -> Result<String, EngineError> {
        self.ensure_live_wake()?;
        let reviewed = self.wake_reviews.locked().take(review_id, Utc::now()).ok_or_else(|| {
            EngineError::NotFound("That review expired or was already used; review the wake again.".into())
        })?;
        if !reviewed.review.ready {
            return Err(EngineError::Invalid("This wake has blockers and can't be sent.".into()));
        }
        let (inventory, view, current) = self.current_wake(&reviewed.review.machine_id).await?;
        let (now, was) = (&current.review, &reviewed.review);
        let key = |r: &WakeReview| {
            (
                r.host.clone(),
                r.via.clone(),
                r.mac_address.clone(),
                r.broadcast.clone(),
            )
        };
        if !now.ready || key(now) != key(was) || current.via != reviewed.via {
            return Err(EngineError::Invalid(format!(
                "{} or {} changed since you reviewed the wake. Review it again.",
                was.host, was.via
            )));
        }
        let now = current.review;
        let via = inventory
            .find_host(&now.via)
            .map_err(|e| EngineError::NotFound(e.0))?
            .clone();
        let mac: MacAddress = now.mac_address.parse().map_err(EngineError::Invalid)?;
        let target = inventory.find_host(&now.host).map_err(|e| EngineError::NotFound(e.0))?;
        let broadcast = broadcast_for(target).map_err(EngineError::Invalid)?;
        let sender = self.wake_sender.clone();
        let sent = tokio::task::spawn_blocking(move || sender.send(&via, &mac, broadcast))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(id: &str, checked_at: DateTime<Utc>) -> Pending {
        Pending {
            review: WakeReview {
                id: id.into(),
                machine_id: "ts:n".into(),
                host: "mini".into(),
                via: "relay".into(),
                mac_address: "02:00:5e:10:00:01".into(),
                broadcast: "255.255.255.255:9".into(),
                blockers: Vec::new(),
                ready: true,
                checked_at,
            },
            via: ViaConnection {
                target: Some("relay".into()),
                transport: SshTransport::Tailscale,
                resolved: None,
            },
        }
    }

    #[test]
    fn reviews_are_single_use_and_expire_after_fifteen_minutes() {
        let now = Utc::now();
        let mut reviews = WakeReviews::default();
        reviews.add(pending("fresh", now), now);
        reviews.add(pending("old", now - Duration::minutes(14)), now);
        assert!(reviews.take("old", now + Duration::minutes(2)).is_none(), "expired");
        assert!(reviews.take("fresh", now + Duration::minutes(14)).is_some());
        assert!(reviews.take("fresh", now + Duration::minutes(14)).is_none(), "used");
        // Adding a review drops expired ones.
        reviews.add(pending("a", now - Duration::minutes(20)), now);
        reviews.add(pending("b", now), now);
        assert_eq!(
            reviews.0.iter().map(|p| p.review.id.as_str()).collect::<Vec<_>>(),
            ["b"]
        );
    }
}
