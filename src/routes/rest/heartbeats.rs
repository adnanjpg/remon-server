//! Heartbeat check management endpoints (JWT-protected).
//!
//! - `GET    /heartbeats`                 — list with derived state
//! - `POST   /heartbeats`                 — create; response carries the slug ONCE
//! - `GET    /heartbeats/{id}`            — detail
//! - `PUT    /heartbeats/{id}`            — update (selective overlay)
//! - `DELETE /heartbeats/{id}`            — delete (ping log cascades)
//! - `POST   /heartbeats/{id}/pause`      — operator pause (indefinite / until / duration)
//! - `DELETE /heartbeats/{id}/pause`      — resume
//! - `POST   /heartbeats/{id}/rotate-slug`— invalidate the old ping URL, mint a new one
//! - `GET    /heartbeats/{id}/pings`      — ping log, newest first
//!
//! The anonymous ping side lives in `ping.rs`; state derivation in
//! `models/heartbeat.rs`; alerting via the `heartbeat` resolver namespace
//! (`heartbeat.up < 1` — one unfiltered rule covers every check).

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::Utc;
use rand::Rng;
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::routes::dtos::heartbeats::{
    CreateHeartbeatRequest, CreateHeartbeatResponse, HeartbeatCheckDto, HeartbeatSlugDto,
    ListHeartbeatPingsResponse, ListHeartbeatsResponse, PauseHeartbeatRequest,
    UpdateHeartbeatRequest, check_dto_from,
};
use crate::routes::extractors::Claims;
use crate::state::AppState;
use crate::storage::repositories::{HeartbeatRepository, UpsertHeartbeatCheck};

const MIN_PERIOD_SECS: i64 = 10;
const MAX_PERIOD_SECS: i64 = 31_536_000; // 365d
const MAX_GRACE_SECS: i64 = 31_536_000;

/// Operator pauses with an explicit end share the alert-silence ceiling;
/// "longer than 30 days" is what the indefinite form is for.
const MAX_OPERATOR_PAUSE_SECS: i64 = 30 * 86_400;

const DEFAULT_PING_LIMIT: u32 = 100;
const MAX_PING_LIMIT: u32 = 1000;
const MAX_PING_OFFSET: u32 = 100_000;

#[derive(Debug, serde::Deserialize)]
pub struct PingsQuery {
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// Same shape as probe names: expression label values quote anything, but
/// a tight charset keeps names shell- and URL-friendly everywhere else.
fn validate_name(name: &str) -> AppResult<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if !ok {
        return Err(AppError::BadRequest(
            "name must match [a-z][a-z0-9_-]* and be at most 64 chars".to_string(),
        ));
    }
    Ok(())
}

fn validate_timing(period_secs: i64, grace_secs: i64) -> AppResult<()> {
    if !(MIN_PERIOD_SECS..=MAX_PERIOD_SECS).contains(&period_secs) {
        return Err(AppError::BadRequest(format!(
            "period_secs {} out of range [{}..{}]",
            period_secs, MIN_PERIOD_SECS, MAX_PERIOD_SECS
        )));
    }
    if !(0..=MAX_GRACE_SECS).contains(&grace_secs) {
        return Err(AppError::BadRequest(format!(
            "grace_secs {} out of range [0..{}]",
            grace_secs, MAX_GRACE_SECS
        )));
    }
    Ok(())
}

/// 128-bit random slug, lowercase hex. The stored side is blake3(slug);
/// the cleartext leaves the server exactly once, in the create/rotate
/// response.
fn generate_slug() -> (String, String) {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let slug = hex::encode(bytes);
    let hash = blake3::hash(slug.as_bytes()).to_hex().to_string();
    (slug, hash)
}

fn ping_path(slug: &str) -> String {
    format!("/ping/{}", slug)
}

/// A UNIQUE hit on `name` is caller error, not a 500.
fn map_unique_name(e: AppError) -> AppError {
    match &e {
        AppError::DatabaseError(msg) if msg.contains("heartbeat_checks.name") => {
            AppError::Conflict("a check with this name already exists".to_string())
        }
        _ => e,
    }
}

pub async fn list_heartbeats(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ListHeartbeatsResponse>> {
    let repo = HeartbeatRepository::new(state.db.clone());
    let now = Utc::now().timestamp();
    let checks = repo.list_all().await?;
    Ok(Json(ListHeartbeatsResponse {
        checks: checks.into_iter().map(|c| check_dto_from(c, now)).collect(),
    }))
}

pub async fn get_heartbeat(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<Json<HeartbeatCheckDto>> {
    let repo = HeartbeatRepository::new(state.db.clone());
    let check = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Heartbeat check {}", id)))?;
    Ok(Json(check_dto_from(check, Utc::now().timestamp())))
}

pub async fn create_heartbeat(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateHeartbeatRequest>,
) -> AppResult<(StatusCode, Json<CreateHeartbeatResponse>)> {
    validate_name(&req.name)?;
    validate_timing(req.period_secs, req.grace_secs)?;

    let repo = HeartbeatRepository::new(state.db.clone());
    let (slug, slug_hash) = generate_slug();
    let upsert = UpsertHeartbeatCheck {
        name: req.name,
        description: req.description,
        period_secs: req.period_secs,
        grace_secs: req.grace_secs,
        enabled: req.enabled,
    };
    // Born paused (indefinite operator pause) rides inside the INSERT, so
    // a check created ahead of its pinger can't slip through live.
    let id = repo
        .insert(&upsert, &slug_hash, req.paused)
        .await
        .map_err(map_unique_name)?;

    let stored = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::Internal("created check not readable".into()))?;
    Ok((
        StatusCode::CREATED,
        Json(CreateHeartbeatResponse {
            check: check_dto_from(stored, Utc::now().timestamp()),
            ping_path: ping_path(&slug),
            slug,
        }),
    ))
}

pub async fn update_heartbeat(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateHeartbeatRequest>,
) -> AppResult<Json<HeartbeatCheckDto>> {
    let repo = HeartbeatRepository::new(state.db.clone());
    let mut current = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Heartbeat check {}", id)))?;
    let was_enabled = current.enabled;

    if let Some(v) = req.name {
        current.name = v;
    }
    if let Some(v) = req.description {
        current.description = v;
    }
    if let Some(v) = req.period_secs {
        current.period_secs = v;
    }
    if let Some(v) = req.grace_secs {
        current.grace_secs = v;
    }
    if let Some(v) = req.enabled {
        current.enabled = v;
    }

    validate_name(&current.name)?;
    validate_timing(current.period_secs, current.grace_secs)?;

    // Re-enabling ends a quiet window the anchor formula can't see —
    // grant the fresh period+grace BEFORE flipping enabled, so a crash in
    // between leaves only a harmless anchor bump on a disabled check.
    if !was_enabled && current.enabled {
        repo.re_enable_grant(id, Utc::now().timestamp()).await?;
    }

    let upsert = UpsertHeartbeatCheck {
        name: current.name.clone(),
        description: current.description.clone(),
        period_secs: current.period_secs,
        grace_secs: current.grace_secs,
        enabled: current.enabled,
    };
    let updated = repo.update(id, &upsert).await.map_err(map_unique_name)?;
    if !updated {
        return Err(AppError::NotFound(format!("Heartbeat check {}", id)));
    }
    let stored = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::Internal("updated check not readable".into()))?;
    Ok(Json(check_dto_from(stored, Utc::now().timestamp())))
}

pub async fn delete_heartbeat(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let repo = HeartbeatRepository::new(state.db.clone());
    let removed = repo.delete(id).await?;
    if !removed {
        return Err(AppError::NotFound(format!("Heartbeat check {}", id)));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// POST /heartbeats/{id}/pause — operator pause. Empty body, `{}`, or
/// neither field = indefinite; overwrites any service-announced pause.
pub async fn pause_heartbeat(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    req: Option<Json<PauseHeartbeatRequest>>,
) -> AppResult<Json<HeartbeatCheckDto>> {
    let req = req.map(|Json(r)| r).unwrap_or_default();
    let now = Utc::now().timestamp();
    if req.until.is_some() && req.duration_secs.is_some() {
        return Err(AppError::BadRequest(
            "until and duration_secs are mutually exclusive".to_string(),
        ));
    }
    let until = match (req.until, req.duration_secs) {
        (Some(until), None) => {
            if until <= now || until > now + MAX_OPERATOR_PAUSE_SECS {
                return Err(AppError::BadRequest(format!(
                    "until must fall within the next {} seconds",
                    MAX_OPERATOR_PAUSE_SECS
                )));
            }
            Some(until)
        }
        (None, Some(secs)) => {
            if !(1..=MAX_OPERATOR_PAUSE_SECS).contains(&secs) {
                return Err(AppError::BadRequest(format!(
                    "duration_secs {} out of range [1..{}]",
                    secs, MAX_OPERATOR_PAUSE_SECS
                )));
            }
            Some(now + secs)
        }
        _ => None,
    };
    let reason = req
        .reason
        .as_deref()
        .map(|r| r.chars().take(256).collect::<String>());

    let repo = HeartbeatRepository::new(state.db.clone());
    let updated = repo
        .operator_pause(id, now, until, reason.as_deref())
        .await?;
    if !updated {
        return Err(AppError::NotFound(format!("Heartbeat check {}", id)));
    }
    let stored = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::Internal("paused check not readable".into()))?;
    Ok(Json(check_dto_from(stored, now)))
}

/// DELETE /heartbeats/{id}/pause — resume. Idempotent: 204 whether or not
/// a pause was active. Resume re-anchors the deadline, so the check gets
/// one full period+grace before it can go down.
pub async fn resume_heartbeat(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let repo = HeartbeatRepository::new(state.db.clone());
    if repo.get(id).await?.is_none() {
        return Err(AppError::NotFound(format!("Heartbeat check {}", id)));
    }
    let _ = repo.operator_resume(id, Utc::now().timestamp()).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /heartbeats/{id}/rotate-slug — mint a new capability URL. The old
/// slug 404s from this response on; the new one is shown exactly once.
pub async fn rotate_heartbeat_slug(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<Json<HeartbeatSlugDto>> {
    let repo = HeartbeatRepository::new(state.db.clone());
    let (slug, slug_hash) = generate_slug();
    let updated = repo.rotate_slug(id, &slug_hash).await?;
    if !updated {
        return Err(AppError::NotFound(format!("Heartbeat check {}", id)));
    }
    Ok(Json(HeartbeatSlugDto {
        ping_path: ping_path(&slug),
        slug,
    }))
}

pub async fn list_heartbeat_pings(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<PingsQuery>,
) -> AppResult<Json<ListHeartbeatPingsResponse>> {
    let limit = q.limit.unwrap_or(DEFAULT_PING_LIMIT).min(MAX_PING_LIMIT);
    let offset = q.offset.unwrap_or(0).min(MAX_PING_OFFSET);
    let repo = HeartbeatRepository::new(state.db.clone());
    if repo.get(id).await?.is_none() {
        return Err(AppError::NotFound(format!("Heartbeat check {}", id)));
    }
    let pings = repo.list_pings(id, limit, offset).await?;
    Ok(Json(ListHeartbeatPingsResponse {
        pings: pings.into_iter().map(Into::into).collect(),
    }))
}
