use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::ConnectOptions;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};

const SQLITE_DB_PATH: &str = "sqlite:./db/auth.sqlite3";

const TOKEN_EXPIRE_TIME: i64 = 60 * 60 * 24;

// TODO: !
const JWT_SECRET: &str = "d3f4ult";

#[derive(sqlx::FromRow)]
struct Token {
    id: i32,
    device_id: String,
    token: String,
    token_expire: i64,
}

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
struct JwtClaims {
    device_id: String,
    exp: i64,
}

pub async fn generate_token(device_id: &str) -> Result<String, jsonwebtoken::errors::Error> {
    let token_expire = chrono::Utc::now()
        .checked_add_signed(chrono::Duration::seconds(TOKEN_EXPIRE_TIME))
        .unwrap()
        .timestamp();

    let claims = JwtClaims {
        device_id: device_id.to_owned(),
        exp: token_expire,
    };

    let token = encode::<JwtClaims>(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(JWT_SECRET.as_ref()),
    )?;

    // TODO(): fix return error types
    // since we no longer need to check the expire date we saved
    // this part maybe redundant
    insert_token(device_id, &token, token_expire).await.unwrap();

    Ok(token)
}

// TODO(isaidsari): find more convenient way
fn extract_token(auth_header: &str) -> Option<&str> {
    // Split the string by whitespace and take the second part
    let parts: Vec<&str> = auth_header.split_whitespace().collect();

    if parts.len() == 2 && parts[0] == "Bearer" {
        // Return the token if "Bearer" is the first part
        Some(parts[1])
    } else {
        // Return None if the format is incorrect
        None
    }
}

pub async fn validate_token(auth_payload: &str) -> Result<String, jsonwebtoken::errors::Error> {
    let jwt = match extract_token(auth_payload) {
        Some(jwt) => jwt,
        None => {
            return Err(jsonwebtoken::errors::Error::from(
                jsonwebtoken::errors::ErrorKind::InvalidToken,
            ))
        }
    };

    decode::<JwtClaims>(
        jwt,
        &DecodingKey::from_secret(JWT_SECRET.as_ref()),
        &Validation::new(Algorithm::HS256),
    )
    .map(|data| data.claims.device_id)
}

/// not tested yet
pub async fn create_table() -> Result<(), sqlx::Error> {
    let mut conn = SqliteConnectOptions::from_str(SQLITE_DB_PATH)
        .unwrap()
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS tokens (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            device_id TEXT NOT NULL,
            token TEXT NOT NULL,
            token_expire INTEGER NOT NULL
        )",
    )
    .execute(&mut conn)
    .await?;

    Ok(())
}

pub async fn insert_token(
    device_id: &str,
    token: &str,
    token_expire: i64,
) -> Result<(), sqlx::Error> {
    let mut conn = SqliteConnectOptions::from_str(SQLITE_DB_PATH)
        .unwrap()
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await?;

    sqlx::query("INSERT INTO tokens (device_id, token, token_expire) VALUES (?, ?, ?)")
        .bind(device_id)
        .bind(token)
        .bind(token_expire)
        .execute(&mut conn)
        .await?;

    Ok(())
}

pub async fn fetch_token(device_id: &str) -> Result<Option<String>, sqlx::Error> {
    let mut conn = SqliteConnectOptions::from_str(SQLITE_DB_PATH)
        .unwrap()
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await?;

    let token = sqlx::query_as::<_, Token>("SELECT * FROM tokens WHERE device_id = ?")
        .bind(device_id)
        .fetch_optional(&mut conn)
        .await?;

    Ok(token.map(|t| t.token))
}
