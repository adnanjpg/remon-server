//! Web Push subscription + VAPID key management.
//!
//! Phase 2 scope: generate-or-load the server's VAPID identity once,
//! persist subscription rows when clients opt in, and expose the
//! public half so the browser can sign its subscribe call. Phase 3
//! will layer the actual `aes128gcm` payload encryption + HTTP
//! delivery on top.
//!
//! Storage layout: keypair lives in the singleton `vapid_keys` row,
//! per-browser subscriptions live on the existing `devices` row
//! (`web_push_endpoint`, `_p256dh`, `_auth`). Nullable when not
//! subscribed.

use anyhow::{Context, Result};
use p256::{
    SecretKey,
    elliptic_curve::{rand_core::OsRng, sec1::ToEncodedPoint},
    pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding},
};
use sqlx::SqlitePool;

/// Stored VAPID keypair. Both halves are PEM-encoded so the signer
/// (phase 3) can read the private key directly via standard PKCS#8
/// loading; the public key is also exposed to clients in raw
/// uncompressed-point base64url form via `public_key_for_client()`.
#[derive(Debug, Clone)]
pub struct VapidKeyPair {
    pub public_key_pem: String,
    pub private_key_pem: String,
}

impl VapidKeyPair {
    /// Generate a fresh ECDSA P-256 keypair (the algorithm Web Push
    /// mandates) and return it PEM-encoded.
    pub fn generate() -> Result<Self> {
        let secret = SecretKey::random(&mut OsRng);
        let private_key_pem = secret
            .to_pkcs8_pem(LineEnding::LF)
            .context("encode VAPID private key as PKCS#8 PEM")?
            .to_string();
        let public_key_pem = secret.public_key().to_string();
        Ok(Self {
            public_key_pem,
            private_key_pem,
        })
    }

    /// Convert the public half into the base64url-encoded uncompressed
    /// point string (65 raw bytes, leading 0x04) the browser's
    /// `applicationServerKey` field expects.
    pub fn public_key_for_client(&self) -> Result<String> {
        let secret = SecretKey::from_pkcs8_pem(&self.private_key_pem)
            .context("re-parse VAPID PKCS#8 PEM")?;
        let encoded = secret.public_key().to_encoded_point(false); // uncompressed
        Ok(base64url_encode(encoded.as_bytes()))
    }
}

/// Load the singleton VAPID keypair from the database, generating
/// (and persisting) a fresh one on first call. Idempotent; subsequent
/// calls return the same keypair.
pub async fn load_or_generate(pool: &SqlitePool) -> Result<VapidKeyPair> {
    let existing: Option<(String, String)> =
        sqlx::query_as("SELECT public_key, private_key FROM vapid_keys WHERE id = 1")
            .fetch_optional(pool)
            .await
            .context("read vapid_keys row")?;
    if let Some((public_key_pem, private_key_pem)) = existing {
        return Ok(VapidKeyPair {
            public_key_pem,
            private_key_pem,
        });
    }
    let pair = VapidKeyPair::generate()?;
    sqlx::query("INSERT INTO vapid_keys (id, public_key, private_key) VALUES (1, ?, ?)")
        .bind(&pair.public_key_pem)
        .bind(&pair.private_key_pem)
        .execute(pool)
        .await
        .context("insert generated vapid_keys row")?;
    log::info!("Generated and stored fresh VAPID keypair");
    Ok(pair)
}

/// Standard URL-safe base64 without padding — the encoding Web Push
/// uses for `applicationServerKey`, p256dh, and auth.
fn base64url_encode(bytes: &[u8]) -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    URL_SAFE_NO_PAD.encode(bytes)
}
