//! Reviews for maintenance changes (cleanup, upgrade, drain, resume): the
//! extra read-only facts they need, the pure functions that turn those facts
//! into plan items, and the fictional reviews shown with demo data.

use super::super::{Engine, EngineError};
use super::review::{Proposal, block, missing, not_found};
use crate::desired::github::ReleaseAsset;
use crate::desired::maintenance::{
    CleanupOptions, CleanupTarget, MACHINE, check_cleanup_options, check_service_change, cleanup_candidates,
    cleanup_refusal, version_behind,
};
use crate::desired::model::{FleetPlanAction, FleetSnapshot, Host, Inventory, NO_VALUE, PlanItem, Runner};
use crate::desired::operations::{check_enabled, inventory_runner};
use crate::desired::remote::CleanupSurvey;
use crate::fleet_plan::ChangeRequest;

/// What a maintenance review reads besides the snapshot: the cleanup survey
/// and the latest runner release. Both are read-only.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReviewFacts {
    pub(crate) cleanup: Option<Result<CleanupSurvey, String>>,
    pub(crate) release: Option<Result<ReleaseAsset, String>>,
}

impl ReviewFacts {
    /// The release a ready upgrade review was built against.
    pub(crate) fn asset(&self) -> Option<&ReleaseAsset> {
        self.release.as_ref().and_then(|r| r.as_ref().ok())
    }
}

impl Engine {
    /// Reads the facts `request` needs beyond the snapshot, off the async runtime.
    pub(super) async fn review_facts(
        &self,
        inventory: &Inventory,
        request: &ChangeRequest,
    ) -> Result<ReviewFacts, EngineError> {
        let (host_id, cleanup) = match request {
            ChangeRequest::Cleanup { host, options } => (host.clone(), Some(options.clone())),
            ChangeRequest::Upgrade { host, .. } => (host.clone(), None),
            ChangeRequest::Apply { .. }
            | ChangeRequest::Retire { .. }
            | ChangeRequest::Decommission { .. }
            | ChangeRequest::Restart { .. }
            | ChangeRequest::Drain { .. }
            | ChangeRequest::Resume { .. } => return Ok(ReviewFacts::default()),
        };
        let host = inventory.find_host(&host_id).map_err(not_found)?.clone();
        let (github, remote) = (self.desired_io.boundaries)(inventory);
        tokio::task::spawn_blocking(move || match cleanup {
            Some(options) => ReviewFacts {
                cleanup: Some(
                    remote
                        .survey_cleanup(&host, &cleanup_candidates(&host, &options), &options)
                        .map_err(|e| e.0),
                ),
                release: None,
            },
            None => ReviewFacts {
                cleanup: None,
                release: Some(github.latest_asset(host.platform.runner_asset()).map_err(|e| e.0)),
            },
        })
        .await
        .map_err(|e| EngineError::Invalid(format!("Couldn't read the machine for the review: {e}")))
    }
}

/// Plan items for each surveyed target (in the same order as the targets),
/// then for anything the host refused to measure. A busy runner's targets,
/// and machine-wide ones while any runner is busy, can't run.
pub(super) fn review_cleanup(
    host: &Host,
    options: &CleanupOptions,
    snapshot: &FleetSnapshot,
    facts: &ReviewFacts,
    blockers: &mut Vec<String>,
) -> Proposal {
    block(blockers, check_enabled(host));
    block(blockers, check_cleanup_options(options));
    let survey = match &facts.cleanup {
        Some(Ok(survey)) => survey.clone(),
        Some(Err(e)) => {
            blockers.push(format!("Couldn't measure what can be cleaned up on {}: {e}", host.id));
            CleanupSurvey::default()
        }
        None => CleanupSurvey::default(),
    };
    let mut items: Vec<PlanItem> = survey
        .targets
        .iter()
        .map(|target| {
            let item = PlanItem::new(
                FleetPlanAction::Cleanup,
                &host.id,
                target.item_runner(),
                target.reason(options),
            );
            match cleanup_refusal(host, target, &snapshot.github).filter(|_| snapshot.github_error.is_none()) {
                Some(reason) => PlanItem {
                    reason: format!("{}; not now: {reason}", item.reason),
                    ..item
                }
                .blocked(),
                None => item,
            }
        })
        .collect();
    for (_, runner, message) in &survey.refused {
        items.push(
            PlanItem::new(
                FleetPlanAction::Cleanup,
                &host.id,
                runner.as_deref().unwrap_or(MACHINE),
                message.clone(),
            )
            .blocked(),
        );
    }
    if facts.cleanup.as_ref().is_some_and(Result::is_ok) && !items.iter().any(|i| !i.blocked) {
        blockers.push(if items.is_empty() {
            "Nothing to clean up: the caches and logs this looks for are empty.".into()
        } else {
            "Nothing can be cleaned up right now.".into()
        });
    }
    Proposal {
        title: format!("Clean up {}", host.id),
        items,
        cleanup: survey.targets,
        ..Proposal::default()
    }
}

/// The reviewed cleanup targets whose plan items can run.
pub(crate) fn runnable_targets(targets: &[CleanupTarget], items: &[PlanItem]) -> Vec<CleanupTarget> {
    targets
        .iter()
        .zip(items)
        .filter(|(_, item)| !item.blocked)
        .map(|(target, _)| target.clone())
        .collect()
}

/// One item per runner behind the latest release. Busy runners, and those
/// GitHub doesn't list, can't run now.
pub(super) fn review_upgrade(
    host: &Host,
    runner: Option<&str>,
    snapshot: &FleetSnapshot,
    facts: &ReviewFacts,
    blockers: &mut Vec<String>,
) -> Result<Proposal, EngineError> {
    block(blockers, check_enabled(host));
    let selected: Vec<&Runner> = match runner {
        Some(name) => {
            let declared = inventory_runner(host, name).map_err(missing)?;
            block(
                blockers,
                check_service_change(name, declared, crate::desired::model::RunnerRole::Declared),
            );
            vec![declared]
        }
        None => host.desired_runners().collect(),
    };
    let asset = match &facts.release {
        Some(Ok(asset)) => Some(asset),
        Some(Err(e)) => {
            blockers.push(format!("Couldn't read the latest runner release: {e}"));
            None
        }
        None => None,
    };
    if let Some(asset) = asset
        && asset.sha256.is_none()
    {
        blockers.push(format!(
            "GitHub didn't publish a SHA-256 digest for {}, so it can't be verified. Not upgrading.",
            asset.name
        ));
    }
    let mut items = Vec::new();
    if let Some(asset) = asset {
        for r in selected {
            let local = snapshot.local.get(&r.name).filter(|l| l.configured);
            let item = |reason: String| PlanItem::new(FleetPlanAction::Upgrade, &host.id, &r.name, reason);
            let Some(current) = local.and_then(|l| l.version.as_deref()) else {
                if runner.is_some() {
                    items.push(item("its runner version is unknown; check the machine first".into()).blocked());
                }
                continue;
            };
            if !version_behind(current, &asset.version) {
                if runner.is_some() {
                    items.push(item(format!("already runs {current}")).blocked());
                }
                continue;
            }
            let upgrade = item(format!(
                "upgrade from {current} to {}: stop the idle service, replace bin/ and externals/ in place, start it",
                asset.version
            ));
            items.push(match snapshot.github.get(&r.name) {
                _ if snapshot.github_error.is_some() => upgrade,
                None => PlanItem {
                    reason: format!(
                        "{}; not now: missing on GitHub, so it can't be verified idle",
                        upgrade.reason
                    ),
                    ..upgrade
                }
                .blocked(),
                Some(g) if g.busy => PlanItem {
                    reason: format!("{}; not now: busy", upgrade.reason),
                    ..upgrade
                }
                .blocked(),
                Some(_) => upgrade,
            });
        }
    }
    if asset.is_some() && blockers.is_empty() && !items.iter().any(|i| !i.blocked) {
        blockers.push(if items.iter().all(|i| i.blocked && !i.reason.contains("not now")) {
            "Nothing to upgrade: every runner already runs the latest release.".into()
        } else {
            "No idle runner can be upgraded right now.".into()
        });
    }
    Ok(Proposal {
        title: match runner {
            Some(r) => format!("Upgrade {r}"),
            None => format!("Upgrade idle runners on {}", host.id),
        },
        items,
        upgrade_to: asset.map(|a| a.version.clone()),
        ..Proposal::default()
    })
}

fn service_state(snapshot: &FleetSnapshot, runner: &str) -> Option<String> {
    snapshot
        .local
        .get(runner)
        .filter(|l| l.configured)
        .map(|l| l.service_state.clone())
}

/// Drain stops the service once the runner is idle. The reason doesn't name
/// the runner's current activity, so a job finishing between review and run
/// doesn't invalidate the review.
pub(super) fn review_drain(
    inventory: &Inventory,
    host: &Host,
    runner: &str,
    snapshot: &FleetSnapshot,
    blockers: &mut Vec<String>,
) -> Result<Proposal, EngineError> {
    let (_, declared, role) = inventory.find_runner(runner).map_err(not_found)?;
    block(blockers, check_service_change(runner, declared, role));
    match service_state(snapshot, runner) {
        None if !snapshot.host_errors.contains_key(&host.id) => {
            blockers.push(format!("{runner} isn't configured on {}.", host.id));
        }
        Some(state) if !crate::desired::model::is_active(&state) => {
            blockers.push(format!("{runner}'s service is already {state}."));
        }
        _ => {}
    }
    if snapshot.github_error.is_none() && !snapshot.github.contains_key(runner) {
        blockers.push(format!(
            "{runner} is missing on GitHub, so the app can't tell when it's idle."
        ));
    }
    Ok(Proposal {
        title: format!("Drain {runner}"),
        items: vec![PlanItem::new(
            FleetPlanAction::Drain,
            &host.id,
            runner,
            "stop the service once the runner is idle: now if it is, otherwise after its current job. GitHub has \
             no way to pause a runner, so it shows offline until it's resumed",
        )],
        ..Proposal::default()
    })
}

/// Resume starts a stopped service.
pub(super) fn review_resume(
    inventory: &Inventory,
    host: &Host,
    runner: &str,
    snapshot: &FleetSnapshot,
    blockers: &mut Vec<String>,
) -> Result<Proposal, EngineError> {
    let (_, declared, role) = inventory.find_runner(runner).map_err(not_found)?;
    block(blockers, check_service_change(runner, declared, role));
    let state = service_state(snapshot, runner);
    match state.as_deref() {
        None if !snapshot.host_errors.contains_key(&host.id) => {
            blockers.push(format!("{runner} isn't configured on {}.", host.id));
        }
        Some(s) if crate::desired::model::is_active(s) => blockers.push(format!("{runner} is already running.")),
        Some("unknown") => blockers.push(format!("{runner}'s service state is unknown; check the machine first.")),
        _ => {}
    }
    Ok(Proposal {
        title: format!("Resume {runner}"),
        items: vec![PlanItem::new(
            FleetPlanAction::Resume,
            &host.id,
            runner,
            format!("start the {} service", state.as_deref().unwrap_or(NO_VALUE)),
        )],
        ..Proposal::default()
    })
}

/// Demo data's facts for a maintenance review: fictional sizes and release.
pub(super) fn demo_facts(host: &Host, request: &ChangeRequest) -> ReviewFacts {
    match request {
        ChangeRequest::Cleanup { options, .. } => ReviewFacts {
            cleanup: Some(Ok(crate::demo_desired::cleanup_survey(host, options))),
            release: None,
        },
        ChangeRequest::Upgrade { .. } => ReviewFacts {
            cleanup: None,
            release: Some(Ok(crate::demo_desired::release(host))),
        },
        ChangeRequest::Apply { .. }
        | ChangeRequest::Retire { .. }
        | ChangeRequest::Decommission { .. }
        | ChangeRequest::Restart { .. }
        | ChangeRequest::Drain { .. }
        | ChangeRequest::Resume { .. } => ReviewFacts::default(),
    }
}
