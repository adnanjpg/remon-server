//! REST DTOs for the unified host-event timeline (`GET /events`).
//!
//! One normalized shape regardless of which table a row came from
//! (`host_events`, `alert_events`, `incident_snapshots`), so chart
//! annotation and timeline clients render a single stream. `events` are
//! sorted by `ts` descending.

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct ListEventsQuery {
    pub start: Option<i64>,
    pub end: Option<i64>,
    /// CSV of event kinds to include (open vocabulary — e.g.
    /// `boot,oom_kill,alert_fired`). Absent = all kinds.
    pub kinds: Option<String>,
    /// CSV of sources to include: `system`, `operator`, `agent`.
    /// Absent = all sources.
    pub sources: Option<String>,
    /// Hard cap on events returned. Defaults to 500, max 1000.
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct EventActorDto {
    pub device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// What the event points at, when it points at a concrete object —
/// `("alert_rule", "3")`, `("incident", "17")`, `("service", "nginx")`,
/// `("process", "chrome")`, `("disk", "/dev/sda")`.
#[derive(Debug, Serialize)]
pub struct EventRefDto {
    #[serde(rename = "type")]
    pub ref_type: String,
    pub id: String,
}

#[derive(Debug, Serialize)]
pub struct EventDto {
    pub ts: i64,
    /// system | operator | agent
    pub source: String,
    pub kind: String,
    /// info | warn | error
    pub severity: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<EventActorDto>,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<EventRefDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct ListEventsResponse {
    pub start: i64,
    pub end: i64,
    pub count: usize,
    pub events: Vec<EventDto>,
}
