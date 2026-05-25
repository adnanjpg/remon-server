//! Web Push notification channel — pure-Rust, no OpenSSL.
//!
//! Payload encryption: RFC 8291 (aes128gcm) via `ring`.
//! VAPID auth:         RFC 8292 (ES256 JWT) via `jsonwebtoken`.
//! HTTP transport:     `reqwest` (rustls).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use log::{debug, warn};
use ring::{aead, agreement, hkdf, rand};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::notify::channel::{ChannelError, NotificationChannel};
use crate::notify::types::{Notification, NotificationEvent, Severity};
use crate::services::webpush::VapidKeyPair;
use crate::storage::repositories::DeviceRepository;

const VAPID_SUB: &str = "mailto:noreply@remon.local";

// ring HKDF helper — tells ring how many bytes we want out of Expand.
struct OkmLen(usize);
impl hkdf::KeyType for OkmLen {
    fn len(&self) -> usize {
        self.0
    }
}

#[derive(Clone)]
pub struct WebPushChannel {
    vapid: Arc<VapidKeyPair>,
    pool: SqlitePool,
    client: reqwest::Client,
}

impl WebPushChannel {
    pub fn new(
        vapid: Arc<VapidKeyPair>,
        pool: SqlitePool,
        client: reqwest::Client,
    ) -> Result<Self, ChannelError> {
        Ok(Self {
            vapid,
            pool,
            client,
        })
    }

    async fn send_to_subscriber(
        &self,
        device_id: &str,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        notification: &Notification,
    ) -> Result<(), ChannelError> {
        let payload = format_payload(notification);

        let ciphertext = encrypt_payload(p256dh, auth, payload.as_bytes())
            .map_err(|e| ChannelError::Send(format!("encrypt ({}): {}", device_id, e)))?;

        let token = vapid_jwt(&self.vapid.private_key_pem, endpoint)
            .map_err(|e| ChannelError::Send(format!("VAPID JWT ({}): {}", device_id, e)))?;
        let pubkey = self
            .vapid
            .public_key_for_client()
            .map_err(|e| ChannelError::Send(format!("VAPID pubkey ({}): {}", device_id, e)))?;

        let authorization = format!("vapid t={},k={}", token, pubkey);

        let resp = self
            .client
            .post(endpoint)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("Authorization", &authorization)
            .header("TTL", "43200")
            .body(ciphertext)
            .send()
            .await
            .map_err(|e| ChannelError::Send(format!("HTTP send ({}): {}", device_id, e)))?;

        match resp.status().as_u16() {
            200..=299 => Ok(()),
            404 | 410 => {
                if let Err(db_err) = DeviceRepository::new(self.pool.clone())
                    .set_web_push_subscription(device_id, None, None, None)
                    .await
                {
                    warn!(
                        "web-push: failed to clear dead subscription for {}: {}",
                        device_id, db_err
                    );
                } else {
                    log::info!("web-push: cleared dead subscription for {}", device_id);
                }
                Err(ChannelError::Send(format!(
                    "relay ({}): endpoint gone",
                    device_id
                )))
            }
            status => Err(ChannelError::Send(format!(
                "relay ({}): HTTP {}",
                device_id, status
            ))),
        }
    }
}

#[async_trait]
impl NotificationChannel for WebPushChannel {
    async fn send(&self, notification: &Notification) -> Result<usize, ChannelError> {
        let targets = DeviceRepository::new(self.pool.clone())
            .list_active_web_push_targets()
            .await
            .map_err(|e| ChannelError::Send(format!("load web-push targets: {}", e)))?;

        if targets.is_empty() {
            debug!("web-push: no subscribed devices");
            return Ok(0);
        }

        let mut join_set = tokio::task::JoinSet::new();
        for (device_id, endpoint, p256dh, auth) in targets {
            let chan = self.clone();
            let notif = notification.clone();
            join_set.spawn(async move {
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    chan.send_to_subscriber(&device_id, &endpoint, &p256dh, &auth, &notif),
                )
                .await
                {
                    Ok(Ok(())) => true,
                    Ok(Err(e)) => {
                        warn!("web-push device {}: {}", device_id, e);
                        false
                    }
                    Err(_) => {
                        warn!("web-push device {} timed out", device_id);
                        false
                    }
                }
            });
        }

        let mut success = 0usize;
        while let Some(res) = join_set.join_next().await {
            if res.unwrap_or(false) {
                success += 1;
            }
        }
        Ok(success)
    }

}

/// RFC 8291 + RFC 8188 (aes128gcm) payload encryption using ring.
///
/// Output layout: salt(16) | rs(4 BE) | idlen(1) | sender_pub(65) | ciphertext
fn encrypt_payload(p256dh: &str, auth: &str, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let browser_pub_bytes = URL_SAFE_NO_PAD
        .decode(p256dh)
        .map_err(|e| anyhow::anyhow!("decode p256dh: {}", e))?;
    let auth_secret = URL_SAFE_NO_PAD
        .decode(auth)
        .map_err(|e| anyhow::anyhow!("decode auth: {}", e))?;

    let rng = rand::SystemRandom::new();

    // Random 16-byte salt
    let mut salt = [0u8; 16];
    rand::SecureRandom::fill(&rng, &mut salt).map_err(|_| anyhow::anyhow!("generate salt"))?;

    // Ephemeral P-256 key pair (sender side)
    let ephemeral_priv = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
        .map_err(|_| anyhow::anyhow!("generate ephemeral key"))?;
    let ephemeral_pub = ephemeral_priv
        .compute_public_key()
        .map_err(|_| anyhow::anyhow!("compute ephemeral public key"))?;
    let ephemeral_pub_bytes = ephemeral_pub.as_ref().to_vec(); // 65 bytes, uncompressed

    // ECDH: shared secret from ephemeral private + browser public key
    let browser_pub = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, &browser_pub_bytes);
    let ecdh_secret = agreement::agree_ephemeral(ephemeral_priv, &browser_pub, |kd| kd.to_vec())
        .map_err(|_| anyhow::anyhow!("ECDH agree"))?;

    // RFC 8291 §3.3 key derivation
    //
    // PRK_key = HKDF-Extract(auth_secret, ecdh_secret)
    let prk_key = hkdf::Salt::new(hkdf::HKDF_SHA256, &auth_secret).extract(&ecdh_secret);

    // IKM = HKDF-Expand(PRK_key, "WebPush: info\x00" || ua_pub || as_pub, 32)
    let mut key_info = b"WebPush: info\x00".to_vec();
    key_info.extend_from_slice(&browser_pub_bytes);
    key_info.extend_from_slice(&ephemeral_pub_bytes);
    let key_info_ref = [key_info.as_slice()];
    let ikm_okm = prk_key
        .expand(&key_info_ref, OkmLen(32))
        .map_err(|_| anyhow::anyhow!("HKDF expand IKM"))?;
    let mut ikm = vec![0u8; 32];
    ikm_okm
        .fill(&mut ikm)
        .map_err(|_| anyhow::anyhow!("HKDF fill IKM"))?;

    // PRK = HKDF-Extract(salt, IKM)
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &salt).extract(&ikm);

    // CEK = HKDF-Expand(PRK, "Content-Encoding: aes128gcm\x00", 16)
    let cek_okm = prk
        .expand(&[b"Content-Encoding: aes128gcm\x00"], OkmLen(16))
        .map_err(|_| anyhow::anyhow!("HKDF expand CEK"))?;
    let mut cek = [0u8; 16];
    cek_okm
        .fill(&mut cek)
        .map_err(|_| anyhow::anyhow!("HKDF fill CEK"))?;

    // NONCE = HKDF-Expand(PRK, "Content-Encoding: nonce\x00", 12)
    let nonce_okm = prk
        .expand(&[b"Content-Encoding: nonce\x00"], OkmLen(12))
        .map_err(|_| anyhow::anyhow!("HKDF expand nonce"))?;
    let mut nonce_bytes = [0u8; 12];
    nonce_okm
        .fill(&mut nonce_bytes)
        .map_err(|_| anyhow::anyhow!("HKDF fill nonce"))?;

    // Encrypt with AES-128-GCM; append 0x02 delimiter (single/last record per RFC 8188)
    let mut record = plaintext.to_vec();
    record.push(0x02);

    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, &cek)
            .map_err(|_| anyhow::anyhow!("build AES key"))?,
    );
    let nonce = aead::Nonce::try_assume_unique_for_key(&nonce_bytes)
        .map_err(|_| anyhow::anyhow!("build nonce"))?;
    key.seal_in_place_append_tag(nonce, aead::Aad::empty(), &mut record)
        .map_err(|_| anyhow::anyhow!("AES-GCM encrypt"))?;

    // RFC 8188 content header: salt(16) | rs(4 BE=4096) | idlen(1) | keyid(65)
    let mut output = Vec::with_capacity(16 + 4 + 1 + 65 + record.len());
    output.extend_from_slice(&salt);
    output.extend_from_slice(&4096u32.to_be_bytes());
    output.push(ephemeral_pub_bytes.len() as u8); // 65
    output.extend_from_slice(&ephemeral_pub_bytes);
    output.extend_from_slice(&record);

    Ok(output)
}

/// Build a VAPID JWT (RFC 8292) signed with ES256.
fn vapid_jwt(private_key_pem: &str, endpoint: &str) -> anyhow::Result<String> {
    #[derive(Serialize)]
    struct Claims {
        sub: &'static str,
        aud: String,
        exp: u64,
    }

    let aud = parse_origin(endpoint)?;
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 43_200; // 12 h

    let claims = Claims {
        sub: VAPID_SUB,
        aud,
        exp,
    };
    let key = EncodingKey::from_ec_pem(private_key_pem.as_bytes())
        .map_err(|e| anyhow::anyhow!("parse VAPID key: {}", e))?;
    encode(&Header::new(Algorithm::ES256), &claims, &key)
        .map_err(|e| anyhow::anyhow!("encode VAPID JWT: {}", e))
}

fn parse_origin(url: &str) -> anyhow::Result<String> {
    let (scheme, rest) = if let Some(r) = url.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = url.strip_prefix("http://") {
        ("http", r)
    } else {
        return Err(anyhow::anyhow!("invalid endpoint URL: {}", url));
    };
    let host = rest.split('/').next().unwrap_or(rest);
    Ok(format!("{}://{}", scheme, host))
}

fn format_payload(n: &Notification) -> String {
    let title = match n.event {
        NotificationEvent::Fired => format!("{}{}", n.severity.label(), n.title),
        NotificationEvent::Resolved => format!("[Resolved] {}", n.title),
    };
    let severity = match n.severity {
        Severity::Crit => "crit",
        Severity::Warn => "warn",
    };
    let event = match n.event {
        NotificationEvent::Fired => "fired",
        NotificationEvent::Resolved => "resolved",
    };
    serde_json::json!({
        "title": title,
        "body": n.body,
        "severity": severity,
        "event": event,
    })
    .to_string()
}
