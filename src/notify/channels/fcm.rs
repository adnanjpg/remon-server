use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chrono::Utc;
use log::{debug, warn};
use reqwest::Client;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::RwLock;

use crate::notify::channel::{ChannelError, NotificationChannel};
use crate::notify::types::{Notification, NotificationEvent};
use crate::storage::repositories::DeviceRepository;

// ── Service account ───────────────────────────────────────────────────────────

struct ServiceAccount {
    project_id: String,
    client_email: String,
    token_uri: String,
    /// Key ID — included as `kid` in the JWT header per Google's recommendation.
    key_id: String,
    /// PKCS#8 DER bytes. `RsaKeyPair` is not Sync so we re-parse per JWT
    /// build (once per hour at most — negligible cost).
    key_der: Vec<u8>,
}

struct CachedToken {
    token: String,
    expires_at: i64,
}

// ── Channel ───────────────────────────────────────────────────────────────────

struct FcmInner {
    sa: ServiceAccount,
    http: Client,
    token_cache: RwLock<Option<CachedToken>>,
}

pub struct FcmChannel {
    inner: Arc<FcmInner>,
    pool: SqlitePool,
}

impl FcmChannel {
    pub async fn new(
        service_account_path: &str,
        http: Client,
        pool: SqlitePool,
    ) -> Result<Self, ChannelError> {
        let json_str = tokio::fs::read_to_string(service_account_path)
            .await
            .map_err(|e| {
                ChannelError::Config(format!(
                    "cannot read FCM service account '{}': {}",
                    service_account_path, e
                ))
            })?;

        #[derive(Deserialize)]
        struct SaJson {
            project_id: String,
            client_email: String,
            #[serde(default = "default_token_uri")]
            token_uri: String,
            private_key_id: String,
            private_key: String,
        }
        fn default_token_uri() -> String {
            "https://oauth2.googleapis.com/token".to_string()
        }

        let raw: SaJson = serde_json::from_str(&json_str)
            .map_err(|e| ChannelError::Config(format!("parse FCM service account: {}", e)))?;

        let key_der = pem_body_to_der(&raw.private_key)
            .map_err(|e| ChannelError::Config(format!("decode FCM private key: {}", e)))?;

        // Validate the key parses at construction time.
        RsaKeyPair::from_pkcs8(&key_der)
            .map_err(|e| ChannelError::Config(format!("invalid FCM RSA key: {:?}", e)))?;

        Ok(Self {
            inner: Arc::new(FcmInner {
                sa: ServiceAccount {
                    project_id: raw.project_id,
                    client_email: raw.client_email,
                    token_uri: raw.token_uri,
                    key_id: raw.private_key_id,
                    key_der,
                },
                http,
                token_cache: RwLock::new(None),
            }),
            pool,
        })
    }
}

// ── OAuth2 token management ───────────────────────────────────────────────────

impl FcmInner {
    async fn access_token(&self) -> Result<String, ChannelError> {
        {
            let cache = self.token_cache.read().await;
            if let Some(ref c) = *cache
                && Utc::now().timestamp() < c.expires_at - 60
            {
                return Ok(c.token.clone());
            }
        }

        let token = self.fetch_token().await?;
        *self.token_cache.write().await = Some(CachedToken {
            token: token.clone(),
            expires_at: Utc::now().timestamp() + 3600,
        });
        Ok(token)
    }

    async fn fetch_token(&self) -> Result<String, ChannelError> {
        let jwt = build_jwt(
            &self.sa.key_der,
            &self.sa.key_id,
            &self.sa.client_email,
            &self.sa.token_uri,
        )
        .map_err(|e| ChannelError::Send(format!("build FCM JWT: {}", e)))?;

        #[derive(Deserialize)]
        struct TokenResp {
            access_token: String,
        }

        let resp = self
            .http
            .post(&self.sa.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth2:grant-type:jwt-bearer"),
                ("assertion", jwt.as_str()),
            ])
            .send()
            .await
            .map_err(|e| ChannelError::Send(format!("FCM token request: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            debug!("FCM token endpoint raw error body: {}", body);
            // Google's token endpoint returns {"error":"...", "error_description":"..."}.
            // `error` is an enum (e.g. invalid_grant); the description may include the
            // service-account email — keep it out of the public error string.
            let code = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string());
            return Err(ChannelError::Send(format!(
                "FCM token endpoint: {} ({})",
                status, code
            )));
        }

        let tr: TokenResp = resp
            .json()
            .await
            .map_err(|e| ChannelError::Send(format!("FCM token parse: {}", e)))?;

        Ok(tr.access_token)
    }

    async fn send_to_device(
        &self,
        device_token: &str,
        notification: &Notification,
        access_token: &str,
        project_id: &str,
    ) -> Result<(), ChannelError> {
        let (title, body) = format_fcm(notification);
        let url = format!(
            "https://fcm.googleapis.com/v1/projects/{}/messages:send",
            project_id
        );

        let resp = self
            .http
            .post(&url)
            .bearer_auth(access_token)
            .json(&serde_json::json!({
                "message": {
                    "token": device_token,
                    "notification": { "title": title, "body": body },
                    "android": { "priority": "HIGH" },
                    "apns": { "headers": { "apns-priority": "10" } },
                }
            }))
            .send()
            .await
            .map_err(|e| ChannelError::Send(e.to_string()))?;

        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            debug!("FCM send raw error body: {}", body);
            Err(ChannelError::Send(format!(
                "FCM {} ({})",
                status,
                summarize_fcm_error(&body)
            )))
        }
    }
}

/// Extract only safe enum-valued fields from an FCM v1 error response.
/// Drops `error.message` (free-text, may include identifiers/project refs).
/// Returns "unknown" if the body isn't recognizable.
fn summarize_fcm_error(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return "unknown".to_string();
    };
    let err = v.get("error");
    let status = err
        .and_then(|e| e.get("status"))
        .and_then(|s| s.as_str())
        .unwrap_or("unknown");
    let fcm_code = err
        .and_then(|e| e.get("details"))
        .and_then(|d| d.as_array())
        .and_then(|arr| {
            arr.iter()
                .find_map(|d| d.get("errorCode").and_then(|c| c.as_str()))
        });
    match fcm_code {
        Some(c) => format!("{}/{}", status, c),
        None => status.to_string(),
    }
}

// ── NotificationChannel impl ──────────────────────────────────────────────────

#[async_trait]
impl NotificationChannel for FcmChannel {
    async fn send(&self, notification: &Notification) -> Result<usize, ChannelError> {
        let access_token = self.inner.access_token().await?;

        let targets = DeviceRepository::new(self.pool.clone())
            .list_active_fcm_targets()
            .await
            .map_err(|e| ChannelError::Send(format!("load FCM targets: {}", e)))?;

        if targets.is_empty() {
            return Ok(0);
        }

        let inner = Arc::clone(&self.inner);
        let project_id = inner.sa.project_id.clone();
        let mut join_set = tokio::task::JoinSet::new();

        for (device_id, device_token) in targets {
            let inner = Arc::clone(&inner);
            let at = access_token.clone();
            let notif = notification.clone();
            let pid = project_id.clone();

            join_set.spawn(async move {
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    inner.send_to_device(&device_token, &notif, &at, &pid),
                )
                .await
                {
                    Ok(Ok(())) => true,
                    Ok(Err(e)) => {
                        warn!("FCM device {}: {}", device_id, e);
                        false
                    }
                    Err(_) => {
                        warn!("FCM device {} timed out", device_id);
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

// ── JWT building ──────────────────────────────────────────────────────────────

fn build_jwt(
    key_der: &[u8],
    key_id: &str,
    client_email: &str,
    token_uri: &str,
) -> Result<String, String> {
    let now = Utc::now().timestamp();

    #[derive(Serialize)]
    struct Claims<'a> {
        iss: &'a str,
        scope: &'static str,
        aud: &'a str,
        exp: i64,
        iat: i64,
    }

    // Include `kid` (key ID) so Google can identify which key signed the JWT
    // without inspecting the payload — recommended by Google's auth docs.
    let header_json = format!(r#"{{"alg":"RS256","typ":"JWT","kid":"{}"}}"#, key_id);
    let header = URL_SAFE_NO_PAD.encode(&header_json);
    let claims_json = serde_json::to_string(&Claims {
        iss: client_email,
        scope: "https://www.googleapis.com/auth/firebase.messaging",
        aud: token_uri,
        exp: now + 3600,
        iat: now,
    })
    .map_err(|e| e.to_string())?;

    let claims = URL_SAFE_NO_PAD.encode(&claims_json);
    let signing_input = format!("{}.{}", header, claims);

    let key_pair = RsaKeyPair::from_pkcs8(key_der).map_err(|e| format!("RsaKeyPair: {:?}", e))?;

    let rng = SystemRandom::new();
    let mut sig = vec![0u8; key_pair.public().modulus_len()];
    key_pair
        .sign(&RSA_PKCS1_SHA256, &rng, signing_input.as_bytes(), &mut sig)
        .map_err(|e| format!("RSA sign: {:?}", e))?;

    Ok(format!(
        "{}.{}",
        signing_input,
        URL_SAFE_NO_PAD.encode(&sig)
    ))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Decode a PEM-encoded key (any header) to raw DER bytes.
fn pem_body_to_der(pem: &str) -> Result<Vec<u8>, String> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    STANDARD.decode(body.trim()).map_err(|e| e.to_string())
}

fn format_fcm(n: &Notification) -> (String, String) {
    let title = match n.event {
        NotificationEvent::Fired => format!("{}{}", n.severity.label(), n.title),
        NotificationEvent::Resolved => format!("[Resolved] {}", n.title),
    };
    (title, n.body.clone())
}
