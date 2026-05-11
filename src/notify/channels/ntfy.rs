use async_trait::async_trait;
use reqwest::Client;

use crate::notify::channel::{ChannelError, NotificationChannel};
use crate::notify::types::{Notification, NotificationEvent, Severity};

pub struct NtfyChannel {
    http: Client,
    /// Base URL, e.g. "https://ntfy.sh" or self-hosted "https://push.example.com"
    server: String,
    topic: String,
    /// Optional Bearer token for authenticated ntfy servers.
    token: Option<String>,
}

impl NtfyChannel {
    pub fn new(
        server: String,
        topic: String,
        token: Option<String>,
        http: Client,
    ) -> Result<Self, ChannelError> {
        if topic.is_empty() {
            return Err(ChannelError::Config(
                "ntfy channel config missing 'topic'".to_string(),
            ));
        }
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
        })
    }
}

fn priority(n: &Notification) -> &'static str {
    match (n.event, n.severity) {
        (NotificationEvent::Fired, Severity::Crit) => "urgent",
        (NotificationEvent::Fired, Severity::Warn) => "high",
        (NotificationEvent::Resolved, _) => "low",
    }
}

fn tags(n: &Notification) -> &'static str {
    match n.event {
        NotificationEvent::Fired => "rotating_light",
        NotificationEvent::Resolved => "white_check_mark",
    }
}

#[async_trait]
impl NotificationChannel for NtfyChannel {
    async fn send(&self, notification: &Notification) -> Result<usize, ChannelError> {
        let url = format!("{}/{}", self.server, self.topic);

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
            .map_err(|e| ChannelError::Send(e.to_string()))?;

        if resp.status().is_success() {
            Ok(1)
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(ChannelError::Send(format!("ntfy {} — {}", status, body)))
        }
    }

    fn type_name(&self) -> &'static str {
        "ntfy"
    }
}
