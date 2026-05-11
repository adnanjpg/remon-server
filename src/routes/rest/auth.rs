//! Authentication endpoints

use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
};
use log::info;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::auth::service::AuthService;
use crate::error::{AppError, AppResult};
use crate::routes::dtos::auth::{DeviceLoginRequest, RefreshRequest, TokenResponse};
use crate::routes::extractors::Claims;
use crate::state::AppState;
use crate::storage::repositories::DeviceRepository;

/// POST /auth/login — login with device credentials (device_id + device_token).
pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<DeviceLoginRequest>,
) -> AppResult<Json<TokenResponse>> {
    let device_repo = DeviceRepository::new(state.db.clone());
    let device = device_repo
        .get_by_id(&req.device_id)
        .await?
        .ok_or(AppError::DeviceNotFound)?;

    if !device.is_active {
        return Err(AppError::DeviceInactive);
    }

    if !AuthService::verify_token(&req.device_token, &device.token_hash) {
        return Err(AppError::InvalidToken);
    }

    let client_ip = extract_client_ip(&headers, &addr, state.trusted_proxy);
    let _ = device_repo
        .update_last_seen(&req.device_id, Some(&client_ip))
        .await;

    let auth_service = AuthService::new(state.auth_config.clone());
    let tokens = auth_service.create_tokens(&req.device_id)?;

    persist_session_pair(&device_repo, &req.device_id, &tokens).await?;

    info!("Device {} logged in from {}", req.device_id, client_ip);

    Ok(Json(TokenResponse {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_in: tokens.expires_in,
    }))
}

/// POST /auth/refresh — exchange a refresh token for a new pair, rotating it.
///
/// Both the old refresh-token jti and any access-token jti's still alive for
/// this device are invalidated; the client must use the new pair from now on.
pub async fn refresh(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RefreshRequest>,
) -> AppResult<Json<TokenResponse>> {
    let auth_service = AuthService::new(state.auth_config.clone());
    let claims = auth_service
        .validate_refresh_token(&req.refresh_token)
        .map_err(|_| AppError::InvalidToken)?;

    let device_repo = DeviceRepository::new(state.db.clone());

    // Revocation check: the refresh jti must still be active.
    if !device_repo.session_exists(&claims.jti).await? {
        return Err(AppError::InvalidToken);
    }

    let device = device_repo
        .get_by_id(&claims.sub)
        .await?
        .ok_or(AppError::DeviceNotFound)?;
    if !device.is_active {
        return Err(AppError::DeviceInactive);
    }

    // Rotation: wipe ALL sessions for this device, then issue and persist a
    // fresh pair. This is intentionally aggressive — if a stolen refresh
    // token has been used in parallel with the legitimate one, both sides
    // get logged out and the device must re-authenticate.
    device_repo.delete_device_sessions(&claims.sub).await?;

    let tokens = auth_service.create_tokens(&claims.sub)?;
    persist_session_pair(&device_repo, &claims.sub, &tokens).await?;

    Ok(Json(TokenResponse {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_in: tokens.expires_in,
    }))
}

/// POST /auth/logout — revoke the current access token's session.
///
/// Only this access token's jti is invalidated; the refresh token (if still
/// in the client's possession) keeps working until used or expired. Use
/// `delete_device_sessions` here if a "log out everywhere" flow is added.
pub async fn logout(State(state): State<Arc<AppState>>, claims: Claims) -> AppResult<StatusCode> {
    let device_repo = DeviceRepository::new(state.db.clone());
    device_repo.delete_session(&claims.jti).await?;
    info!(
        "Device {} logged out (jti={})",
        claims.device_id, claims.jti
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Resolve the client IP to record in audit. When `trust_proxy` is false we
/// only trust the TCP peer — XFF can be forged by any direct caller. When
/// true (deployer has opted in), prefer the leftmost XFF entry, which is the
/// real client under any proxy that strips incoming XFF before adding its
/// own. Falls back to the peer if XFF is absent or malformed.
fn extract_client_ip(headers: &HeaderMap, addr: &SocketAddr, trust_proxy: bool) -> String {
    if trust_proxy {
        if let Some(v) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return v.to_string();
        }
    }
    addr.ip().to_string()
}

/// Persist both jti's from a freshly issued token pair into the `sessions`
/// table. If either insert fails, the partial state is rolled back so no
/// half-issued credentials linger.
async fn persist_session_pair(
    repo: &DeviceRepository,
    device_id: &str,
    tokens: &crate::auth::service::CreatedTokens,
) -> AppResult<()> {
    repo.create_session(&tokens.access_jti, device_id, tokens.access_expires_at)
        .await?;
    if let Err(e) = repo
        .create_session(&tokens.refresh_jti, device_id, tokens.refresh_expires_at)
        .await
    {
        // Roll back the access-jti row so we don't leave it dangling.
        let _ = repo.delete_session(&tokens.access_jti).await;
        return Err(e);
    }
    Ok(())
}
