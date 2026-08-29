/// Notification severity — mirrors AlertSeverity but kept independent so
/// this module has no coupling to the alert engine.
/// Ordered: `Warn < Crit`, so folding several events into one page can keep
/// the loudest severity among them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    /// An alert rule crossed its threshold.
    Fired,
    /// An alert rule recovered.
    Resolved,
    /// A discrete host event (OOM kill, unclean reboot, SMART failure) — not
    /// part of an alert's fired→resolved lifecycle; it never resolves. Renders
    /// like `Fired` for urgency (severity-driven) but titled by the event.
    HostEvent,
    /// An alert action is waiting for a human. Unlike every other event here,
    /// this one is a *question*: something will happen on the host if the
    /// operator says yes, and nothing will if they don't answer. Channels
    /// render it at fire-level urgency for that reason — an unanswered
    /// proposal is a remediation that silently didn't happen.
    ActionRequired,
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
