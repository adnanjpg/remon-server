//! Incident snapshots — the flight recorder's outside face.
//!
//! `capture` is the external-trigger side (`services::incidents`); alert
//! transitions capture on their own, and this lets an operator, a script, or
//! an external detector (fail2ban action, IDS hook) anchor one: "record the
//! box, now". `list` and `get` read them back — the timeline projects captures
//! as `incident_captured` events whose `ref` carries the id `get` takes.
//!
//! The split matters: rollup thins the metric series behind an incident within
//! hours, so the bundle frozen at trigger time is the only place that detail
//! survives. Listing never reads it.

use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::routes::extractors::{Claims, ValidatedQuery};
use crate::services::incidents;
use crate::state::AppState;
use crate::storage::repositories::IncidentRepository;

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;

#[derive(Debug, Deserialize)]
pub struct CaptureRequest {
    /// Why this moment is worth recording (stored, clamped server-side).
    pub reason: String,
    /// resource | availability | security | custom. Default custom.
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CaptureResponse {
    pub id: i64,
}

/// POST /incidents/capture — freeze the current host context.
pub async fn capture(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(req): Json<CaptureRequest>,
) -> AppResult<Json<CaptureResponse>> {
    let reason = req.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest("reason must not be empty".to_string()));
    }
    let category = req.category.as_deref().unwrap_or("custom");
    if !matches!(
        category,
        "resource" | "availability" | "security" | "custom"
    ) {
        return Err(AppError::BadRequest(
            "category must be resource|availability|security|custom".to_string(),
        ));
    }

    let id = incidents::capture_manual(&state, reason, category).await?;
    Ok(Json(CaptureResponse { id }))
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Hard cap on rows returned. Defaults to 50, max 200.
    pub limit: Option<u32>,
}

/// The episode envelope — enough to pick one out of a list, no frame payloads.
#[derive(Debug, Serialize)]
pub struct IncidentSummaryDto {
    pub id: i64,
    pub opened_at: i64,
    /// Absent while the episode is still live.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<i64>,
    /// `resolved` | `expired` | `daemon_restart`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
    pub trigger_kind: String,
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_set: Option<String>,
    /// The value that opened the episode, and the worst it reached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worst_value: Option<f64>,
    pub trigger_context: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Frames captured so far — one means only the opening moment is on record.
    pub frame_count: i64,
}

#[derive(Debug, Serialize)]
pub struct ListIncidentsResponse {
    pub count: usize,
    pub incidents: Vec<IncidentSummaryDto>,
}

/// One captured moment. The envelope is modelled here; `payload` is not — its
/// shape is owned by `services::incidents` and forwarded verbatim so the two
/// cannot drift apart.
#[derive(Debug, Serialize)]
pub struct IncidentFrameDto {
    pub seq: i64,
    /// `onset` | `escalation` | `peak` | `resolution` | `followup`.
    pub kind: String,
    pub captured_at: i64,
    pub payload: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct IncidentDto {
    pub id: i64,
    pub opened_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
    pub trigger_kind: String,
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_set: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worst_value: Option<f64>,
    pub trigger_context: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Oldest first, so reading top to bottom replays the episode.
    pub frames: Vec<IncidentFrameDto>,
}

/// The daemon writes these blobs itself, so unparseable means the row is
/// damaged, not that the caller asked for something wrong.
fn parse_bundle(raw: &str, id: i64, which: &str) -> AppResult<serde_json::Value> {
    serde_json::from_str(raw)
        .map_err(|e| AppError::Internal(format!("incident {} has a damaged {}: {}", id, which, e)))
}

/// GET /incidents — newest first, bundles omitted.
pub async fn list(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    ValidatedQuery(q): ValidatedQuery<ListQuery>,
) -> AppResult<Json<ListIncidentsResponse>> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let incidents: Vec<IncidentSummaryDto> = IncidentRepository::new(state.db.clone())
        .list(limit)
        .await?
        .into_iter()
        .map(|r| IncidentSummaryDto {
            id: r.id,
            opened_at: r.opened_at,
            closed_at: r.closed_at,
            close_reason: r.close_reason,
            trigger_kind: r.trigger_kind,
            category: r.category,
            rule_name: r.rule_name,
            label_set: r.label_set,
            trigger_value: r.trigger_value,
            worst_value: r.worst_value,
            trigger_context: r
                .trigger_context
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok()),
            reason: r.reason,
            frame_count: r.frame_count,
        })
        .collect();

    Ok(Json(ListIncidentsResponse {
        count: incidents.len(),
        incidents,
    }))
}

/// GET /incidents/{id} — the whole reel, oldest frame first.
pub async fn get(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<Json<IncidentDto>> {
    let row = IncidentRepository::new(state.db.clone())
        .get(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Incident {}", id)))?;

    let frames = row
        .frames
        .into_iter()
        .map(|f| {
            Ok(IncidentFrameDto {
                payload: parse_bundle(&f.payload, id, &format!("frame {}", f.seq))?,
                seq: f.seq,
                kind: f.kind,
                captured_at: f.captured_at,
            })
        })
        .collect::<AppResult<Vec<_>>>()?;

    Ok(Json(IncidentDto {
        id: row.id,
        opened_at: row.opened_at,
        closed_at: row.closed_at,
        close_reason: row.close_reason,
        trigger_kind: row.trigger_kind,
        category: row.category,
        rule_name: row.rule_name,
        label_set: row.label_set,
        trigger_value: row.trigger_value,
        worst_value: row.worst_value,
        trigger_context: row
            .trigger_context
            .as_deref()
            .map(|s| parse_bundle(s, id, "trigger context"))
            .transpose()?,
        reason: row.reason,
        frames,
    }))
}
