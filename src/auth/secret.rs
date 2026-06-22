//! Effective JWT signing-secret resolution.
//!
//! Precedence: an explicit strong secret from config/env wins — it lets an
//! operator rotate the secret or share one across instances. Otherwise the
//! server falls back to a per-install secret generated on first boot and
//! persisted in `server_secrets`, mirroring the generate-or-load contract the
//! VAPID keypair uses (see `services::webpush`). A fresh deployment therefore
//! comes up securely with no secret wrangling.

use anyhow::{Context, Result};
use rand::Rng;
use sqlx::SqlitePool;

/// Minimum length we accept for an operator-provided secret.
const MIN_SECRET_LEN: usize = 32;
/// Placeholder shipped in `config/default.toml`; treated as "not provided".
const PLACEHOLDER_SECRET: &str = "d3f4ult";

/// Resolve the effective JWT secret. Returns the configured value when it is
/// strong enough to use as-is; otherwise loads (or generates and persists) the
/// per-install secret from the database.
pub async fn resolve(configured: &str, pool: &SqlitePool) -> Result<String> {
    if is_strong(configured) {
        log::info!("using operator-provided JWT secret from config/env");
        return Ok(configured.to_string());
    }

    // Distinguish "left at the shipped placeholder" (silent, expected) from
    // "operator set something, but it's too weak" (worth a warning).
    if configured != PLACEHOLDER_SECRET && !configured.is_empty() {
        log::warn!(
            "configured JWT secret is too weak ({} chars, minimum {}); using the \
             auto-generated per-install secret instead",
            configured.len(),
            MIN_SECRET_LEN
        );
    }

    load_or_generate(pool).await
}

/// An operator-provided secret is usable as-is only when it is neither the
/// shipped placeholder nor shorter than the minimum length.
fn is_strong(secret: &str) -> bool {
    secret != PLACEHOLDER_SECRET && secret.len() >= MIN_SECRET_LEN
}

/// Load the singleton secret, generating and persisting one on first call.
/// Idempotent; subsequent calls return the same secret.
async fn load_or_generate(pool: &SqlitePool) -> Result<String> {
    let existing: Option<(String,)> =
        sqlx::query_as("SELECT jwt_secret FROM server_secrets WHERE id = 1")
            .fetch_optional(pool)
            .await
            .context("read server_secrets row")?;
    if let Some((secret,)) = existing {
        return Ok(secret);
    }

    let secret = generate();
    sqlx::query("INSERT INTO server_secrets (id, jwt_secret) VALUES (1, ?)")
        .bind(&secret)
        .execute(pool)
        .await
        .context("insert generated server_secrets row")?;
    log::info!("generated and stored a fresh per-install JWT secret");
    Ok(secret)
}

/// 48 random bytes, hex-encoded — comfortably above the minimum length, drawn
/// from the same CSPRNG the device-token generator uses.
fn generate() -> String {
    let mut bytes = [0u8; 48];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;

    async fn migrated_pool() -> SqlitePool {
        let db = Database::connect("sqlite::memory:", 1)
            .await
            .expect("connect in-memory sqlite");
        db.migrate().await.expect("run migrations");
        db.pool().clone()
    }

    #[tokio::test]
    async fn weak_secret_generates_and_then_persists() {
        let pool = migrated_pool().await;

        let first = resolve(PLACEHOLDER_SECRET, &pool).await.expect("resolve");
        assert!(first.len() >= MIN_SECRET_LEN);

        // A second resolve must return the same persisted secret, not a new one.
        let second = resolve(PLACEHOLDER_SECRET, &pool)
            .await
            .expect("resolve again");
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn strong_secret_used_as_is_and_not_persisted() {
        let pool = migrated_pool().await;
        let strong = "this-is-a-perfectly-strong-secret-0123456789";

        let resolved = resolve(strong, &pool).await.expect("resolve");
        assert_eq!(resolved, strong);

        // The operator-provided path must not touch server_secrets.
        let row: Option<(String,)> =
            sqlx::query_as("SELECT jwt_secret FROM server_secrets WHERE id = 1")
                .fetch_optional(&pool)
                .await
                .expect("query server_secrets");
        assert!(row.is_none());
    }
}
