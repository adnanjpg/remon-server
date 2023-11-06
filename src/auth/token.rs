use chrono::{Duration, Utc};
use std::time::Duration as StdDuration;

use serde::{Deserialize, Serialize};

use jsonwebtoken::{
    decode, encode, errors::Error as JwtError, errors::ErrorKind as JwtErrorKind, Algorithm,
    DecodingKey, EncodingKey, Header, Validation,
};

// TODO: !
const JWT_SECRET: &str = "d3f4ult";

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
        &EncodingKey::from_secret(JWT_SECRET.as_ref()),
    )?;

    Ok(token)
}

pub async fn validate_token(auth_token: &str) -> Result<String, JwtError> {
    if !auth_token.starts_with("Bearer ") {
        return Err(JwtError::from(JwtErrorKind::InvalidToken));
    }

    let jwt = auth_token.trim_start_matches("Bearer ");

    decode::<Claims>(
        jwt,
        &DecodingKey::from_secret(JWT_SECRET.as_ref()),
        &Validation::new(Algorithm::HS256),
    )
    .map(|data| data.claims.device_id)
}
