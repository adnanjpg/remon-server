pub mod fcm;
pub mod ntfy;
pub mod telegram;
pub mod webhook;

use reqwest::Client;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::config::NotificationsConfig;
use crate::notify::channel::{ChannelError, NotificationChannel};

use fcm::FcmChannel;
use ntfy::NtfyChannel;
use telegram::TelegramChannel;
use webhook::WebhookChannel;

/// Build a channel instance from a DB row.
///
/// `channel_type` — one of "fcm" | "telegram" | "ntfy" | "webhook".
/// `config`       — parsed JSON from `notification_channels.config`.
/// `credentials`  — server-side secrets loaded from config files / env vars.
///
/// Returns `Err(ChannelError::Config)` if required fields are missing or
/// the credential path can't be read. The manager logs the error and skips
/// the channel rather than crashing.
pub fn build_channel(
    channel_type: &str,
    config: &Value,
    credentials: &NotificationsConfig,
    http: Client,
    pool: SqlitePool,
) -> Result<Box<dyn NotificationChannel>, ChannelError> {
    match channel_type {
        "fcm" => {
            let path = &credentials.fcm.service_account_path;
            if path.is_empty() {
                return Err(ChannelError::Config(
                    "FCM requires notifications.fcm.service_account_path to be set".to_string(),
                ));
            }
            Ok(Box::new(FcmChannel::new(path, http, pool)?))
        }

        "telegram" => {
            let bot_token = credentials.telegram.bot_token.clone();
            let chat_id = config["chat_id"].as_str().unwrap_or("").to_string();
            Ok(Box::new(TelegramChannel::new(bot_token, chat_id, http)?))
        }

        "ntfy" => {
            let server = config["server"].as_str().unwrap_or("").to_string();
            let topic = config["topic"].as_str().unwrap_or("").to_string();
            let token = if credentials.ntfy.token.as_deref().unwrap_or("").is_empty() {
                None
            } else {
                credentials.ntfy.token.clone()
            };
            Ok(Box::new(NtfyChannel::new(server, topic, token, http)?))
        }

        "webhook" => {
            let url = config["url"].as_str().unwrap_or("").to_string();
            let secret = if credentials.webhook.secret.as_deref().unwrap_or("").is_empty() {
                None
            } else {
                credentials.webhook.secret.clone()
            };
            Ok(Box::new(WebhookChannel::new(url, secret, http)?))
        }

        other => Err(ChannelError::Config(format!(
            "unknown channel type '{}'",
            other
        ))),
    }
}
