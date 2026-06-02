//! Web Push registration endpoints.
//!
//! - `GET /push/vapid-public-key` — returns the server's VAPID public
//!   key as a base64url-encoded uncompressed P-256 point. The browser
//!   feeds this directly into `pushManager.subscribe({ applicationServerKey })`.
//!
//! - `POST /me/push-subscription` — accepts the three-tuple the browser
//!   gets back from the relay (endpoint URL + p256dh + auth secret) and
//!   stores it on the calling device. Phase 3 reads these out when an
//!   alert fires.
//!
//! - `DELETE /me/push-subscription` — clears the triplet so the device
//!   stops receiving notifications without losing the rest of its
//!   pairing.

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::routes::extractors::Claims;
use crate::state::AppState;
use crate::storage::repositories::DeviceRepository;

#[derive(Debug, Serialize)]
pub struct VapidPublicKeyResponse {
    /// Base64url-encoded uncompressed P-256 point (65 bytes raw,
    /// leading 0x04). This is what `applicationServerKey` expects.
    pub public_key: String,
}

#[derive(Debug, Deserialize)]
pub struct SubscribePushRequest {
    /// The push relay endpoint URL the browser was assigned. Vendor-
    /// specific (Mozilla, Google, Apple). We POST encrypted payloads
    /// here when sending notifications.
    pub endpoint: String,
    /// ECDH P-256 public key for this subscription, base64url-encoded.
    /// Used to derive the per-message encryption key.
    pub p256dh: String,
    /// Per-subscription HMAC auth secret, base64url-encoded.
    pub auth: String,
}

/// GET /push/vapid-public-key — public-key half of the server's VAPID
/// identity, in the format the browser's subscribe call expects.
pub async fn vapid_public_key(
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<VapidPublicKeyResponse>> {
    let public_key = state
        .vapid_keys
        .public_key_for_client()
        .map_err(|e| AppError::Internal(format!("derive VAPID public key: {}", e)))?;
    Ok(Json(VapidPublicKeyResponse { public_key }))
}

/// POST /me/push-subscription — register the calling device's browser
/// subscription. Idempotent: re-posting overwrites the previous triplet.
pub async fn subscribe_push(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(req): Json<SubscribePushRequest>,
) -> AppResult<StatusCode> {
    if req.endpoint.is_empty() || req.p256dh.is_empty() || req.auth.is_empty() {
        return Err(AppError::BadRequest(
            "endpoint, p256dh, and auth are all required".to_string(),
        ));
    }
    // SSRF guard: reject endpoints that resolve to loopback / RFC1918 /
    // link-local (incl. cloud metadata) so a paired device can't turn the
    // web-push relay into an internal-network probe. Same policy the webhook
    // and ntfy channels enforce; the channel re-checks at send time too.
    crate::notify::url_policy::check_url(&req.endpoint, &state.notify.webhook_policy())
        .await
        .map_err(AppError::BadRequest)?;

    let repo = DeviceRepository::new(state.db.clone());
    repo.set_web_push_subscription(
        &claims.device_id,
        Some(&req.endpoint),
        Some(&req.p256dh),
        Some(&req.auth),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /me/push-subscription — unsubscribe the calling device.
/// Leaves the rest of the pairing intact.
pub async fn unsubscribe_push(
    claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<StatusCode> {
    let repo = DeviceRepository::new(state.db.clone());
    repo.set_web_push_subscription(&claims.device_id, None, None, None)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
