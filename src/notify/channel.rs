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

    /// Whether this channel performs its own per-target timeout + retry and
    /// must therefore NOT be re-invoked by the fanout wrapper. Multi-target
    /// channels (FCM, Web Push) re-deliver to *every* subscriber on each
    /// `send()`, so a whole-channel retry would double-notify devices that
    /// already received the push. Single-target channels leave this `false`
    /// and rely on the wrapper's one-shot retry.
    fn self_retries(&self) -> bool {
        false
    }
}
