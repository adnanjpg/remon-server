use async_trait::async_trait;
use reqwest::Client;

use crate::notify::channel::{ChannelError, NotificationChannel};
use crate::notify::types::{Notification, NotificationEvent};

pub struct WebhookChannel {
    http: Client,
    url: String,
    /// Optional Bearer token sent as `Authorization: Bearer <secret>`.
    secret: Option<String>,
}

impl WebhookChannel {
    pub fn new(url: String, secret: Option<String>, http: Client) -> Result<Self, ChannelError> {
        if url.is_empty() {
            return Err(ChannelError::Config(
                "webhook channel config missing 'url'".to_string(),
            ));
        }
        Ok(Self { http, url, secret })
    }
}

#[async_trait]
impl NotificationChannel for WebhookChannel {
    async fn send(&self, notification: &Notification) -> Result<usize, ChannelError> {
        let payload = serde_json::json!({
            "event": match notification.event {
                NotificationEvent::Fired    => "fired",
                NotificationEvent::Resolved => "resolved",
            },
            "severity": notification.severity.as_str(),
            "title":    notification.title,
            "body":     notification.body,
            "timestamp": chrono::Utc::now().timestamp(),
        });

        let mut req = self.http.post(&self.url).json(&payload);

        if let Some(ref secret) = self.secret {
            req = req.bearer_auth(secret);
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
            Err(ChannelError::Send(format!("webhook {} — {}", status, body)))
        }
    }

    fn type_name(&self) -> &'static str {
        "webhook"
    }
}
