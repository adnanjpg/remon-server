//! Web Push registration.
//!
//! - `POST /me/push-subscription` stores the calling browser's subscription
//!   on its device row: the relay endpoint, the encryption keys, and the
//!   browser's own VAPID key, which this server signs its pushes with. A
//!   browser holds one subscription and brings the same key to every server,
//!   so it can take alerts from all of them.
//!
//! - `DELETE /me/push-subscription` clears it, leaving the pairing intact.

use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::routes::extractors::Claims;
use crate::state::AppState;
use crate::storage::repositories::{DeviceRepository, WebPushSubscription};

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
    /// The browser's own VAPID private key (PKCS#8 PEM) the subscription was
    /// made with.
    pub vapid_private_key: String,
    /// Opaque handle echoed back in each payload as `ref`.
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
    /// `warn` or `crit`. Omitted: both.
    #[serde(default)]
    pub min_severity: Option<String>,
}

/// Longest `ref` accepted; it rides in every payload, which is capped at 4 KB.
const MAX_REF_LEN: usize = 128;

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

    crate::services::webpush::client_public_key(&req.vapid_private_key)
        .map_err(|_| AppError::BadRequest("vapid_private_key is not a P-256 PKCS#8 key".into()))?;
    if req
        .reference
        .as_ref()
        .is_some_and(|r| r.len() > MAX_REF_LEN)
    {
        return Err(AppError::BadRequest(format!(
            "ref is longer than {MAX_REF_LEN} bytes"
        )));
    }
    if req
        .min_severity
        .as_deref()
        .is_some_and(|s| s != "warn" && s != "crit")
    {
        return Err(AppError::BadRequest(
            "min_severity must be 'warn' or 'crit'".to_string(),
        ));
    }

    let repo = DeviceRepository::new(state.db.clone());
    repo.set_web_push_subscription(
        &claims.device_id,
        Some(&WebPushSubscription {
            endpoint: req.endpoint,
            p256dh: req.p256dh,
            auth: req.auth,
            vapid_key: req.vapid_private_key,
            reference: req.reference,
            min_severity: req.min_severity,
        }),
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
    repo.set_web_push_subscription(&claims.device_id, None)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
