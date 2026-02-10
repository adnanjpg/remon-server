use serde::{Deserialize, Serialize};

/// Device registration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub last_ip: Option<String>,
    pub last_seen: i64,
    pub created_at: i64,
    pub is_active: bool,
    pub has_totp: bool,
}

/// Stored device (includes sensitive data)
#[derive(Debug, Clone)]
pub struct StoredDevice {
    pub id: String,
    pub name: String,
    pub token_hash: String,
    pub totp_secret: Option<String>,
    pub last_ip: Option<String>,
    pub last_seen: i64,
    pub created_at: i64,
    pub is_active: bool,
}

impl From<StoredDevice> for Device {
    fn from(d: StoredDevice) -> Self {
        Device {
            id: d.id,
            name: d.name,
            last_ip: d.last_ip,
            last_seen: d.last_seen,
            created_at: d.created_at,
            is_active: d.is_active,
            has_totp: d.totp_secret.is_some(),
        }
    }
}

/// Pairing initiation response (shown in terminal)
#[derive(Debug, Clone, Serialize)]
pub struct PairingCode {
    pub code: String,
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

/// Login request
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub device_id: String,
    pub device_token: String,
}

/// Login response
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

/// Refresh token request
#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// Token claims (JWT payload)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessTokenClaims {
    pub sub: String,       // device_id
    pub exp: i64,          // expiration
    pub iat: i64,          // issued at
    pub jti: String,       // unique token id
    pub typ: String,       // "access"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshTokenClaims {
    pub sub: String,       // device_id
    pub exp: i64,
    pub iat: i64,
    pub jti: String,
    pub typ: String,       // "refresh"
}

/// TOTP setup response
#[derive(Debug, Serialize)]
pub struct TotpSetupResponse {
    pub secret: String,
    pub qr_code_base64: String,
    pub otpauth_url: String,
}

/// TOTP verification request
#[derive(Debug, Deserialize)]
pub struct TotpVerifyRequest {
    pub code: String,
}
