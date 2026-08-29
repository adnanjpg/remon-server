use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;

use crate::notify::channel::{ChannelError, NotificationChannel};
use crate::notify::types::{Notification, NotificationEvent, Severity};
use crate::notify::url_policy::{WebhookPolicy, check_url};

pub struct NtfyChannel {
    http: Client,
    /// Base URL, e.g. "https://ntfy.sh" or self-hosted "https://push.example.com"
    server: String,
    topic: String,
    /// Optional Bearer token for authenticated ntfy servers.
    token: Option<String>,
    /// SSRF policy applied at every send (re-resolved as a DNS-rebinding
    /// defense), same gate the webhook channel uses. A self-hosted ntfy
    /// `server` on a private address is rejected unless explicitly allowed.
    policy: Arc<WebhookPolicy>,
}

impl NtfyChannel {
    pub fn new(
        server: String,
        topic: String,
        token: Option<String>,
        http: Client,
        policy: Arc<WebhookPolicy>,
    ) -> Result<Self, ChannelError> {
        if topic.is_empty() {
            return Err(ChannelError::Config(
                "ntfy channel config missing 'topic'".to_string(),
            ));
        }
        // Trim before the empty/default check so this matches channel_check_url
        // exactly — otherwise a leading-whitespace server passes create-time
        // validation but is rejected at send time when the URL fails to parse.
        let server = server.trim();
        let server = if server.is_empty() {
            "https://ntfy.sh".to_string()
        } else {
            server.trim_end_matches('/').to_string()
        };
        Ok(Self {
            http,
            server,
            topic,
            token,
            policy,
        })
    }
}

fn priority(n: &Notification) -> &'static str {
    use NotificationEvent::{ActionRequired, Fired, HostEvent};
    match (n.event, n.severity) {
        (Fired | HostEvent | ActionRequired, Severity::Crit) => "urgent",
        (Fired | HostEvent | ActionRequired, Severity::Warn) => "high",
        (NotificationEvent::Resolved, _) => "low",
    }
}

fn tags(n: &Notification) -> &'static str {
    match n.event {
        NotificationEvent::Fired => "rotating_light",
        NotificationEvent::HostEvent => "warning",
        // A question, not a report — the icon says "you have a decision".
        NotificationEvent::ActionRequired => "question",
        NotificationEvent::Resolved => "white_check_mark",
    }
}

#[async_trait]
impl NotificationChannel for NtfyChannel {
    async fn send(&self, notification: &Notification) -> Result<usize, ChannelError> {
        let url = format!("{}/{}", self.server, self.topic);

        // Re-resolve and re-check on every send (DNS-rebinding defense).
        check_url(&url, &self.policy)
            .await
            .map_err(ChannelError::Send)?;

        let mut req = self
            .http
            .post(&url)
            .header("Title", &notification.title)
            .header("Priority", priority(notification))
            .header("Tags", tags(notification))
            .body(notification.body.clone());

        if let Some(ref token) = self.token {
            req = req.bearer_auth(token);
        }

        let resp = req
            .send()
            .await
            // The ntfy topic (a bearer-equivalent secret for unauthenticated
            // servers) lives in this URL — keep it out of the logged error.
            .map_err(|e| ChannelError::Send(e.without_url().to_string()))?;

        if resp.status().is_success() {
            Ok(1)
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(ChannelError::Send(format!("ntfy {} — {}", status, body)))
        }
    }
}
