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
use ring::{aead, agreement, digest, hkdf, rand};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::notify::channel::{ChannelError, NotificationChannel};
use crate::notify::types::{Notification, NotificationEvent, Severity};
use crate::notify::url_policy::{WebhookPolicy, check_url};
use crate::services::webpush::client_public_key;
use crate::storage::repositories::{DeviceRepository, WebPushTarget};

const VAPID_SUB: &str = "mailto:noreply@remon.local";

/// Per-subscriber send timeout. Two attempts plus the backoff below must fit
/// inside the fanout wrapper's FANOUT_TIMEOUT (see notify/mod.rs).
const PER_DEVICE_TIMEOUT: Duration = Duration::from_secs(8);
/// Pause between a failed per-subscriber attempt and its single retry.
const PER_DEVICE_BACKOFF: Duration = Duration::from_millis(500);

// ring HKDF helper — tells ring how many bytes we want out of Expand.
struct OkmLen(usize);
impl hkdf::KeyType for OkmLen {
    fn len(&self) -> usize {
        self.0
    }
}

#[derive(Clone)]
pub struct WebPushChannel {
    pool: SqlitePool,
    client: reqwest::Client,
    /// SSRF policy applied to each subscriber's relay endpoint at send time.
    /// Endpoints are also validated at subscribe time (routes/rest/push.rs);
    /// re-checking here is the DNS-rebinding defense, mirroring the webhook
    /// and ntfy channels.
    policy: Arc<WebhookPolicy>,
}

impl WebPushChannel {
    pub fn new(pool: SqlitePool, client: reqwest::Client, policy: Arc<WebhookPolicy>) -> Self {
        Self {
            pool,
            client,
            policy,
        }
    }

    async fn send_to_subscriber(
        &self,
        target: &WebPushTarget,
        notification: &Notification,
    ) -> Result<(), ChannelError> {
        let device_id = target.device_id.as_str();
        let sub = &target.subscription;
        let (endpoint, p256dh, auth) = (
            sub.endpoint.as_str(),
            sub.p256dh.as_str(),
            sub.auth.as_str(),
        );
        // SSRF guard: re-resolve the relay endpoint on every send. Real push
        // relays (Mozilla/Google/Apple) are public HTTPS, so legitimate
        // subscriptions pass; an endpoint pointed at loopback/RFC1918/link-
        // local (incl. cloud metadata) is rejected.
        check_url(endpoint, &self.policy)
            .await
            .map_err(|e| ChannelError::Send(format!("endpoint blocked ({}): {}", device_id, e)))?;

        let payload = format_payload(notification, sub.reference.as_deref());

        let ciphertext = encrypt_payload(p256dh, auth, payload.as_bytes())
            .map_err(|e| ChannelError::Send(format!("encrypt ({}): {}", device_id, e)))?;

        let signing_pem = sub.vapid_key.as_str();
        let token = vapid_jwt(signing_pem, endpoint)
            .map_err(|e| ChannelError::Send(format!("VAPID JWT ({}): {}", device_id, e)))?;
        let pubkey = client_public_key(signing_pem)
            .map_err(|e| ChannelError::Send(format!("VAPID pubkey ({}): {}", device_id, e)))?;

        let authorization = format!("vapid t={},k={}", token, pubkey);

        let resp = self
            .client
            .post(endpoint)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("Authorization", &authorization)
            .header("TTL", "43200")
            .header("Urgency", urgency(notification))
            // A newer message for the same alert replaces one still waiting at the relay.
            .header("Topic", topic(&notification.target.key))
            .body(ciphertext)
            .send()
            .await
            // The per-subscriber relay endpoint is itself an unguessable
            // secret (equivalent to a bearer credential) — keep it out of
            // the logged error.
            .map_err(|e| {
                ChannelError::Send(format!("HTTP send ({}): {}", device_id, e.without_url()))
            })?;

        match resp.status().as_u16() {
            200..=299 => Ok(()),
            404 | 410 => {
                if let Err(db_err) = DeviceRepository::new(self.pool.clone())
                    .set_web_push_subscription(device_id, None)
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
        for target in targets {
            if !wants(
                target.subscription.min_severity.as_deref(),
                notification.severity,
            ) {
                continue;
            }
            let chan = self.clone();
            let notif = notification.clone();
            join_set.spawn(async move {
                // Retry per-subscriber (not per-channel) so a transient blip
                // on one relay gets a second chance without re-delivering to
                // subscribers already reached this fanout.
                let mut last = String::new();
                for attempt in 0u8..2 {
                    if attempt > 0 {
                        tokio::time::sleep(PER_DEVICE_BACKOFF).await;
                    }
                    match tokio::time::timeout(
                        PER_DEVICE_TIMEOUT,
                        chan.send_to_subscriber(&target, &notif),
                    )
                    .await
                    {
                        Ok(Ok(())) => return true,
                        Ok(Err(e)) => last = e.to_string(),
                        Err(_) => last = format!("timed out after {:?}", PER_DEVICE_TIMEOUT),
                    }
                }
                warn!(
                    "web-push device {} failed after retry: {}",
                    target.device_id, last
                );
                false
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

    fn self_retries(&self) -> bool {
        true
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

/// Structured, so the browser renders it in its own language and routes a tap.
fn format_payload(n: &Notification, reference: Option<&str>) -> String {
    let event = match n.event {
        NotificationEvent::Fired => "fired",
        NotificationEvent::Resolved => "resolved",
        NotificationEvent::HostEvent => "host_event",
        NotificationEvent::ActionRequired => "action_required",
    };
    serde_json::json!({
        "server": n.target.server,
        "key": n.target.key,
        "subject": n.target.subject,
        "detail": n.body,
        "severity": n.severity.as_str(),
        "event": event,
        "path": n.target.path,
        "ref": reference,
    })
    .to_string()
}

/// RFC 8030 urgency: a critical fire or a pending question should wake a
/// dozing phone; the rest can wait for its next window.
fn urgency(n: &Notification) -> &'static str {
    match (n.event, n.severity) {
        (NotificationEvent::ActionRequired, _) => "high",
        (NotificationEvent::Fired | NotificationEvent::HostEvent, Severity::Crit) => "high",
        _ => "normal",
    }
}

/// RFC 8030 topics are at most 32 URL-safe base64 characters, so the key is hashed.
fn topic(key: &str) -> String {
    let hash = digest::digest(&digest::SHA256, key.as_bytes());
    URL_SAFE_NO_PAD.encode(&hash.as_ref()[..24])
}

/// Whether a device asking for `min` and above takes this severity.
fn wants(min: Option<&str>, severity: Severity) -> bool {
    !(min == Some("crit") && severity == Severity::Warn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SecureRandom;

    /// Decrypt an RFC 8291 `aes128gcm` body from the *browser* side, mirroring
    /// the UA's key derivation. If this recovers the plaintext, `encrypt_payload`
    /// produced a spec-correct ciphertext end to end (ECDH → HKDF → AES-GCM).
    fn browser_decrypt(
        body: &[u8],
        browser_priv: agreement::EphemeralPrivateKey,
        browser_pub: &[u8],
        auth: &[u8],
    ) -> Vec<u8> {
        let salt = &body[0..16];
        let idlen = body[20] as usize;
        let sender_pub = &body[21..21 + idlen];
        let ciphertext = &body[21 + idlen..];

        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, sender_pub.to_vec());
        let ecdh = agreement::agree_ephemeral(browser_priv, &peer, |kd| kd.to_vec()).expect("ecdh");

        let prk_key = hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(ecdh.as_slice());
        let mut key_info = b"WebPush: info\x00".to_vec();
        key_info.extend_from_slice(browser_pub);
        key_info.extend_from_slice(sender_pub);
        let mut ikm = [0u8; 32];
        prk_key
            .expand(&[key_info.as_slice()], OkmLen(32))
            .unwrap()
            .fill(&mut ikm)
            .unwrap();

        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
        let mut cek = [0u8; 16];
        prk.expand(&[b"Content-Encoding: aes128gcm\x00"], OkmLen(16))
            .unwrap()
            .fill(&mut cek)
            .unwrap();
        let mut nonce = [0u8; 12];
        prk.expand(&[b"Content-Encoding: nonce\x00"], OkmLen(12))
            .unwrap()
            .fill(&mut nonce)
            .unwrap();

        let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
        let mut buf = ciphertext.to_vec();
        let plain = key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::empty(),
                &mut buf,
            )
            .expect("gcm open");
        let mut out = plain.to_vec();
        // RFC 8188 single/last record delimiter.
        assert_eq!(out.pop(), Some(0x02), "record delimiter");
        out
    }

    #[test]
    fn encrypt_payload_roundtrips() {
        let rng = rand::SystemRandom::new();
        let browser_priv =
            agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let browser_pub = browser_priv.compute_public_key().unwrap().as_ref().to_vec();
        let mut auth = [0u8; 16];
        rng.fill(&mut auth).unwrap();

        let p256dh = URL_SAFE_NO_PAD.encode(&browser_pub);
        let auth_b64 = URL_SAFE_NO_PAD.encode(auth);

        let plaintext = br#"{"title":"hi","body":"x"}"#;
        let body = encrypt_payload(&p256dh, &auth_b64, plaintext).expect("encrypt");

        // Header structure: salt(16) | rs(4 BE) | idlen(1) | keyid(65) | ct
        assert_eq!(body[20], 65, "keyid length byte");
        assert_eq!(
            u32::from_be_bytes(body[16..20].try_into().unwrap()),
            4096,
            "record size"
        );

        let recovered = browser_decrypt(&body, browser_priv, &browser_pub, &auth);
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn vapid_jwt_is_well_formed() {
        // Also exercises that jsonwebtoken accepts our PKCS#8 EC PEM.
        let key = crate::services::webpush::test_key();
        let token = vapid_jwt(&key, "https://push.example.com/a/b/c").unwrap();
        assert_eq!(token.split('.').count(), 3, "header.payload.signature");
    }

    #[test]
    fn parse_origin_strips_path_and_validates_scheme() {
        assert_eq!(
            parse_origin("https://push.example.com/a/b").unwrap(),
            "https://push.example.com"
        );
        assert_eq!(
            parse_origin("http://localhost:8080/x").unwrap(),
            "http://localhost:8080"
        );
        assert!(parse_origin("ftp://nope").is_err());
    }

    fn notif(event: NotificationEvent, severity: Severity) -> Notification {
        Notification {
            title: "[web-01] high cpu".to_string(),
            body: "cpu.usage_percent = 93".to_string(),
            severity,
            event,
            target: crate::notify::Target {
                server: "web-01".to_string(),
                key: "alert:7:{}".to_string(),
                subject: "high cpu".to_string(),
                path: "/alerts",
            },
        }
    }

    #[test]
    fn payload_says_what_where_and_how_bad() {
        let n = notif(NotificationEvent::Resolved, Severity::Warn);
        let v: serde_json::Value =
            serde_json::from_str(&format_payload(&n, Some("profile-1"))).unwrap();
        assert_eq!(v["detail"], "cpu.usage_percent = 93");
        assert_eq!(v["event"], "resolved");
        assert_eq!(v["severity"], "warn");
        assert_eq!(v["server"], "web-01");
        assert_eq!(v["key"], "alert:7:{}");
        assert_eq!(v["subject"], "high cpu");
        assert_eq!(v["path"], "/alerts");
        assert_eq!(v["ref"], "profile-1");
    }

    #[test]
    fn topic_is_a_valid_rfc8030_topic_and_stable() {
        let t = topic("alert:7:{\"mount\":\"/\"}");
        assert_eq!(t.len(), 32);
        assert!(
            t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(t, topic("alert:7:{\"mount\":\"/\"}"));
        assert_ne!(t, topic("alert:8:{\"mount\":\"/\"}"));
    }

    #[test]
    fn critical_fires_and_questions_are_urgent() {
        assert_eq!(
            urgency(&notif(NotificationEvent::Fired, Severity::Crit)),
            "high"
        );
        assert_eq!(
            urgency(&notif(NotificationEvent::ActionRequired, Severity::Warn)),
            "high"
        );
        assert_eq!(
            urgency(&notif(NotificationEvent::Fired, Severity::Warn)),
            "normal"
        );
        assert_eq!(
            urgency(&notif(NotificationEvent::Resolved, Severity::Crit)),
            "normal"
        );
    }

    #[test]
    fn min_severity_filters_warnings_only() {
        assert!(wants(None, Severity::Warn));
        assert!(wants(Some("warn"), Severity::Warn));
        assert!(!wants(Some("crit"), Severity::Warn));
        assert!(wants(Some("crit"), Severity::Crit));
    }

    #[test]
    fn a_browser_key_signs_and_names_itself() {
        let key = crate::services::webpush::test_key();
        let token = vapid_jwt(&key, "https://push.example.com/x").unwrap();
        assert_eq!(token.split('.').count(), 3);
        // 65-byte uncompressed point: 87 base64url characters, leading 0x04.
        let public = client_public_key(&key).unwrap();
        assert_eq!(public.len(), 87);
        assert!(public.starts_with('B'));
    }
}
