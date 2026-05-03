use async_trait::async_trait;

use super::types::Notification;

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("send failed: {0}")]
    Send(String),
    #[error("misconfigured: {0}")]
    Config(String),
}

#[async_trait]
pub trait NotificationChannel: Send + Sync {
    /// Dispatch a notification. Returns the number of successful deliveries.
    ///
    /// Single-target channels (Telegram, ntfy, webhook) return 0 or 1.
    /// FCM returns the number of paired devices successfully reached.
    async fn send(&self, notification: &Notification) -> Result<usize, ChannelError>;

    fn type_name(&self) -> &'static str;
}
