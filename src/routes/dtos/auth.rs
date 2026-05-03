use serde::{Deserialize, Serialize};

// ===== Login (device credentials) =====

#[derive(Debug, Deserialize)]
pub struct DeviceLoginRequest {
    pub device_id: String,
    pub device_token: String,
}

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

// ===== Pairing =====

#[derive(Debug, Clone, Serialize)]
pub struct PairingInitiateResponse {
    pub message: String,
    pub expires_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct PairCompleteRequest {
    pub pairing_code: String,
    pub device_name: String,
    /// Optional. If the client already has an FCM push token at pairing
    /// time it can register it here instead of making a separate
    /// `PATCH /me/fcm-token` call after login.
    #[serde(default)]
    pub fcm_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PairCompleteResponse {
    pub device_id: String,
    pub device_token: String,
}
