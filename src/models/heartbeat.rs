//! Domain types for heartbeat checks — push-model dead-man's switches.
//!
//! The inverse of a probe: instead of this server running a script on a
//! schedule, an external job proves its own liveness by pinging a
//! capability URL. There is no watchdog task; state is a pure function
//! of the row's timestamps, computed here and shared by the REST DTOs
//! and the alert resolver so both always agree. The alert rule's eval
//! tick is the only clock that ever "notices" a missed deadline.
//!
//! All `now` values must come from `Utc::now().timestamp()` in the
//! caller — the same clock the alert evaluator stamps with — never from
//! SQL `unixepoch()`, so state derivation and rule evaluation can't
//! disagree across clock sources.

use serde::{Deserialize, Serialize};

/// Who declared the active pause. Operator pauses always win: a service
/// pause cannot be placed over one, and service `/resume` cannot lift one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PauseOrigin {
    Operator,
    Service,
}

impl PauseOrigin {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "operator" => Some(PauseOrigin::Operator),
            "service" => Some(PauseOrigin::Service),
            _ => None,
        }
    }
}

/// Derived check state, in precedence order. `late` is the grace window
/// — period elapsed but the deadline hasn't; `waiting` is a check that
/// has never pinged and is still inside its first window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeartbeatState {
    Disabled,
    Paused,
    Failed,
    Down,
    Waiting,
    Late,
    Up,
}

impl HeartbeatState {
    pub fn as_str(&self) -> &'static str {
        match self {
            HeartbeatState::Disabled => "disabled",
            HeartbeatState::Paused => "paused",
            HeartbeatState::Failed => "failed",
            HeartbeatState::Down => "down",
            HeartbeatState::Waiting => "waiting",
            HeartbeatState::Late => "late",
            HeartbeatState::Up => "up",
        }
    }
}

/// Kind of a received ping-family request, as logged in `heartbeat_pings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PingKind {
    Success,
    Fail,
    Pause,
    Resume,
}

impl PingKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PingKind::Success => "success",
            PingKind::Fail => "fail",
            PingKind::Pause => "pause",
            PingKind::Resume => "resume",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "success" => Some(PingKind::Success),
            "fail" => Some(PingKind::Fail),
            "pause" => Some(PingKind::Pause),
            "resume" => Some(PingKind::Resume),
            _ => None,
        }
    }
}

/// Row mirror of `heartbeat_checks` minus the SQL-only columns
/// (`slug_hash` is write/lookup-only, `pause_until_ping` steers the
/// auto-resume UPDATE) — neither is read by any Rust consumer.
#[derive(Debug, Clone)]
pub struct HeartbeatCheck {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub period_secs: i64,
    pub grace_secs: i64,
    pub enabled: bool,
    pub last_ping_at: Option<i64>,
    /// Explicit-failure latch — set by fail reports, cleared by the next
    /// success. A write-order flag rather than a timestamp comparison,
    /// so same-second fail→recover sequences read correctly.
    pub failed: bool,
    pub last_fail_at: Option<i64>,
    pub paused_at: Option<i64>,
    pub paused_until: Option<i64>,
    pub pause_origin: Option<PauseOrigin>,
    pub pause_reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl HeartbeatCheck {
    /// Anchor of the down-deadline: the check earns one full
    /// period+grace from whichever came last — its most recent ping, its
    /// creation, or the end of a pause window. The `max(paused_until)`
    /// term is what makes maintenance expiry grant a fresh window instead
    /// of firing the instant the pause lapses; expired pause columns are
    /// deliberately never cleared because of it.
    fn anchor(&self) -> i64 {
        self.last_ping_at
            .unwrap_or(self.created_at)
            .max(self.paused_until.unwrap_or(0))
    }

    /// When this check flips to `down`, absent further pings or pauses.
    pub fn deadline(&self) -> i64 {
        self.anchor() + self.period_secs + self.grace_secs
    }

    /// `paused_until = NULL` while `paused_at` is set means indefinite
    /// (operator-only; service pauses always carry a concrete end).
    pub fn is_paused(&self, now: i64) -> bool {
        self.paused_at.is_some() && self.paused_until.map(|until| now < until).unwrap_or(true)
    }

    pub fn state(&self, now: i64) -> HeartbeatState {
        if !self.enabled {
            return HeartbeatState::Disabled;
        }
        if self.is_paused(now) {
            return HeartbeatState::Paused;
        }
        // The explicit-fail latch beats every silence-derived state
        // (including a pause expiring) until the next success clears it.
        if self.failed {
            return HeartbeatState::Failed;
        }
        if now > self.deadline() {
            return HeartbeatState::Down;
        }
        if self.last_ping_at.is_none() {
            return HeartbeatState::Waiting;
        }
        if now > self.anchor() + self.period_secs {
            return HeartbeatState::Late;
        }
        HeartbeatState::Up
    }
}

/// Row mirror of `heartbeat_pings`.
#[derive(Debug, Clone, Serialize)]
pub struct HeartbeatPing {
    pub id: i64,
    pub check_id: i64,
    pub received_at: i64,
    pub kind: PingKind,
    pub exit_code: Option<i32>,
    pub source_ip: Option<String>,
    pub user_agent: Option<String>,
    pub body: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check() -> HeartbeatCheck {
        HeartbeatCheck {
            id: 1,
            name: "db-backup".into(),
            description: None,
            period_secs: 3600,
            grace_secs: 300,
            enabled: true,
            last_ping_at: None,
            failed: false,
            last_fail_at: None,
            paused_at: None,
            paused_until: None,
            pause_origin: None,
            pause_reason: None,
            created_at: 1000,
            updated_at: 1000,
        }
    }

    #[test]
    fn never_pinged_waits_then_downs() {
        let c = check();
        // Inside the first window: waiting, not late — there is no ping
        // to be late relative to.
        assert_eq!(c.state(1000), HeartbeatState::Waiting);
        assert_eq!(c.state(1000 + 3600 + 300), HeartbeatState::Waiting);
        // Past created_at + period + grace: a forgotten wiring is itself
        // a failure worth catching.
        assert_eq!(c.state(1000 + 3600 + 301), HeartbeatState::Down);
    }

    #[test]
    fn up_late_down_progression() {
        let mut c = check();
        c.last_ping_at = Some(10_000);
        assert_eq!(c.state(10_000 + 3600), HeartbeatState::Up);
        assert_eq!(c.state(10_000 + 3601), HeartbeatState::Late);
        assert_eq!(c.state(10_000 + 3900), HeartbeatState::Late);
        assert_eq!(c.state(10_000 + 3901), HeartbeatState::Down);
    }

    #[test]
    fn pause_overrides_down_and_expiry_grants_fresh_window() {
        let mut c = check();
        c.last_ping_at = Some(10_000);
        c.paused_at = Some(10_100);
        c.paused_until = Some(50_000);
        // Deep past the original deadline, but paused.
        assert_eq!(c.state(40_000), HeartbeatState::Paused);
        // Pause expired with no ping since: fresh period+grace from the
        // window end, not instant-down.
        assert_eq!(c.state(50_000 + 3600), HeartbeatState::Up);
        assert_eq!(c.state(50_000 + 3601), HeartbeatState::Late);
        assert_eq!(c.state(50_000 + 3901), HeartbeatState::Down);
    }

    #[test]
    fn indefinite_pause_never_expires() {
        let mut c = check();
        c.last_ping_at = Some(10_000);
        c.paused_at = Some(10_100);
        c.paused_until = None;
        assert_eq!(c.state(10_000_000), HeartbeatState::Paused);
    }

    #[test]
    fn ping_during_pause_does_not_resume() {
        let mut c = check();
        c.paused_at = Some(10_000);
        c.paused_until = Some(20_000);
        c.last_ping_at = Some(15_000);
        assert_eq!(c.state(16_000), HeartbeatState::Paused);
    }

    #[test]
    fn fail_latches_until_next_success() {
        let mut c = check();
        c.last_ping_at = Some(10_000);
        c.failed = true;
        assert_eq!(c.state(10_600), HeartbeatState::Failed);
        // Fail survives what would otherwise be a healthy window.
        assert_eq!(c.state(10_000 + 3600), HeartbeatState::Failed);
        // The next success clears the latch (repository does the clear).
        c.failed = false;
        c.last_ping_at = Some(10_500);
        assert_eq!(c.state(10_600), HeartbeatState::Up);
    }

    #[test]
    fn pause_beats_failed_disabled_beats_all() {
        let mut c = check();
        c.failed = true;
        c.paused_at = Some(2100);
        c.paused_until = None;
        assert_eq!(c.state(2200), HeartbeatState::Paused);
        c.enabled = false;
        assert_eq!(c.state(2200), HeartbeatState::Disabled);
    }

    #[test]
    fn resume_grants_fresh_window() {
        // Operator resume sets paused_until = now; the anchor max() then
        // gives one full period+grace from the resume instant.
        let mut c = check();
        c.last_ping_at = Some(10_000);
        c.paused_at = Some(10_100);
        c.paused_until = Some(30_000); // resumed at 30_000
        assert_eq!(c.state(30_000 + 3600), HeartbeatState::Up);
        assert_eq!(c.state(30_000 + 3901), HeartbeatState::Down);
    }

    #[test]
    fn backwards_clock_reads_up_and_self_heals() {
        let mut c = check();
        c.last_ping_at = Some(50_000);
        // now < last_ping (NTP step back): anchor in the future → up.
        assert_eq!(c.state(49_000), HeartbeatState::Up);
    }
}
