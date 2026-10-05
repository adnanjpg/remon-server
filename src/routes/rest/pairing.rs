//! Device pairing endpoints

use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::HeaderMap,
};
use log::{info, warn};
use std::net::SocketAddr;
use std::sync::Arc;

use subtle::ConstantTimeEq;

use crate::auth::pairing::{self, MAX_ATTEMPTS, MAX_ATTEMPTS_PER_IP};
use crate::auth::service::AuthService;
use crate::error::{AppError, AppResult};
use crate::models::auth::StoredDevice;
use crate::routes::dtos::auth::{
    PairCompleteRequest, PairCompleteResponse, PairingInitiateResponse, PairingOpenResponse,
};
use crate::routes::extractors::Claims;
use crate::routes::rest::auth::extract_client_ip;
use crate::state::AppState;
use crate::storage::repositories::DeviceRepository;

/// POST /auth/pair/initiate — opens nothing.
///
/// Anyone could call it, so it no longer opens a window: `remon-server pair`
/// on the host or `POST /auth/pair/open` from a paired device does. Kept so
/// older clients still move on to their code entry step.
pub async fn initiate_pairing(
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<PairingInitiateResponse>> {
    let expires_at =
        chrono::Utc::now().timestamp() + state.auth_config.pairing_code_ttl_secs as i64;
    Ok(Json(PairingInitiateResponse {
        message: "Run `remon-server pair` on the server, or open pairing from a paired device"
            .to_string(),
        expires_at,
    }))
}

/// POST /auth/pair/open — a paired device opens a window for another one.
///
/// The code goes back to the caller, who is already trusted; any open window
/// is replaced.
pub async fn open_pairing(
    claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<PairingOpenResponse>> {
    let window = pairing::open(&state.db, state.auth_config.pairing_code_ttl_secs).await?;
    info!("pairing window opened by device {}", claims.device_id);
    crate::services::events::record_operator(
        &state,
        &claims.device_id,
        "pairing_opened",
        "Pairing window opened".to_string(),
        None,
        None,
        None,
    );
    Ok(Json(PairingOpenResponse {
        pairing_code: window.code,
        expires_at: window.expires_at,
    }))
}

/// POST /auth/pair/complete — exchange pairing code for device credentials.
///
/// Each address gets `MAX_ATTEMPTS_PER_IP` wrong codes, so one client cannot
/// burn a window someone else is using; the window closes after
/// `MAX_ATTEMPTS` in total, on success, or at expiry.
pub async fn complete_pairing(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<PairCompleteRequest>,
) -> AppResult<Json<PairCompleteResponse>> {
    let now = chrono::Utc::now().timestamp();
    let client_ip = extract_client_ip(&headers, &addr, state.proxy_hops);

    {
        let mut attempts = state.pairing_attempts.lock().await;
        let Some(window) = pairing::current(&state.db).await? else {
            return Err(AppError::PairingExpired);
        };
        attempts.track(&window);

        let used = attempts.per_ip.get(&client_ip).copied().unwrap_or(0);
        if used >= MAX_ATTEMPTS_PER_IP {
            warn!("pairing complete refused: {client_ip} has used its attempts");
            return Err(AppError::PairingExpired);
        }

        let matches: bool = window
            .code
            .as_bytes()
            .ct_eq(req.pairing_code.as_bytes())
            .into();
        if !matches {
            attempts.per_ip.insert(client_ip.clone(), used + 1);
            attempts.total += 1;
            if attempts.total >= MAX_ATTEMPTS {
                pairing::close(&state.db).await?;
                warn!("pairing complete failed: window took {MAX_ATTEMPTS} wrong codes, closed");
            } else {
                warn!(
                    "pairing complete failed: wrong code from {client_ip} ({} left for it)",
                    MAX_ATTEMPTS_PER_IP - used - 1
                );
            }
            return Err(AppError::PairingExpired);
        }

        pairing::close(&state.db).await?;
        *attempts = Default::default();
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
        && let Err(e) = device_repo.set_fcm_token(&device_id, Some(token)).await
    {
        warn!(
            "failed to register FCM token for newly paired device {}: {:?}",
            device_id, e
        );
    }

    info!("new device paired: {} ({})", device_id, req.device_name);

    // Security-relevant enough for the ledger: a new credential now exists.
    // Actor fields are filled directly — the device was created two lines up.
    crate::services::events::record(
        &state,
        crate::storage::repositories::NewHostEvent {
            source: "operator",
            kind: "device_paired",
            severity: "info",
            message: format!("New device '{}' paired", req.device_name),
            actor_device_id: Some(device_id.clone()),
            actor_name: Some(req.device_name.clone()),
            ref_type: Some("device"),
            ref_id: Some(device_id.clone()),
            ..Default::default()
        },
    );

    Ok(Json(PairCompleteResponse {
        device_id,
        device_token,
    }))
}
