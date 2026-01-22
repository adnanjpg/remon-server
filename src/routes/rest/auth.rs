//! Authentication endpoints

use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::StatusCode,
};
use log::info;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use crate::auth::service::AuthService;
use crate::persistence::devices;
use crate::routes::dtos::{auth::GetOtpQrRequest, common::ResponseBody};
use crate::state::AppState;

// ==================== New Token-Based Auth ====================

/// Login request with device credentials
#[derive(Debug, Deserialize)]
pub struct DeviceLoginRequest {
    pub device_id: String,
    pub device_token: String,
}

/// Refresh token request
#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// Login response with JWT tokens
#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

/// Error response
#[derive(Debug, Serialize)]
pub struct AuthErrorResponse {
    pub error: String,
}

/// Login with device credentials (device_id + device_token)
/// POST /auth/login
pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<DeviceLoginRequest>,
) -> Result<Json<TokenResponse>, (StatusCode, Json<AuthErrorResponse>)> {
    // Get device from database
    let device = devices::get_device_by_id(&state.db, &req.device_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AuthErrorResponse {
                    error: format!("Database error: {}", e),
                }),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(AuthErrorResponse {
                    error: "Device not found".to_string(),
                }),
            )
        })?;

    // Check if device is active
    if !device.is_active {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(AuthErrorResponse {
                error: "Device is deactivated".to_string(),
            }),
        ));
    }

    // Verify device token
    if !AuthService::verify_token(&req.device_token, &device.token_hash) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(AuthErrorResponse {
                error: "Invalid device token".to_string(),
            }),
        ));
    }

    // Update last seen
    let client_ip = addr.ip().to_string();
    let _ = devices::update_last_seen(&state.db, &req.device_id, Some(&client_ip)).await;

    // Create JWT tokens
    let auth_service = AuthService::new(state.auth_config.clone());
    let tokens = auth_service.create_tokens(&req.device_id).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AuthErrorResponse {
                error: format!("Failed to create tokens: {}", e),
            }),
        )
    })?;

    info!("Device {} logged in from {}", req.device_id, client_ip);

    Ok(Json(TokenResponse {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_in: tokens.expires_in,
    }))
}

/// Refresh access token
/// POST /auth/refresh
pub async fn refresh(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<TokenResponse>, (StatusCode, Json<AuthErrorResponse>)> {
    let auth_service = AuthService::new(state.auth_config.clone());

    // Validate refresh token
    let claims = auth_service
        .validate_refresh_token(&req.refresh_token)
        .map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                Json(AuthErrorResponse {
                    error: "Invalid or expired refresh token".to_string(),
                }),
            )
        })?;

    // Verify device still exists and is active
    let device = devices::get_device_by_id(&state.db, &claims.sub)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AuthErrorResponse {
                    error: format!("Database error: {}", e),
                }),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(AuthErrorResponse {
                    error: "Device not found".to_string(),
                }),
            )
        })?;

    if !device.is_active {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(AuthErrorResponse {
                error: "Device is deactivated".to_string(),
            }),
        ));
    }

    // Create new tokens
    let tokens = auth_service.create_tokens(&claims.sub).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AuthErrorResponse {
                error: format!("Failed to create tokens: {}", e),
            }),
        )
    })?;

    Ok(Json(TokenResponse {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_in: tokens.expires_in,
    }))
}

// ==================== Legacy OTP Auth (Deprecated) ====================

/// Generate OTP QR code (deprecated - use pairing flow instead)
/// POST /auth/otp/qr
pub async fn get_otp_qr(
    Json(payload): Json<GetOtpQrRequest>,
) -> Result<Json<ResponseBody>, (StatusCode, Json<ResponseBody>)> {
    let url = crate::auth::otp::generate_otp_qr_url(&payload.device_id);

    match crate::auth::otp::outputqr(&url) {
        Ok(qr) => {
            println!("{}\r\n{}", url, qr);
            Ok(Json(ResponseBody::Success(true)))
        }
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ResponseBody::Error(
                "Failed to generate QR code.".to_string(),
            )),
        )),
    }
}

/// Legacy OTP login (deprecated - use /auth/login with device token instead)
/// POST /auth/login/otp
pub async fn login_otp(
    Json(login_req): Json<crate::auth::token::LoginRequest>,
) -> Result<Json<ResponseBody>, (StatusCode, Json<ResponseBody>)> {
    if crate::auth::otp::check_totp_match_dev_id(&login_req.otp, &login_req.device_id) {
        let token = crate::auth::token::generate_token(&login_req.device_id)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ResponseBody::Error("Failed to generate token.".to_string())),
                )
            })?;

        Ok(Json(ResponseBody::Token(token)))
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(ResponseBody::Error("Invalid OTP code.".to_string())),
        ))
    }
}
