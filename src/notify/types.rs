/// Notification severity — mirrors AlertSeverity but kept independent so
/// this module has no coupling to the alert engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warn,
    Crit,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Crit => "crit",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Warn => "[Warning] ",
            Self::Crit => "[Critical] ",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationEvent {
    Fired,
    Resolved,
}

/// A notification ready to be dispatched to one or more channels.
///
/// `title` and `body` are plain text — channels are responsible for any
/// channel-specific formatting (HTML for Telegram, priority headers for
/// ntfy, etc.) using `severity` and `event` as signals.
#[derive(Debug, Clone)]
pub struct Notification {
    /// Short, human-readable title (e.g. rule name or "Service failed: nginx").
    pub title: String,
    /// Supporting detail — metric values, labels, context.
    pub body: String,
    pub severity: Severity,
    pub event: NotificationEvent,
}
