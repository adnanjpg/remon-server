//! Authentication service for device pairing and JWT token management

use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::config::AuthConfig;

/// Build the strict validation profile used for both access and refresh tokens.
///
/// - Locks the algorithm to HS256 (refuses the `alg=none` confusion attack).
/// - Requires `exp`, `sub`, and `iat` claims to be present.
/// - Audience validation is left off because we don't issue an `aud` claim yet;
///   if/when we add one, set `validate_aud = true` and `set_audience` here.
fn strict_validation() -> Validation {
    let mut v = Validation::new(Algorithm::HS256);
    v.validate_exp = true;
    v.validate_aud = false;
    v.required_spec_claims = HashSet::from_iter(
        ["exp", "sub", "iat"].iter().map(|s| s.to_string()),
    );
    v
}

/// Access token claims (short-lived)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessTokenClaims {
    /// Subject (device_id)
    pub sub: String,
    /// Expiration timestamp
    pub exp: i64,
    /// Issued at timestamp
    pub iat: i64,
    /// Unique token ID
    pub jti: String,
    /// Token type
    pub typ: String,
}

/// Refresh token claims (long-lived)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshTokenClaims {
    /// Subject (device_id)
    pub sub: String,
    /// Expiration timestamp
    pub exp: i64,
    /// Issued at timestamp
    pub iat: i64,
    /// Unique token ID
    pub jti: String,
    /// Token type
    pub typ: String,
}

/// Result of issuing a new access+refresh pair. Also exposes both jti's and
/// their expiry timestamps so the caller can persist them in the `sessions`
/// table for revocation tracking.
#[derive(Debug, Serialize)]
pub struct CreatedTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
    #[serde(skip)]
    pub access_jti: String,
    #[serde(skip)]
    pub access_expires_at: i64,
    #[serde(skip)]
    pub refresh_jti: String,
    #[serde(skip)]
    pub refresh_expires_at: i64,
}

/// Auth service for token generation and validation
pub struct AuthService {
    config: AuthConfig,
}

impl AuthService {
    pub fn new(config: AuthConfig) -> Self {
        Self { config }
    }

    /// Generate an 8-digit pairing code (~26-bit entropy).
    /// Combined with `PAIRING_MAX_ATTEMPTS` and the TTL window this is well
    /// out of online brute-force range.
    pub fn generate_pairing_code() -> String {
        format!("{:08}", rand::random_range(0..100_000_000u32))
    }

    /// Generate a secure device token (64 hex characters)
    pub fn generate_device_token() -> String {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        hex::encode(bytes)
    }

    /// Hash a device token using Argon2
    pub fn hash_token(token: &str) -> Result<String, String> {
        let salt = SaltString::generate(&mut OsRng);
        let argon2 = Argon2::default();

        let hash = argon2
            .hash_password(token.as_bytes(), &salt)
            .map_err(|e| format!("Failed to hash token: {}", e))?;

        Ok(hash.to_string())
    }

    /// Verify a device token against stored hash
    pub fn verify_token(token: &str, hash: &str) -> bool {
        let parsed_hash = match PasswordHash::new(hash) {
            Ok(h) => h,
            Err(_) => return false,
        };

        Argon2::default()
            .verify_password(token.as_bytes(), &parsed_hash)
            .is_ok()
    }

    // ==================== JWT Token Management ====================

    /// Issue a new access+refresh pair for a device. The caller is expected
    /// to persist both jti's into the `sessions` table so middleware can
    /// reject revoked tokens.
    pub fn create_tokens(
        &self,
        device_id: &str,
    ) -> Result<CreatedTokens, jsonwebtoken::errors::Error> {
        let now = chrono::Utc::now().timestamp();

        let access_jti = uuid::Uuid::new_v4().to_string();
        let access_exp = now + self.config.access_token_ttl_secs as i64;
        let access_claims = AccessTokenClaims {
            sub: device_id.to_string(),
            exp: access_exp,
            iat: now,
            jti: access_jti.clone(),
            typ: "access".to_string(),
        };
        let access_token = encode(
            &Header::default(),
            &access_claims,
            &EncodingKey::from_secret(self.config.jwt_secret.as_bytes()),
        )?;

        let refresh_jti = uuid::Uuid::new_v4().to_string();
        let refresh_exp = now + self.config.refresh_token_ttl_secs as i64;
        let refresh_claims = RefreshTokenClaims {
            sub: device_id.to_string(),
            exp: refresh_exp,
            iat: now,
            jti: refresh_jti.clone(),
            typ: "refresh".to_string(),
        };
        let refresh_token = encode(
            &Header::default(),
            &refresh_claims,
            &EncodingKey::from_secret(self.config.jwt_secret.as_bytes()),
        )?;

        Ok(CreatedTokens {
            access_token,
            refresh_token,
            expires_in: self.config.access_token_ttl_secs,
            access_jti,
            access_expires_at: access_exp,
            refresh_jti,
            refresh_expires_at: refresh_exp,
        })
    }

    /// Validate an access token. Verifies HS256 signature, expiry, required
    /// claims, and that `typ == "access"`.
    pub fn validate_access_token(
        &self,
        token: &str,
    ) -> Result<AccessTokenClaims, jsonwebtoken::errors::Error> {
        let data = decode::<AccessTokenClaims>(
            token,
            &DecodingKey::from_secret(self.config.jwt_secret.as_bytes()),
            &strict_validation(),
        )?;

        if data.claims.typ != "access" {
            return Err(jsonwebtoken::errors::Error::from(
                jsonwebtoken::errors::ErrorKind::InvalidToken,
            ));
        }

        Ok(data.claims)
    }

    /// Validate a refresh token. Same checks as `validate_access_token` but
    /// requires `typ == "refresh"`.
    pub fn validate_refresh_token(
        &self,
        token: &str,
    ) -> Result<RefreshTokenClaims, jsonwebtoken::errors::Error> {
        let data = decode::<RefreshTokenClaims>(
            token,
            &DecodingKey::from_secret(self.config.jwt_secret.as_bytes()),
            &strict_validation(),
        )?;

        if data.claims.typ != "refresh" {
            return Err(jsonwebtoken::errors::Error::from(
                jsonwebtoken::errors::ErrorKind::InvalidToken,
            ));
        }

        Ok(data.claims)
    }

    /// Get pairing code TTL
    pub fn pairing_code_ttl(&self) -> u64 {
        self.config.pairing_code_ttl_secs
    }
}
