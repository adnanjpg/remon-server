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

/// Everything but the bundles — enough to pick one out of a list.
#[derive(Debug, Serialize)]
pub struct IncidentSummaryDto {
    pub id: i64,
    pub created_at: i64,
    pub trigger_kind: String,
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_set: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The follow-up landed, so `GET /incidents/{id}` carries `after_bundle`.
    pub has_after: bool,
}

#[derive(Debug, Serialize)]
pub struct ListIncidentsResponse {
    pub count: usize,
    pub incidents: Vec<IncidentSummaryDto>,
}

#[derive(Debug, Serialize)]
pub struct IncidentDto {
    pub id: i64,
    pub created_at: i64,
    pub trigger_kind: String,
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_set: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Context frozen at trigger time. Shape is owned by `services::incidents`,
    /// forwarded here rather than re-modelled so the two cannot drift.
    pub bundle: serde_json::Value,
    /// The same shape ~60 s later. Absent when the follow-up never landed —
    /// the daemon restarted, or the capture is younger than a minute.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_bundle: Option<serde_json::Value>,
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
            created_at: r.created_at,
            trigger_kind: r.trigger_kind,
            category: r.category,
            rule_name: r.rule_name,
            label_set: r.label_set,
            metric_value: r.metric_value,
            reason: r.reason,
            has_after: r.has_after,
        })
        .collect();

    Ok(Json(ListIncidentsResponse {
        count: incidents.len(),
        incidents,
    }))
}

/// GET /incidents/{id} — the frozen bundle, and the follow-up if it landed.
pub async fn get(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<Json<IncidentDto>> {
    let row = IncidentRepository::new(state.db.clone())
        .get(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Incident {}", id)))?;

    let after_bundle = row
        .after_bundle
        .as_deref()
        .map(|raw| parse_bundle(raw, id, "after_bundle"))
        .transpose()?;

    Ok(Json(IncidentDto {
        id: row.id,
        created_at: row.created_at,
        trigger_kind: row.trigger_kind,
        category: row.category,
        rule_name: row.rule_name,
        label_set: row.label_set,
        metric_value: row.metric_value,
        reason: row.reason,
        bundle: parse_bundle(&row.bundle, id, "bundle")?,
        after_bundle,
    }))
}
