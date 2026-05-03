//! Endpoints scoped to the *calling device* (the one whose access token is
//! in the Authorization header).
//!
//! Right now only the FCM-token write lives here. As more "self" actions
//! show up (rename device, list own sessions, etc.) they'll join.

use axum::{
    Json,
    extract::State,
    http::StatusCode,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::error::AppResult;
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
