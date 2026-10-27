//! Background polling. Each source has its own cadence; after every poll the
//! fresh `FleetView` is pushed to the frontend as `fleet-updated`. A five-second tick
//! re-derives the view for history and freshness, and pushes it only when its content
//! changed (a staleness flag flipped, say) or a 30-second heartbeat is due.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fleet::engine::Engine;
use fleet::model::FleetView;
use fleet::monitoring::FleetMachineEvent;
use fleet::sync::MutexExt;
use fleet::view_updates::{ViewPushes, revision};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::watch;

pub const FLEET_UPDATED: &str = "fleet-updated";

pub struct Poller {
    /// A generation counter: a loop that is mid-poll when it changes polls again right after.
    refresh: watch::Sender<u64>,
    pushes: Mutex<ViewPushes>,
}

impl Poller {
    /// Poll every source now. Requests made while a source is polling aren't lost.
    pub fn refresh_now(&self) {
        self.refresh
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Push {
    Always,
    /// Periodic re-derivations: only when the content changed or the heartbeat is due.
    IfChanged,
}

pub fn emit_view(app: &AppHandle, engine: &Engine) {
    push(app, engine.view(), Push::Always);
}

fn push(app: &AppHandle, view: FleetView, when: Push) {
    if let Some(poller) = app.try_state::<Poller>() {
        let revision = revision(&view).unwrap_or_else(|e| {
            tracing::warn!("fleet view revision failed: {e}");
            String::new()
        });
        let now = Instant::now();
        let mut pushes = poller.pushes.locked();
        if when == Push::IfChanged && !pushes.due(&revision, now) {
            return;
        }
        pushes.pushed(revision, now);
    }
    if let Err(e) = app.emit(FLEET_UPDATED, view) {
        tracing::warn!("emit failed: {e}");
    }
}

pub fn start(app: AppHandle, engine: Arc<Engine>) -> Poller {
    let (refresh, _) = watch::channel(0u64);

    spawn_loop(&app, &engine, &refresh, Duration::from_secs(15), |e| async move {
        e.poll_tailscale().await
    });
    spawn_loop(&app, &engine, &refresh, Duration::from_secs(60), |e| async move {
        if !e.tailnet_available() {
            e.poll_tailscale().await;
        }
        e.poll_machines().await
    });
    spawn_loop(&app, &engine, &refresh, Duration::from_secs(20), |e| async move {
        e.poll_github_runners().await
    });
    spawn_loop(&app, &engine, &refresh, Duration::from_secs(45), |e| async move {
        e.poll_github_jobs_and_history().await
    });

    // Re-derive periodically even without new data so history is recorded and
    // staleness thresholds advance on screen.
    {
        let app = app.clone();
        let engine = engine.clone();
        tauri::async_runtime::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let monitor = engine.clone();
                match tauri::async_runtime::spawn_blocking(move || monitor.monitoring_round(chrono::Utc::now())).await {
                    Ok((events, view)) => {
                        deliver_notifications(&app, &engine, events).await;
                        push(&app, view, Push::IfChanged);
                    }
                    Err(error) => {
                        tracing::warn!("recording machine history failed: {error}");
                        push(&app, engine.view(), Push::IfChanged);
                    }
                }
            }
        });
    }

    // Desired state: one read-only check shortly after launch, then on demand.
    if !engine.config().demo_mode {
        let engine = engine.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            if let Err(e) = engine.desired_refresh(None).await {
                tracing::info!("desired-state check skipped: {e}");
            }
        });
    }

    // Desired state is part of the fleet view: push it whenever a check or
    // change progresses.
    {
        let app = app.clone();
        let engine = engine.clone();
        let mut changes = engine.desired_changes();
        tauri::async_runtime::spawn(async move {
            while changes.changed().await.is_ok() {
                emit_view(&app, &engine);
            }
        });
    }

    Poller {
        refresh,
        pushes: Mutex::new(ViewPushes::default()),
    }
}

/// Ask the OS to show due alerts, then record that it accepted them so they aren't sent again.
/// Unaccepted requests stay queued and are retried by a later tick.
async fn deliver_notifications(app: &AppHandle, engine: &Arc<Engine>, events: Vec<FleetMachineEvent>) {
    let config = engine.config();
    if events.is_empty() || !config.desktop_notifications || config.demo_mode {
        return;
    }
    if let Err(error) = crate::notifications::send_alerts(app, &events) {
        tracing::warn!("notification request failed; will retry: {error}");
        return;
    }
    let monitor = engine.clone();
    match tauri::async_runtime::spawn_blocking(move || monitor.acknowledge_notifications(&events)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!("saving notification acknowledgement failed: {error}"),
        Err(error) => tracing::warn!("notification acknowledgement task failed: {error}"),
    }
}

fn spawn_loop<F, Fut>(app: &AppHandle, engine: &Arc<Engine>, refresh: &watch::Sender<u64>, period: Duration, poll: F)
where
    F: Fn(Arc<Engine>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let app = app.clone();
    let engine = engine.clone();
    // Subscribing marks the current generation seen; later refreshes wake the loop
    // even if they arrive while it is polling.
    let mut refresh = refresh.subscribe();
    tauri::async_runtime::spawn(async move {
        loop {
            if !engine.config().demo_mode {
                poll(engine.clone()).await;
                emit_view(&app, &engine);
            }
            tokio::select! {
                _ = tokio::time::sleep(period) => {}
                changed = refresh.changed() => {
                    if changed.is_err() {
                        // The poller is gone (app shutting down): keep the normal cadence.
                        tokio::time::sleep(period).await;
                    }
                }
            }
        }
    });
}
