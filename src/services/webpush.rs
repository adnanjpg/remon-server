//! Web Push VAPID keys.
//!
//! The server holds no VAPID identity of its own. A browser may hold only one
//! push subscription, so it makes its own P-256 key, subscribes with it, and
//! hands the private half (PKCS#8 PEM) to every server it wants alerts from;
//! each signs that browser's pushes with it. It authorises pushes to that one
//! subscription and nothing else.

use anyhow::{Context, Result};
use p256::{SecretKey, elliptic_curve::sec1::ToSec1Point, pkcs8::DecodePrivateKey};

/// Public half of a VAPID private key (PKCS#8 PEM), base64url uncompressed
/// point: what the relay expects in `Authorization: vapid k=`. Also the
/// validity check for a key a browser hands us.
pub fn client_public_key(private_key_pem: &str) -> Result<String> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let secret = SecretKey::from_pkcs8_pem(private_key_pem).context("parse VAPID PKCS#8 PEM")?;
    let encoded = secret.public_key().to_sec1_point(false); // uncompressed
    Ok(URL_SAFE_NO_PAD.encode(encoded.as_bytes()))
}

/// A fresh browser-style key, for tests.
#[cfg(test)]
pub fn test_key() -> String {
    use p256::{
        elliptic_curve::Generate,
        pkcs8::{EncodePrivateKey, LineEnding},
    };
    SecretKey::generate_from_rng(&mut rand::rng())
        .to_pkcs8_pem(LineEnding::LF)
        .expect("encode PKCS#8 PEM")
        .to_string()
}
