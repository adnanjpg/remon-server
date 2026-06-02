pub mod fcm;
pub mod ntfy;
pub mod telegram;
pub mod webhook;
pub mod webpush;

use std::sync::Arc;

use reqwest::Client;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::config::NotificationsConfig;
use crate::notify::channel::{ChannelError, NotificationChannel};
use crate::notify::url_policy::WebhookPolicy;
use crate::services::webpush::VapidKeyPair;

use fcm::FcmChannel;
use ntfy::NtfyChannel;
use telegram::TelegramChannel;
use webhook::WebhookChannel;
use webpush::WebPushChannel;

/// Build a channel instance from a DB row.
///
/// `channel_type`   — one of "fcm" | "telegram" | "ntfy" | "webhook" | "web-push".
/// `config`         — parsed JSON from `notification_channels.config`.
/// `credentials`    — server-side secrets loaded from config files / env vars.
/// `webhook_policy` — SSRF / private-range gate applied to webhook URLs at
///                    send time. Shared across all webhook channels in one
///                    reload pass.
///
/// Returns `Err(ChannelError::Config)` if required fields are missing or
/// the credential path can't be read. The manager logs the error and skips
/// the channel rather than crashing.
pub async fn build_channel(
    channel_type: &str,
    config: &Value,
    credentials: &NotificationsConfig,
    http: Client,
    pool: SqlitePool,
    vapid: &Arc<VapidKeyPair>,
    webhook_policy: &Arc<WebhookPolicy>,
) -> Result<Box<dyn NotificationChannel>, ChannelError> {
    match channel_type {
        "fcm" => {
            let path = &credentials.fcm.service_account_path;
            if path.is_empty() {
                return Err(ChannelError::Config(
                    "FCM requires notifications.fcm.service_account_path to be set".to_string(),
                ));
            }
            Ok(Box::new(FcmChannel::new(path, http, pool).await?))
        }

        "web-push" => {
            // No per-channel config — VAPID is server-wide; subscribers
            // are tracked on `devices` rows. Operator just toggles the
            // channel on; the rest is automatic.
            let _ = config; // suppress unused-var lint at this branch
            Ok(Box::new(WebPushChannel::new(
                Arc::clone(vapid),
                pool,
                http,
                Arc::clone(webhook_policy),
            )?))
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
            Ok(Box::new(NtfyChannel::new(
                server,
                topic,
                token,
                http,
                Arc::clone(webhook_policy),
            )?))
        }

        "webhook" => {
            let url = config["url"].as_str().unwrap_or("").to_string();
            let secret = if credentials
                .webhook
                .secret
                .as_deref()
                .unwrap_or("")
                .is_empty()
            {
                None
            } else {
                credentials.webhook.secret.clone()
            };
            Ok(Box::new(WebhookChannel::new(
                url,
                secret,
                http,
                Arc::clone(webhook_policy),
            )?))
        }

        other => Err(ChannelError::Config(format!(
            "unknown channel type '{}'",
            other
        ))),
    }
}
