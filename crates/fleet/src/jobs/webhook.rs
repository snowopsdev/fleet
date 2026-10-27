//! The loopback receiver for `workflow_job` deliveries relayed by `gh webhook forward`.
//!
//! It's a separate listener from the read-only local API. Every request must come from a
//! loopback address without an `Origin` header and carry a valid `X-Hub-Signature-256`
//! for this launch's secret; anything else is refused before the body is parsed.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use serde::Deserialize;

use super::FinishedJob;
use crate::model::GhJob;

/// The path the forwarder posts to.
pub const RECEIVER_PATH: &str = "/github/workflow-job";
// workflow_job payloads are a few KiB; the rest of GitHub's 25 MB allowance isn't needed.
const MAX_BODY: usize = 1024 * 1024;

/// What happened to a job, per the delivery's `action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedAction {
    Queued,
    Waiting,
    InProgress,
    Completed,
}

/// One validated `workflow_job` delivery.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedDelivery {
    /// Organization login, lowercase.
    pub org: String,
    pub action: FeedAction,
    pub job: FinishedJob,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    #[error("the delivery isn't a valid workflow_job payload: {0}")]
    Payload(String),
    #[error("unsupported workflow_job action {0:?}")]
    Action(String),
}

/// Checks GitHub's `sha256=<hex>` signature in constant time.
pub fn verify_signature(secret: &[u8], body: &[u8], header: Option<&str>) -> bool {
    let Some(signature) = header.and_then(|h| h.strip_prefix("sha256=")).and_then(decode_hex) else {
        return false;
    };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    ring::hmac::verify(&key, body, &signature).is_ok()
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A random per-launch secret, hex encoded. It's never written to disk.
pub fn new_secret() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .map_err(|_| "Couldn't generate a webhook secret.".to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Deserialize)]
struct Payload {
    action: String,
    workflow_job: RawWorkflowJob,
    repository: Repository,
    organization: Option<Account>,
}

#[derive(Deserialize)]
struct Repository {
    full_name: String,
    owner: Account,
}

#[derive(Deserialize)]
struct Account {
    login: String,
}

#[derive(Deserialize)]
struct RawWorkflowJob {
    id: u64,
    run_id: u64,
    name: String,
    workflow_name: Option<String>,
    status: String,
    conclusion: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    runner_id: Option<u64>,
    runner_name: Option<String>,
    created_at: Option<DateTime<Utc>>,
    started_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    html_url: Option<String>,
    head_branch: Option<String>,
}

/// Parses a `workflow_job` delivery body.
pub fn parse_workflow_job(body: &[u8]) -> Result<FeedDelivery, WebhookError> {
    let payload: Payload = serde_json::from_slice(body).map_err(|e| WebhookError::Payload(e.to_string()))?;
    let action = match payload.action.as_str() {
        "queued" => FeedAction::Queued,
        "waiting" => FeedAction::Waiting,
        "in_progress" => FeedAction::InProgress,
        "completed" => FeedAction::Completed,
        other => return Err(WebhookError::Action(other.to_string())),
    };
    let org = payload
        .organization
        .map_or(payload.repository.owner.login, |o| o.login)
        .to_lowercase();
    let raw = payload.workflow_job;
    Ok(FeedDelivery {
        org,
        action,
        job: FinishedJob {
            job: GhJob {
                id: raw.id,
                run_id: raw.run_id,
                repo: payload.repository.full_name,
                name: raw.name,
                workflow_name: raw.workflow_name,
                status: raw.status,
                labels: raw.labels,
                runner_id: raw.runner_id.filter(|id| *id != 0),
                runner_name: raw.runner_name.filter(|n| !n.is_empty()),
                created_at: raw.created_at,
                started_at: raw.started_at,
                html_url: raw.html_url,
                head_branch: raw.head_branch,
            },
            conclusion: raw.conclusion,
            completed_at: raw.completed_at,
        },
    })
}

/// Handles each validated delivery; returns once it's recorded.
pub type DeliverySink = Arc<dyn Fn(FeedDelivery) -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Clone)]
struct Receiver {
    secret: Arc<[u8]>,
    sink: DeliverySink,
}

/// The receiver's routes. Serve with `into_make_service_with_connect_info::<SocketAddr>()`.
pub fn router(secret: &str, sink: DeliverySink) -> Router {
    Router::new()
        .route(RECEIVER_PATH, post(deliver))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(Receiver {
            secret: Arc::from(secret.as_bytes()),
            sink,
        })
}

async fn deliver(State(receiver): State<Receiver>, request: Request<axum::body::Body>) -> StatusCode {
    if let Some(ConnectInfo(peer)) = request.extensions().get::<ConnectInfo<SocketAddr>>()
        && !peer.ip().is_loopback()
    {
        return StatusCode::FORBIDDEN;
    }
    let (parts, body) = request.into_parts();
    if parts.headers.contains_key("origin") {
        return StatusCode::FORBIDDEN;
    }
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY).await else {
        return StatusCode::PAYLOAD_TOO_LARGE;
    };
    handle(&receiver, &parts.headers, &body).await
}

async fn handle(receiver: &Receiver, headers: &HeaderMap, body: &Bytes) -> StatusCode {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if !verify_signature(&receiver.secret, body, header("x-hub-signature-256")) {
        return StatusCode::UNAUTHORIZED;
    }
    match header("x-github-event") {
        Some("ping") => StatusCode::OK,
        Some("workflow_job") => match parse_workflow_job(body) {
            Ok(delivery) => {
                (receiver.sink)(delivery).await;
                StatusCode::OK
            }
            Err(WebhookError::Action(_)) => StatusCode::ACCEPTED,
            Err(error) => {
                tracing::warn!("job feed: {error}");
                StatusCode::UNPROCESSABLE_ENTITY
            }
        },
        // Only workflow_job is requested; anything else is acknowledged and ignored.
        _ => StatusCode::ACCEPTED,
    }
}

#[cfg(test)]
pub(crate) fn sign(secret: &str, body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let tag = ring::hmac::sign(&key, body);
    format!(
        "sha256={}",
        tag.as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}
