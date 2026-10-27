//! Fictional automation state for demo data: a runner draining after its
//! job, the watchdog on with one restart in history, a Slack destination and
//! a published availability value. Nothing here runs or sends anything.

use chrono::{DateTime, Duration, Utc};

use super::{
    FleetAutomation, FleetDrain, FleetDrainState, FleetPublishStatus, FleetWatchdog, FleetWatchdogRunner, publish,
};
use crate::model::FleetView;
use crate::outbound::{AlertDestinationKind, FleetAlertDestinationView};

pub(crate) fn attach(view: &mut FleetView, now: DateTime<Utc>) {
    let ago = |minutes| now - Duration::minutes(minutes);
    let availability = publish::availability(view, now);
    view.automation = FleetAutomation {
        drains: vec![FleetDrain {
            runner: "studio-1".into(),
            host_id: "studio".into(),
            state: FleetDrainState::Draining,
            requested_at: ago(6),
            stopped_at: None,
            error: None,
        }],
        holds: Vec::new(),
        watchdog: FleetWatchdog {
            enabled: true,
            runners: vec![FleetWatchdogRunner {
                runner: "linux-01-a".into(),
                host_id: "build-linux-01".into(),
                attempts: vec![ago(52)],
                down_since: None,
                next_attempt_at: None,
                gave_up_at: None,
                last_error: None,
            }],
        },
        destinations: vec![FleetAlertDestinationView {
            id: "demo-slack".into(),
            name: "#ci-alerts".into(),
            kind: AlertDestinationKind::Slack,
            enabled: true,
            summary: "hooks.slack.com".into(),
            pending: 0,
            delivered_at: Some(ago(12)),
            error: None,
        }],
        publish: FleetPublishStatus {
            enabled: true,
            organization: Some("acme-demo".into()),
            variable: publish::DEFAULT_VARIABLE.into(),
            interval_minutes: publish::DEFAULT_INTERVAL_MINUTES,
            value: availability.as_ref().ok().map(|a| a.value(ago(9))),
            published_at: Some(ago(9)),
            error: None,
            waiting: availability.err(),
        },
    };
}
