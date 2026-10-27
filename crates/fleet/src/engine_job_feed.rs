//! The optional real-time job feed: a loopback receiver plus one supervised
//! `gh webhook forward` per organization that turned it on in Settings.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use super::Engine;
use super::job_history::RoundResult;
use crate::jobs::FleetJobSource;
use crate::jobs::feed::{Backoff, ForwardSpec, GhLauncher, Launch, supervise};
use crate::jobs::webhook::{self, DeliverySink, FeedAction, FeedDelivery};
use crate::sync::{MutexExt, RwLockExt};

/// How often the controller re-reads settings without a change notification (demo mode,
/// removed organizations).
const RECONCILE_EVERY: Duration = Duration::from_secs(5);

impl Engine {
    /// Runs the job feed until [`Engine::stop_job_feeds`]. Binds the receiver on an
    /// ephemeral loopback port, then starts and stops forwarders as settings change.
    ///
    /// # Errors
    ///
    /// Fails if the receiver can't listen or a secret can't be generated.
    pub async fn run_job_feeds(self: Arc<Self>) -> std::io::Result<()> {
        self.run_job_feeds_with(Arc::new(GhLauncher), crate::gh::find, Backoff::default())
            .await
    }

    /// [`Engine::run_job_feeds`] with a replaceable launcher (tests never start `gh`).
    pub async fn run_job_feeds_with<L: Launch>(
        self: Arc<Self>,
        launcher: Arc<L>,
        find_gh: fn() -> Option<std::path::PathBuf>,
        backoff: Backoff,
    ) -> std::io::Result<()> {
        let secret = webhook::new_secret().map_err(std::io::Error::other)?;
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
        let url = format!("http://{}{}", listener.local_addr()?, webhook::RECEIVER_PATH);
        let sink: DeliverySink = {
            let engine = self.clone();
            Arc::new(move |delivery| {
                let engine = engine.clone();
                Box::pin(async move { engine.record_feed_delivery(delivery).await })
            })
        };
        let receiver = webhook::router(&secret, sink).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, receiver).await {
                tracing::warn!("job feed receiver stopped: {error}");
            }
        });
        let mut running: BTreeMap<String, tokio::sync::watch::Sender<bool>> = BTreeMap::new();
        let mut changes = self.job_activity.settings.subscribe();
        loop {
            let config = self.config();
            let wanted: BTreeMap<String, crate::model::OrgConfig> = if config.demo_mode
                || self.job_activity.feeds.is_stopped()
            {
                BTreeMap::new()
            } else {
                config
                    .orgs
                    .into_iter()
                    .map(|o| (o.login.to_lowercase(), o))
                    .filter(|(key, o)| {
                        o.kind == crate::model::AccountKind::Organization && config.job_activity.feed_orgs.contains(key)
                    })
                    .collect()
            };
            running.retain(|org, stop| {
                let keep = wanted.contains_key(org);
                if !keep {
                    // The supervisor may already have ended; nothing else to stop then.
                    let _ = stop.send(true);
                }
                keep
            });
            if self.job_activity.feeds.is_stopped() {
                return Ok(());
            }
            for (key, org) in wanted {
                if running.contains_key(&key) {
                    continue;
                }
                let spec = ForwardSpec {
                    program: find_gh().unwrap_or_else(|| "gh".into()),
                    org: org.login.clone(),
                    host: crate::gh::host_for_api_base(&org.api_base),
                    url: url.clone(),
                    secret: secret.clone(),
                };
                let (stop, stopped) = tokio::sync::watch::channel(false);
                let (launcher, engine) = (launcher.clone(), self.clone());
                tokio::spawn(async move {
                    supervise(&*launcher, spec, &engine.job_activity.feeds, stopped, backoff).await;
                });
                running.insert(key, stop);
            }
            tokio::select! {
                _ = changes.changed() => {}
                () = tokio::time::sleep(RECONCILE_EVERY) => {}
            }
        }
    }

    /// Terminates every forwarder now. Safe to call from the app's exit handler.
    pub fn stop_job_feeds(&self) {
        self.job_activity.feeds.stop_all();
        // Wake the controller so it ends its supervisors now.
        self.job_activity.settings.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Applies one validated `workflow_job` delivery to the observations and the journal.
    ///
    /// Only organizations with the feed turned on are accepted. A queued or running delivery
    /// for a job already seen completed is ignored, so a late delivery can't revive it. The
    /// in-flight scan only takes jobs from repositories in the organization's job scope;
    /// completed self-hosted jobs are recorded from any repository, since they ran on the
    /// organization's runners.
    pub async fn record_feed_delivery(&self, delivery: FeedDelivery) {
        let config = self.config();
        let org = delivery.org.clone();
        let Some(account) = config.orgs.iter().find(|o| o.login.to_lowercase() == org) else {
            return;
        };
        if config.demo_mode || !config.job_activity.feed_orgs.contains(&org) {
            return;
        }
        let now = Utc::now();
        self.job_activity.feeds.delivered(&org, now);
        let job = delivery.job;
        let completed = delivery.action == FeedAction::Completed;
        {
            let mut store = self.job_activity.store.locked();
            if completed {
                store.mark_completed(job.job.id);
            } else if store.is_completed(job.job.id) {
                return;
            }
        }
        let in_scope = scope_includes(&account.repo_scope, &job.job.repo);
        let runners = {
            let mut obs = self.obs.write_locked();
            let Some(observed) = obs.github.get_mut(&org) else {
                return;
            };
            // Keep the in-flight scan current between polls; the next scan replaces it.
            if let Some(scan) = observed.jobs.as_mut() {
                scan.value.jobs.retain(|j| j.id != job.job.id);
                if !completed && in_scope {
                    scan.value.jobs.push(job.job.clone());
                    scan.value.jobs.sort_by_key(|j| j.id);
                }
            }
            crate::jobs::account_runners(observed)
        };
        if completed {
            let mut result = RoundResult::default();
            result.finished.push((org, runners, job));
            self.apply_job_round(result, FleetJobSource::Webhook, now).await;
        } else if in_scope {
            self.job_activity
                .store
                .locked()
                .track(&org, std::slice::from_ref(&job.job), &runners, now);
        }
        self.view_changed();
    }
}

/// Whether the job scan covers `repo` (`owner/name`). A recent-repositories scope can reach
/// any repository where self-hosted jobs run, so it takes them all.
fn scope_includes(scope: &crate::model::RepoScope, repo: &str) -> bool {
    let name = repo.rsplit_once('/').map_or(repo, |(_, name)| name);
    match scope {
        crate::model::RepoScope::Recent { .. } => true,
        crate::model::RepoScope::Selected { repos } => repos.iter().any(|r| r.eq_ignore_ascii_case(name)),
        crate::model::RepoScope::None => false,
    }
}
