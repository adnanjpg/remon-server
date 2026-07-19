//! Endpoints scoped to the *calling device* (the one whose access token is
//! in the Authorization header). Also covers cross-device ops the calling
//! device can perform on its peers (rename / revoke).

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

use crate::error::AppResult;
use crate::routes::dtos::sessions::{ListSessionsResponse, RenameSessionRequest, SessionInfo};
use crate::routes::extractors::Claims;
use crate::state::AppState;
use crate::storage::repositories::DeviceRepository;

#[derive(Debug, Deserialize)]
pub struct UpdateFcmTokenRequest {
    /// New FCM token. `null` (or `""`) clears the registration so this
    /// device stops receiving push notifications.
    pub fcm_token: Option<String>,
}

/// PATCH /me/fcm-token — update (or clear) the FCM push target for the
/// calling device. Returns 204 on success.
pub async fn update_fcm_token(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpdateFcmTokenRequest>,
) -> AppResult<StatusCode> {
    let repo = DeviceRepository::new(state.db.clone());
    let normalized = req.fcm_token.as_deref().filter(|s| !s.is_empty());
    repo.set_fcm_token(&claims.device_id, normalized).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /me/sessions — list every device paired to this server, with the
/// caller's own device marked. Used by the settings page's session
/// management card so operators can see (and clean up) browsers, mobile
/// apps, etc. that share access to this server.
pub async fn list_sessions(
    claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ListSessionsResponse>> {
    let repo = DeviceRepository::new(state.db.clone());
    let devices = repo.get_all().await?;
    let session_counts: HashMap<String, i64> = repo
        .count_active_sessions_per_device()
        .await?
        .into_iter()
        .collect();

    let sessions: Vec<SessionInfo> = devices
        .into_iter()
        .map(|d| {
            let id = d.id.clone();
            let is_current = id == claims.device_id;
            let active = session_counts.get(&id).copied().unwrap_or(0);
            SessionInfo {
                device: d.into(),
                is_current,
                active_sessions: active,
            }
        })
        .collect();

    Ok(Json(ListSessionsResponse { sessions }))
}

/// PATCH /me/sessions/{id} — rename a paired device. Any authenticated
/// caller can rename any device on this server (the trust boundary is
/// the server-wide credential set, not per-device). 204 on success.
pub async fn rename_session(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<RenameSessionRequest>,
) -> AppResult<StatusCode> {
    let trimmed = req.name.trim();
    if trimmed.is_empty() {
        return Err(crate::error::AppError::BadRequest(
            "name must not be empty".to_string(),
        ));
    }
    let repo = DeviceRepository::new(state.db.clone());
    repo.update_name(&id, trimmed).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /me/sessions/{id} — revoke a paired device. Cascades to the
/// `sessions` table via FK so all live JWTs for that device stop
/// authenticating immediately. Calling this with the caller's own
/// `device_id` self-revokes — the next refresh will fail and the
/// browser bounces back to /unlock.
pub async fn revoke_session(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let repo = DeviceRepository::new(state.db.clone());
    // Name read before the delete — afterwards there is nothing to name
    // the audit row with.
    let revoked_name = repo.get_by_id(&id).await.ok().flatten().map(|d| d.name);
    repo.delete(&id).await?;
    // The cascade happens inside SQLite, so there is no per-jti signal to
    // evict on — drop the auth cache wholesale.
    state.session_cache.clear();
    crate::services::events::record_operator(
        &state,
        &claims.device_id,
        "device_revoked",
        match &revoked_name {
            Some(n) => format!("Device '{}' revoked", n),
            None => format!("Device {} revoked", id),
        },
        Some("device"),
        Some(id),
        None,
    );
    Ok(StatusCode::NO_CONTENT)
}
