//! Alert destinations besides desktop notifications: Slack and Discord
//! incoming webhooks, ntfy, and a generic JSON webhook.
//!
//! The settings keep only what isn't secret (kind, name, an ntfy server and
//! topic, and a host summary for display). Webhook URLs and ntfy tokens are
//! kept in the [`SecretStore`](crate::engine::SecretStore) under
//! `alert-destination:<id>`. Deliveries go through the monitoring journal's
//! durable outbox: saved before sending, retried with backoff, and removed
//! once a destination accepts them. Requests use HTTPS (plain HTTP only to
//! this machine), a 10-second timeout, and no redirects; errors never repeat a
//! secret URL.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::monitoring::{FleetEventKind, FleetMachineEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export, rename = "FleetAlertDestinationKind")]
#[serde(rename_all = "camelCase")]
pub enum AlertDestinationKind {
    /// A Slack incoming webhook.
    Slack,
    /// A Discord channel webhook.
    Discord,
    /// An ntfy server and topic, with an optional access token.
    Ntfy,
    /// Any endpoint that accepts a JSON POST.
    Webhook,
}

/// One destination as saved in the app settings. No secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export, rename = "FleetAlertDestination")]
#[serde(rename_all = "camelCase")]
pub struct AlertDestination {
    pub id: String,
    pub name: String,
    pub kind: AlertDestinationKind,
    pub enabled: bool,
    /// ntfy only: the server, such as `https://ntfy.sh`.
    #[serde(default)]
    pub ntfy_server: Option<String>,
    /// ntfy only: the topic.
    #[serde(default)]
    pub ntfy_topic: Option<String>,
    /// Where it sends, for display: the webhook's host or the ntfy topic.
    pub summary: String,
}

/// A destination as Settings shows it, with its delivery state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export, rename = "FleetAlertDestinationView")]
#[serde(rename_all = "camelCase")]
pub struct FleetAlertDestinationView {
    pub id: String,
    pub name: String,
    pub kind: AlertDestinationKind,
    pub enabled: bool,
    pub summary: String,
    /// Alerts saved for delivery and not yet accepted.
    pub pending: u32,
    pub delivered_at: Option<DateTime<Utc>>,
    /// The last delivery failure, until a later delivery succeeds.
    pub error: Option<String>,
}

/// A new or edited destination from Settings. Secrets arrive here and go
/// straight to the Keychain; they are never sent back.
#[derive(Clone, Default, PartialEq, Deserialize, TS)]
#[ts(export, rename = "FleetAlertDestinationInput")]
#[serde(rename_all = "camelCase")]
pub struct AlertDestinationInput {
    /// `None` adds a destination.
    pub id: Option<String>,
    pub name: String,
    pub kind: Option<AlertDestinationKind>,
    pub enabled: bool,
    /// Slack, Discord and webhook URL. When editing, `None` keeps the saved one.
    pub url: Option<String>,
    pub ntfy_server: Option<String>,
    pub ntfy_topic: Option<String>,
    /// ntfy access token. When editing, `None` keeps it and an empty value removes it.
    pub token: Option<String>,
}

impl std::fmt::Debug for AlertDestinationInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlertDestinationInput")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("enabled", &self.enabled)
            .field("url", &self.url.as_ref().map(|_| "[redacted]"))
            .field("ntfy_server", &self.ntfy_server)
            .field("ntfy_topic", &self.ntfy_topic)
            .field("token", &self.token.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// What the Keychain holds for one destination.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DestinationSecret {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
}

impl std::fmt::Debug for DestinationSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DestinationSecret")
            .field("url", &self.url.as_ref().map(|_| "[redacted]"))
            .field("token", &self.token.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// The Keychain account for a destination's secret. Organization logins
/// (the other accounts) can't contain `:`.
pub(crate) fn secret_account(id: &str) -> String {
    format!("alert-destination:{id}")
}

/// A URL alerts may be sent to: HTTPS, or plain HTTP to this machine only.
pub fn check_url(text: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(text.trim()).map_err(|_| "Enter a complete URL, starting with https://.".to_string())?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    match url.scheme() {
        "https" if url.host_str().is_some() => Ok(url),
        "http" if loopback => Ok(url),
        _ => Err("Use an https:// URL.".into()),
    }
}

/// An ntfy server, which is saved in the settings file: no credentials, query
/// or fragment, which would end up there in plain text.
pub fn check_ntfy_server(text: &str) -> Result<url::Url, String> {
    let url = check_url(text)?;
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err(
            "Enter only the ntfy server, such as https://ntfy.sh, and put an access token in the token field.".into(),
        );
    }
    Ok(url)
}

/// ntfy topics: letters, digits, `-` and `_`.
pub fn check_topic(topic: &str) -> Result<(), String> {
    if !topic.is_empty()
        && topic.len() <= 64
        && topic.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(())
    } else {
        Err("Use an ntfy topic of up to 64 letters, digits, dashes and underscores.".into())
    }
}

/// A message to send: a monitoring event, or a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Notice {
    pub title: String,
    pub body: String,
    /// Recoveries are sent at normal priority, alerts at high priority.
    pub recovery: bool,
    pub event: Option<FleetMachineEvent>,
}

/// Events that announce something is working again.
pub fn is_recovery(kind: &FleetEventKind) -> bool {
    matches!(
        kind,
        FleetEventKind::Online
            | FleetEventKind::DiskRecovered
            | FleetEventKind::SshRecovered
            | FleetEventKind::WatchdogRestarted
            | FleetEventKind::WatchdogRecovered
    )
}

pub(crate) fn notice(event: &FleetMachineEvent) -> Notice {
    let recovery = is_recovery(&event.kind);
    Notice {
        title: format!(
            "{}: {}",
            if recovery { "Recovered" } else { "Alert" },
            event.machine_name
        ),
        body: event.detail.clone(),
        recovery,
        event: Some(event.clone()),
    }
}

pub(crate) fn test_notice() -> Notice {
    Notice {
        title: "Runner Dashboard test".into(),
        body: "This destination receives Runner Dashboard alerts and recoveries.".into(),
        recovery: true,
        event: None,
    }
}

/// An HTTP request ready to send.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub url: url::Url,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("host", &self.url.host_str())
            .finish_non_exhaustive()
    }
}

const JSON: &str = "application/json";

/// The request that delivers `notice` to `destination`.
pub(crate) fn request(
    destination: &AlertDestination,
    secret: &DestinationSecret,
    notice: &Notice,
) -> Result<Request, String> {
    let text = format!("{}\n{}", notice.title, notice.body);
    let webhook = || {
        secret
            .url
            .as_deref()
            .ok_or_else(|| format!("{} has no saved webhook URL; edit it in Settings.", destination.name))
            .and_then(check_url)
    };
    let json = |url, value: serde_json::Value| Request {
        url,
        headers: vec![("content-type", JSON.into())],
        body: value.to_string().into_bytes(),
    };
    Ok(match destination.kind {
        AlertDestinationKind::Slack => json(webhook()?, serde_json::json!({ "text": text })),
        AlertDestinationKind::Discord => {
            let content: String = text.chars().take(2000).collect();
            json(webhook()?, serde_json::json!({ "content": content }))
        }
        AlertDestinationKind::Webhook => json(
            webhook()?,
            serde_json::json!({
                "source": "runner-dashboard",
                "title": notice.title,
                "text": notice.body,
                "recovery": notice.recovery,
                "event": notice.event,
            }),
        ),
        AlertDestinationKind::Ntfy => {
            let server = destination
                .ntfy_server
                .as_deref()
                .ok_or_else(|| format!("{} has no ntfy server.", destination.name))?;
            let topic = destination
                .ntfy_topic
                .as_deref()
                .ok_or_else(|| format!("{} has no ntfy topic.", destination.name))?;
            check_topic(topic)?;
            let mut url = check_url(server)?;
            url.path_segments_mut()
                .map_err(|()| "That ntfy server URL can't take a topic.".to_string())?
                .pop_if_empty()
                .push(topic);
            let mut headers = vec![
                ("title", ascii_header(&notice.title)),
                ("priority", if notice.recovery { "default" } else { "high" }.to_string()),
                (
                    "tags",
                    if notice.recovery { "white_check_mark" } else { "warning" }.to_string(),
                ),
            ];
            if let Some(token) = secret.token.as_deref().filter(|t| !t.is_empty()) {
                headers.push(("authorization", format!("Bearer {token}")));
            }
            Request {
                url,
                headers,
                body: notice.body.clone().into_bytes(),
            }
        }
    })
}

/// HTTP header values must be visible ASCII; anything else becomes `?`.
fn ascii_header(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' })
        .collect()
}

/// The client every delivery uses: 10-second timeout and no redirects, so a
/// webhook can't send the request (or its secret path) somewhere else.
pub(crate) fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("Couldn't prepare the HTTP client: {}", e.without_url()))
}

/// Sends one request; any 2xx status is success.
pub(crate) async fn send(client: &reqwest::Client, request: Request) -> Result<(), String> {
    let mut builder = client.post(request.url).body(request.body);
    for (name, value) in request.headers {
        builder = builder.header(name, value);
    }
    let response = builder
        .send()
        .await
        .map_err(|e| format!("Couldn't reach the destination: {}", e.without_url()))?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("The destination answered HTTP {}.", status.as_u16()))
    }
}

/// What Settings shows about where a destination sends, without secrets.
pub(crate) fn summary(kind: AlertDestinationKind, url: Option<&url::Url>, topic: Option<&str>) -> String {
    let host = url.and_then(url::Url::host_str).unwrap_or_default();
    match kind {
        AlertDestinationKind::Ntfy => format!("{host}/{}", topic.unwrap_or_default()),
        AlertDestinationKind::Slack | AlertDestinationKind::Discord | AlertDestinationKind::Webhook => host.to_string(),
    }
}

/// Minutes to wait before retrying a delivery after `attempts` failures.
pub(crate) fn retry_after(attempts: u32) -> chrono::Duration {
    const MINUTES: [i64; 6] = [1, 2, 5, 15, 30, 60];
    let index = usize::try_from(attempts.saturating_sub(1)).unwrap_or(usize::MAX);
    chrono::Duration::minutes(MINUTES[index.min(MINUTES.len() - 1)])
}

/// Undelivered alerts are dropped after a day.
pub(crate) const GIVE_UP_AFTER_HOURS: i64 = 24;

#[cfg(test)]
#[path = "outbound_tests.rs"]
mod tests;
