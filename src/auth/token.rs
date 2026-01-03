use chrono::{Duration, Utc};
use std::time::Duration as StdDuration;

use serde::{Deserialize, Serialize};

use jsonwebtoken::{
    decode, encode, errors::Error as JwtError, errors::ErrorKind as JwtErrorKind, Algorithm,
    DecodingKey, EncodingKey, Header, Validation,
};

use crate::config::Config;

const MIN_JWT_SECRET_LENGTH: usize = 32;
const DEFAULT_SECRET: &str = "d3f4ult";

/// Validate JWT secret strength at startup
pub fn validate() -> Result<(), String> {
    let config = Config::new().map_err(|e| format!("Failed to load config: {}", e))?;
    let secret = &config.auth.jwt_secret;

    if secret == DEFAULT_SECRET || secret.len() < MIN_JWT_SECRET_LENGTH {
        if cfg!(debug_assertions) {
            eprintln!("⚠️  WARNING: Using weak/default JWT secret in development!");
            eprintln!("⚠️  Current secret length: {} chars (minimum: {})", secret.len(), MIN_JWT_SECRET_LENGTH);
            eprintln!("⚠️  Set REMON__AUTH__JWT_SECRET or update config/development.toml");
            eprintln!("⚠️  Example: REMON__AUTH__JWT_SECRET=\"$(openssl rand -base64 32)\"");
            Ok(())
        } else {
            Err(format!(
                "FATAL SECURITY ERROR: JWT_SECRET not set or too weak for production.\n\
                 Current secret length: {} chars (minimum required: {})\n\
                 Set REMON__AUTH__JWT_SECRET environment variable with a strong secret.\n\
                 Generate one with: openssl rand -base64 48",
                secret.len(),
                MIN_JWT_SECRET_LENGTH
            ))
        }
    } else {
        Ok(())
    }
}

fn get_jwt_secret() -> Result<String, String> {
    let config = Config::new().map_err(|e| format!("Failed to load config: {}", e))?;

    let secret = &config.auth.jwt_secret;

    // Check if using default or weak secret
    if secret == DEFAULT_SECRET || secret.len() < MIN_JWT_SECRET_LENGTH {
        if cfg!(debug_assertions) {
            // Development: Warning but allow
            eprintln!("⚠️  WARNING: Using weak/default JWT secret in development!");
            eprintln!("⚠️  Current secret length: {} chars (minimum: {})", secret.len(), MIN_JWT_SECRET_LENGTH);
            eprintln!("⚠️  Set REMON__AUTH__JWT_SECRET or update config/development.toml");
            eprintln!("⚠️  Example: REMON__AUTH__JWT_SECRET=\"$(openssl rand -base64 32)\"");

            // Return the weak secret but warn
            Ok(secret.clone())
        } else {
            // Production: FAIL FAST
            Err(format!(
                "FATAL SECURITY ERROR: JWT_SECRET not set or too weak for production.\n\
                 Current secret length: {} chars (minimum required: {})\n\
                 Set REMON__AUTH__JWT_SECRET environment variable with a strong secret.\n\
                 Generate one with: openssl rand -base64 48",
                secret.len(),
                MIN_JWT_SECRET_LENGTH
            ))
        }
    } else {
        Ok(secret.clone())
    }
}

const TOKEN_EXPIRE_TIME: StdDuration = StdDuration::from_secs(60 * 60);

#[derive(Debug, Deserialize, Serialize)]
pub struct LoginRequest {
    pub device_id: String,
    pub otp: String,
}

#[derive(Deserialize)]
pub struct AuthHeader {
    pub device_id: String,
    pub token: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    device_id: String,
    exp: i64,
}

pub async fn generate_token(device_id: &str) -> Result<String, JwtError> {
    let secret = get_jwt_secret().map_err(|_e| {
        JwtError::from(JwtErrorKind::InvalidKeyFormat)
    })?;

    let token_expire = Utc::now()
        .checked_add_signed(Duration::from_std(TOKEN_EXPIRE_TIME).unwrap())
        .unwrap()
        .timestamp();
    let claims = Claims {
        device_id: device_id.to_owned(),
        exp: token_expire,
    };

    let token = encode::<Claims>(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_ref()),
    )?;

    Ok(token)
}

pub async fn validate_token(auth_token: &str) -> Result<String, JwtError> {
    if !auth_token.starts_with("Bearer ") {
        return Err(JwtError::from(JwtErrorKind::InvalidToken));
    }

    let secret = get_jwt_secret().map_err(|_e| {
        JwtError::from(JwtErrorKind::InvalidKeyFormat)
    })?;

    let jwt = auth_token.trim_start_matches("Bearer ");

    let dec = decode::<Claims>(
        jwt,
        &DecodingKey::from_secret(secret.as_ref()),
        &Validation::new(Algorithm::HS256),
    );

    let dev_id = dec.map(|data| data.claims.device_id);

    dev_id
}
