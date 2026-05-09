//! Alert engine v2 REST endpoints.
//!
//! Surface:
//! - `GET    /alerts`              — list rules
//! - `GET    /alerts/{id}`         — single rule
//! - `POST   /alerts`              — create rule
//! - `PUT    /alerts/{id}`         — update rule (full body)
//! - `DELETE /alerts/{id}`         — delete rule
//! - `GET    /alerts/state`        — currently pending or firing
//! - `GET    /alerts/events`       — recent transitions, newest first
//! - `GET    /alerts/{id}/events`  — recent transitions for one rule

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::routes::dtos::alerts::{
    AlertEventDto, AlertRuleDto, AlertStateDto, CreateAlertRuleRequest, ListAlertEventsResponse,
    ListAlertRulesResponse, ListAlertStateResponse, UpdateAlertRuleRequest, state_dto_from,
};
use crate::routes::extractors::Claims;
use crate::services::alerting::expression;
use crate::state::AppState;
use crate::storage::repositories::{AlertRepository, UpsertAlertRule};

const DEFAULT_EVENT_LIMIT: u32 = 100;
const MAX_EVENT_LIMIT: u32 = 1000;

const MIN_EVAL_INTERVAL: i64 = 3;
const MAX_EVAL_INTERVAL: i64 = 3600;
const MAX_FOR_DURATION: i64 = 86_400; // 24h
const MAX_COOLDOWN: i64 = 86_400;

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    pub limit: Option<u32>,
}

/// Common validation for create/update bodies. Bracket every numeric
/// knob and parse-validate the expression so a bad rule never persists.
fn validate(req_expression: &str, for_secs: i64, eval_secs: i64, cooldown_secs: i64) -> AppResult<()> {
    if let Err(e) = expression::parse(req_expression) {
        return Err(AppError::BadRequest(format!("expression: {}", e)));
    }
    if !(MIN_EVAL_INTERVAL..=MAX_EVAL_INTERVAL).contains(&eval_secs) {
        return Err(AppError::BadRequest(format!(
            "eval_interval_secs {} out of range [{}..{}]",
            eval_secs, MIN_EVAL_INTERVAL, MAX_EVAL_INTERVAL
        )));
    }
    if !(0..=MAX_FOR_DURATION).contains(&for_secs) {
        return Err(AppError::BadRequest(format!(
            "for_duration_secs {} out of range [0..{}]",
            for_secs, MAX_FOR_DURATION
        )));
    }
    if !(0..=MAX_COOLDOWN).contains(&cooldown_secs) {
        return Err(AppError::BadRequest(format!(
            "cooldown_secs {} out of range [0..{}]",
            cooldown_secs, MAX_COOLDOWN
        )));
    }
    Ok(())
}

// ===== Rule CRUD =====

pub async fn list_alerts(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ListAlertRulesResponse>> {
    let repo = AlertRepository::new(state.db.clone());
    let rules = repo.list().await?;
    Ok(Json(ListAlertRulesResponse {
        rules: rules.into_iter().map(AlertRuleDto::from).collect(),
    }))
}

pub async fn get_alert(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<Json<AlertRuleDto>> {
    let repo = AlertRepository::new(state.db.clone());
    let rule = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Alert rule {}", id)))?;
    Ok(Json(rule.into()))
}

pub async fn create_alert(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateAlertRuleRequest>,
) -> AppResult<(StatusCode, Json<AlertRuleDto>)> {
    validate(
        &req.expression,
        req.for_duration_secs,
        req.eval_interval_secs,
        req.cooldown_secs,
    )?;

    let repo = AlertRepository::new(state.db.clone());
    let upsert = UpsertAlertRule {
        name: req.name,
        description: req.description,
        enabled: req.enabled,
        expression: req.expression,
        severity: req.severity,
        for_duration_secs: req.for_duration_secs,
        eval_interval_secs: req.eval_interval_secs,
        cooldown_secs: req.cooldown_secs,
    };
    let id = repo.insert(&upsert).await?;
    let stored = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::Internal("created rule not readable".into()))?;
    Ok((StatusCode::CREATED, Json(stored.into())))
}

pub async fn update_alert(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateAlertRuleRequest>,
) -> AppResult<Json<AlertRuleDto>> {
    let repo = AlertRepository::new(state.db.clone());
    let mut current = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Alert rule {}", id)))?;

    // Selective overlay — only fields set in the request body are
    // applied. `description: Some(None)` explicitly clears it.
    if let Some(v) = req.name {
        current.name = v;
    }
    if let Some(v) = req.description {
        current.description = v;
    }
    if let Some(v) = req.enabled {
        current.enabled = v;
    }
    if let Some(v) = req.expression {
        current.expression = v;
    }
    if let Some(v) = req.severity {
        current.severity = v;
    }
    if let Some(v) = req.for_duration_secs {
        current.for_duration_secs = v;
    }
    if let Some(v) = req.eval_interval_secs {
        current.eval_interval_secs = v;
    }
    if let Some(v) = req.cooldown_secs {
        current.cooldown_secs = v;
    }

    validate(
        &current.expression,
        current.for_duration_secs,
        current.eval_interval_secs,
        current.cooldown_secs,
    )?;

    let merged = UpsertAlertRule {
        name: current.name.clone(),
        description: current.description.clone(),
        enabled: current.enabled,
        expression: current.expression.clone(),
        severity: current.severity,
        for_duration_secs: current.for_duration_secs,
        eval_interval_secs: current.eval_interval_secs,
        cooldown_secs: current.cooldown_secs,
    };
    let updated = repo.update(id, &merged).await?;
    if !updated {
        return Err(AppError::NotFound(format!("Alert rule {}", id)));
    }
    let stored = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::Internal("updated rule not readable".into()))?;
    Ok(Json(stored.into()))
}

pub async fn delete_alert(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let repo = AlertRepository::new(state.db.clone());
    let removed = repo.delete(id).await?;
    if !removed {
        return Err(AppError::NotFound(format!("Alert rule {}", id)));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ===== Active state =====

pub async fn list_active_state(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ListAlertStateResponse>> {
    let repo = AlertRepository::new(state.db.clone());
    // Joined with alert_rules in SQL — one round-trip, no second query
    // to materialise every rule just to look up two columns.
    let rows = repo.list_active_state().await?;

    let dtos: Vec<AlertStateDto> = rows
        .into_iter()
        .map(|(s, name, severity)| state_dto_from(s, name, severity))
        .collect();

    Ok(Json(ListAlertStateResponse { states: dtos }))
}

// ===== Event log =====

pub async fn list_recent_events(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Query(q): Query<EventsQuery>,
) -> AppResult<Json<ListAlertEventsResponse>> {
    let limit = q.limit.unwrap_or(DEFAULT_EVENT_LIMIT).min(MAX_EVENT_LIMIT);
    let repo = AlertRepository::new(state.db.clone());
    let events = repo.recent_events(limit).await?;
    Ok(Json(ListAlertEventsResponse {
        events: events.into_iter().map(AlertEventDto::from).collect(),
    }))
}

pub async fn list_events_for_rule(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<EventsQuery>,
) -> AppResult<Json<ListAlertEventsResponse>> {
    let limit = q.limit.unwrap_or(DEFAULT_EVENT_LIMIT).min(MAX_EVENT_LIMIT);
    let repo = AlertRepository::new(state.db.clone());
    let events = repo.events_for_rule(id, limit).await?;
    Ok(Json(ListAlertEventsResponse {
        events: events.into_iter().map(AlertEventDto::from).collect(),
    }))
}
