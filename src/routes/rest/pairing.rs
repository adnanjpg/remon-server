//! Device pairing endpoints

use axum::{Json, extract::State};
use colored::Colorize;
use log::{info, warn};
use std::sync::Arc;

use subtle::ConstantTimeEq;

use crate::auth::service::AuthService;
use crate::error::{AppError, AppResult};
use crate::models::auth::StoredDevice;
use crate::routes::dtos::auth::{
    PairCompleteRequest, PairCompleteResponse, PairingInitiateResponse,
};
use crate::state::{AppState, PAIRING_MAX_ATTEMPTS, PairingState};
use crate::storage::repositories::DeviceRepository;

/// POST /auth/pair/initiate — generate a pairing code (printed to terminal only).
///
/// Refuses with 409 if an unexpired pairing window is already active. This
/// prevents an anonymous attacker from wiping a legitimate user's code by
/// repeatedly hitting this endpoint.
pub async fn initiate_pairing(
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<PairingInitiateResponse>> {
    let now = chrono::Utc::now().timestamp();
    let code = AuthService::generate_pairing_code();
    let expires_at = now + state.auth_config.pairing_code_ttl_secs as i64;

    {
        let mut pairing = state.pairing_state.write().await;
        if let Some(existing) = pairing.as_ref() {
            if existing.expires_at > now {
                return Err(AppError::AlreadyExists);
            }
        }
        *pairing = Some(PairingState {
            code: code.clone(),
            expires_at,
            attempts_remaining: PAIRING_MAX_ATTEMPTS,
        });
    }

    let ttl = state.auth_config.pairing_code_ttl_secs;

    // Audit trail: log that a window opened, but never the code itself —
    // logs may be shipped to centralized sinks that aren't trust-equivalent
    // to the host terminal.
    info!("Pairing window opened (ttl_secs={})", ttl);

    // Terminal-only display (stdout, not the log pipeline). Possession of the
    // code requires physical/SSH access to the host running the server.
    let ttl_human = if ttl % 60 == 0 {
        format!("{}m", ttl / 60)
    } else {
        format!("{}s", ttl)
    };
    println!();
    println!("  {}", "Device pairing".bold().cyan());
    println!("  {} {}", "code   ".dimmed(), code.bold().yellow());
    println!("  {} {}", "expires".dimmed(), ttl_human.dimmed());
    println!("  {}", "enter this code in your client to pair".dimmed());
    println!();

    // Security: do NOT return the code in the API response — it stays in
    // the server terminal so that a successful complete_pairing call requires
    // physical access to the host.
    Ok(Json(PairingInitiateResponse {
        message: "Pairing code displayed in server terminal".to_string(),
        expires_at,
    }))
}

/// POST /auth/pair/complete — exchange pairing code for device credentials.
///
/// Code validation runs under a single write lock so initiate/complete are
/// atomic. Wrong-code attempts decrement an attempts counter; once exhausted
/// (or on success / on TTL expiry) the pairing state is cleared.
pub async fn complete_pairing(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PairCompleteRequest>,
) -> AppResult<Json<PairCompleteResponse>> {
    let now = chrono::Utc::now().timestamp();

    let outcome = {
        let mut pairing = state.pairing_state.write().await;
        match pairing.as_mut() {
            None => PairingOutcome::NoActiveCode,
            Some(p) if p.expires_at <= now => {
                *pairing = None;
                PairingOutcome::Expired
            }
            Some(p) => {
                let matches: bool = p.code.as_bytes().ct_eq(req.pairing_code.as_bytes()).into();
                if matches {
                    *pairing = None;
                    PairingOutcome::Success
                } else {
                    p.attempts_remaining = p.attempts_remaining.saturating_sub(1);
                    if p.attempts_remaining == 0 {
                        *pairing = None;
                        PairingOutcome::AttemptsExhausted
                    } else {
                        PairingOutcome::WrongCode {
                            remaining: p.attempts_remaining,
                        }
                    }
                }
            }
        }
    };

    match outcome {
        PairingOutcome::Success => {}
        PairingOutcome::WrongCode { remaining } => {
            warn!(
                "Pairing complete failed: wrong code ({} attempts remaining)",
                remaining
            );
            return Err(AppError::PairingExpired);
        }
        PairingOutcome::AttemptsExhausted => {
            warn!("Pairing complete failed: attempts exhausted, code invalidated");
            return Err(AppError::PairingExpired);
        }
        PairingOutcome::NoActiveCode | PairingOutcome::Expired => {
            return Err(AppError::PairingExpired);
        }
    }

    let device_id = uuid::Uuid::new_v4().to_string();
    let device_token = AuthService::generate_device_token();
    let token_hash = AuthService::hash_token(&device_token)
        .map_err(|e| AppError::Internal(format!("Failed to hash token: {}", e)))?;

    let device = StoredDevice {
        id: device_id.clone(),
        name: req.device_name.clone(),
        token_hash,
        last_ip: None,
        last_seen: now,
        created_at: now,
        is_active: true,
    };

    let device_repo = DeviceRepository::new(state.db.clone());
    device_repo.create(&device).await?;

    // If the client supplied an FCM token in the pairing body, register it
    // immediately. Failures here don't poison the pairing — the device
    // still gets back valid credentials and can retry via
    // PATCH /me/fcm-token after login.
    if let Some(token) = req
        .fcm_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Err(e) = device_repo.set_fcm_token(&device_id, Some(token)).await {
            warn!(
                "Failed to register FCM token for newly paired device {}: {:?}",
                device_id, e
            );
        }
    }

    info!("New device paired: {} ({})", device_id, req.device_name);

    Ok(Json(PairCompleteResponse {
        device_id,
        device_token,
    }))
}

/// Internal outcome from the atomic pairing-state critical section.
/// Defined inside the handler so the lock is dropped before any work that
/// shouldn't hold it (DB writes, hashing, logging the bad-attempt warning).
enum PairingOutcome {
    Success,
    WrongCode { remaining: u8 },
    AttemptsExhausted,
    Expired,
    NoActiveCode,
}
