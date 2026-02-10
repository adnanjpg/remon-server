//! Device pairing endpoints

use axum::{Json, extract::State, http::StatusCode};
use log::info;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::auth::service::AuthService;
use crate::models::auth::StoredDevice;
use crate::state::{AppState, PairingState};
use crate::storage::repositories::DeviceRepository;

/// Pairing initiate response (code is shown in terminal only, NOT in API response)
#[derive(Debug, Clone, Serialize)]
pub struct PairingInitiateResponse {
    pub message: String,
    pub expires_at: i64,
}

/// Pairing completion request
#[derive(Debug, Deserialize)]
pub struct PairCompleteRequest {
    pub pairing_code: String,
    pub device_name: String,
}

/// Pairing completion response
#[derive(Debug, Serialize)]
pub struct PairCompleteResponse {
    pub device_id: String,
    pub device_token: String,
}

/// Error response body
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

/// Initiate device pairing (prints code to terminal)
/// POST /auth/pair/initiate
pub async fn initiate_pairing(
    State(state): State<Arc<AppState>>,
) -> Result<Json<PairingInitiateResponse>, (StatusCode, Json<ErrorResponse>)> {
    let code = AuthService::generate_pairing_code();
    let expires_at =
        chrono::Utc::now().timestamp() + state.auth_config.pairing_code_ttl_secs as i64;

    // Store pairing state
    {
        let mut pairing = state.pairing_state.write().await;
        *pairing = Some(PairingState {
            code: code.clone(),
            expires_at,
        });
    }

    // Print to terminal for user to see
    info!("=================================================");
    info!("  PAIRING CODE: {}                          ", code);
    info!(
        "  Expires in {} seconds                     ",
        state.auth_config.pairing_code_ttl_secs
    );
    info!("=================================================");

    // Also print QR-style for visibility
    println!("\n");
    println!("╔═══════════════════════════════════════════════╗");
    println!("║                                               ║");
    println!("║          DEVICE PAIRING CODE                  ║");
    println!("║                                               ║");
    println!("║              {}                          ║", code);
    println!("║                                               ║");
    println!("║     Enter this code in your mobile app        ║");
    println!(
        "║     Expires in {} seconds                    ║",
        state.auth_config.pairing_code_ttl_secs
    );
    println!("║                                               ║");
    println!("╚═══════════════════════════════════════════════╝");
    println!("\n");

    // Security: Do NOT return the code in API response
    // Code is only visible in terminal (physical access required)
    Ok(Json(PairingInitiateResponse {
        message: "Pairing code displayed in server terminal".to_string(),
        expires_at,
    }))
}

/// Complete device pairing
/// POST /auth/pair/complete
pub async fn complete_pairing(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PairCompleteRequest>,
) -> Result<Json<PairCompleteResponse>, (StatusCode, Json<ErrorResponse>)> {
    let now = chrono::Utc::now().timestamp();

    // Validate pairing code
    {
        let pairing = state.pairing_state.read().await;
        match pairing.as_ref() {
            Some(p) if p.code == req.pairing_code && p.expires_at > now => {}
            _ => {
                return Err((
                    StatusCode::GONE,
                    Json(ErrorResponse {
                        error: "Pairing code expired or invalid".to_string(),
                    }),
                ));
            }
        }
    }

    // Clear pairing state (one-time use)
    {
        let mut pairing = state.pairing_state.write().await;
        *pairing = None;
    }

    // Create device
    let device_id = uuid::Uuid::new_v4().to_string();
    let device_token = AuthService::generate_device_token();
    let token_hash = AuthService::hash_token(&device_token).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to hash token: {}", e),
            }),
        )
    })?;

    let device = StoredDevice {
        id: device_id.clone(),
        name: req.device_name.clone(),
        token_hash,
        totp_secret: None,
        last_ip: None,
        last_seen: now,
        created_at: now,
        is_active: true,
    };

    // Save device to database
    let device_repo = DeviceRepository::new(state.db.clone());
    device_repo
        .create(&device)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to create device: {}", e),
                }),
            )
        })?;

    info!("New device paired: {} ({})", device_id, req.device_name);

    Ok(Json(PairCompleteResponse {
        device_id,
        device_token,
    }))
}
