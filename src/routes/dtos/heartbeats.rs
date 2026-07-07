//! Heartbeat REST DTOs.

use serde::{Deserialize, Serialize};

use crate::models::heartbeat::{
    HeartbeatCheck, HeartbeatPing, HeartbeatState, PauseOrigin, PingKind,
};

#[derive(Debug, Serialize)]
pub struct HeartbeatCheckDto {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub period_secs: i64,
    pub grace_secs: i64,
    pub enabled: bool,
    /// Derived at read time from the check's timestamps — same function
    /// the alert resolver uses, so UI and alerting never disagree.
    pub state: HeartbeatState,
    pub last_ping_at: Option<i64>,
    pub last_fail_at: Option<i64>,
    /// When the check flips to `down`, absent further pings or pauses.
    pub deadline_at: i64,
    pub paused: bool,
    /// Pause detail is exposed only while a pause is active — the columns
    /// persist after expiry (they anchor the deadline) but stale values
    /// are an implementation detail, not API surface.
    pub paused_until: Option<i64>,
    pub pause_origin: Option<PauseOrigin>,
    pub pause_reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub fn check_dto_from(check: HeartbeatCheck, now: i64) -> HeartbeatCheckDto {
    let state = check.state(now);
    let paused = check.is_paused(now);
    let deadline_at = check.deadline();
    HeartbeatCheckDto {
        id: check.id,
        name: check.name,
        description: check.description,
        period_secs: check.period_secs,
        grace_secs: check.grace_secs,
        enabled: check.enabled,
        state,
        last_ping_at: check.last_ping_at,
        last_fail_at: check.last_fail_at,
        deadline_at,
        paused,
        paused_until: if paused { check.paused_until } else { None },
        pause_origin: if paused { check.pause_origin } else { None },
        pause_reason: if paused { check.pause_reason } else { None },
        created_at: check.created_at,
        updated_at: check.updated_at,
    }
}

#[derive(Debug, Serialize)]
pub struct ListHeartbeatsResponse {
    pub checks: Vec<HeartbeatCheckDto>,
}

#[derive(Debug, Deserialize)]
pub struct CreateHeartbeatRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub period_secs: i64,
    #[serde(default = "default_grace")]
    pub grace_secs: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Create the check under an indefinite operator pause so wiring the
    /// pinger up can take its time without a guaranteed first `down`.
    #[serde(default)]
    pub paused: bool,
}

#[derive(Debug, Deserialize)]
pub struct UpdateHeartbeatRequest {
    pub name: Option<String>,
    /// Absent = leave as-is; explicit `null` = clear.
    #[serde(default, deserialize_with = "super::double_option")]
    pub description: Option<Option<String>>,
    pub period_secs: Option<i64>,
    pub grace_secs: Option<i64>,
    pub enabled: Option<bool>,
}

/// Returned once, at create and rotate — the slug is stored hashed and
/// can never be shown again.
#[derive(Debug, Serialize)]
pub struct HeartbeatSlugDto {
    pub slug: String,
    /// Server-relative ping URL; clients prepend their base URL.
    pub ping_path: String,
}

#[derive(Debug, Serialize)]
pub struct CreateHeartbeatResponse {
    #[serde(flatten)]
    pub check: HeartbeatCheckDto,
    pub slug: String,
    pub ping_path: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct PauseHeartbeatRequest {
    /// Absolute unix-epoch end. Mutually exclusive with `duration_secs`;
    /// neither present = indefinite (operator-only power).
    #[serde(default)]
    pub until: Option<i64>,
    #[serde(default)]
    pub duration_secs: Option<i64>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HeartbeatPingDto {
    pub received_at: i64,
    pub kind: PingKind,
    pub exit_code: Option<i32>,
    pub source_ip: Option<String>,
    pub user_agent: Option<String>,
    pub body: Option<String>,
}

impl From<HeartbeatPing> for HeartbeatPingDto {
    fn from(p: HeartbeatPing) -> Self {
        Self {
            received_at: p.received_at,
            kind: p.kind,
            exit_code: p.exit_code,
            source_ip: p.source_ip,
            user_agent: p.user_agent,
            body: p.body,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListHeartbeatPingsResponse {
    pub pings: Vec<HeartbeatPingDto>,
}

fn default_true() -> bool {
    true
}
fn default_grace() -> i64 {
    300
}
