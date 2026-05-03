use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;

use crate::notify::channel::{ChannelError, NotificationChannel};
use crate::notify::types::{Notification, NotificationEvent, Severity};

pub struct TelegramChannel {
    http: Client,
    bot_token: String,
    chat_id: String,
}

impl TelegramChannel {
    pub fn new(bot_token: String, chat_id: String, http: Client) -> Result<Self, ChannelError> {
        if bot_token.is_empty() {
            return Err(ChannelError::Config(
                "Telegram bot_token not set (notifications.telegram.bot_token)".to_string(),
            ));
        }
        if chat_id.is_empty() {
            return Err(ChannelError::Config(
                "Telegram channel config missing 'chat_id'".to_string(),
            ));
        }
        Ok(Self { http, bot_token, chat_id })
    }
}

fn format_message(n: &Notification) -> String {
    let icon = match (n.event, n.severity) {
        (NotificationEvent::Fired, Severity::Crit) => "🔴",
        (NotificationEvent::Fired, Severity::Warn) => "🟡",
        (NotificationEvent::Resolved, _) => "✅",
    };
    let prefix = if n.event == NotificationEvent::Resolved {
        "Resolved: "
    } else {
        ""
    };
    format!(
        "{} <b>{}{}</b>\n\n{}\n\n<i>via Remon</i>",
        icon,
        prefix,
        escape_html(&n.title),
        escape_html(&n.body),
    )
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[async_trait]
impl NotificationChannel for TelegramChannel {
    async fn send(&self, notification: &Notification) -> Result<usize, ChannelError> {
        let url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            self.bot_token
        );

        #[derive(Deserialize)]
        struct TgResponse {
            ok: bool,
            description: Option<String>,
        }

        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({
                "chat_id": self.chat_id,
                "text": format_message(notification),
                "parse_mode": "HTML",
            }))
            .send()
            .await
            .map_err(|e| ChannelError::Send(e.to_string()))?;

        let status = resp.status();
        let body: TgResponse = resp
            .json()
            .await
            .map_err(|e| ChannelError::Send(format!("parse response: {}", e)))?;

        if body.ok {
            Ok(1)
        } else {
            Err(ChannelError::Send(format!(
                "Telegram API error ({}): {}",
                status,
                body.description.as_deref().unwrap_or("unknown")
            )))
        }
    }

    fn type_name(&self) -> &'static str {
        "telegram"
    }
}
