//! Web Push notification channel.
//!
//! Symmetric counterpart to the FCM channel: one channel instance,
//! per-device fan-out. Each subscribed browser gets a separate
//! aes128gcm-encrypted POST to its assigned push relay endpoint
//! (Mozilla AutoPush / Google Web Push / Apple). VAPID auth identifies
//! us to the relay; the relay forwards to the browser without ever
//! seeing the plaintext payload.
//!
//! Failures are non-fatal — one expired subscription doesn't stop the
//! rest. A future polish pass can detect 410 Gone responses and clear
//! the corresponding device's subscription row, but for phase 3 we
//! just log and move on.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use log::{debug, warn};
use sqlx::SqlitePool;
use web_push::{
    ContentEncoding, HyperWebPushClient, SubscriptionInfo, SubscriptionKeys,
    VapidSignatureBuilder, WebPushClient, WebPushError, WebPushMessageBuilder,
};

use crate::notify::channel::{ChannelError, NotificationChannel};
use crate::notify::types::{Notification, NotificationEvent, Severity};
use crate::services::webpush::VapidKeyPair;
use crate::storage::repositories::DeviceRepository;

/// `sub` claim value baked into every VAPID JWT. Push relays require
/// it as a "where to email if your traffic is abusive" contact; not
/// dispatched anywhere by the relay, just recorded with the request.
const VAPID_SUB: &str = "mailto:noreply@remon.local";

pub struct WebPushChannel {
    /// VAPID identity (PEM). Loaded once at construction; rebuilt into
    /// `VapidSignatureBuilder` per send because the builder consumes
    /// the key on `.build()`.
    vapid: Arc<VapidKeyPair>,
    pool: SqlitePool,
    /// hyper-based push client matching the runtime; reused across all
    /// fan-out batches. Cloning is cheap (Arc wrapper).
    client: HyperWebPushClient,
}

impl WebPushChannel {
    pub fn new(vapid: Arc<VapidKeyPair>, pool: SqlitePool) -> Result<Self, ChannelError> {
        Ok(Self {
            vapid,
            pool,
            client: HyperWebPushClient::new(),
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
        let sub_info = SubscriptionInfo {
            endpoint: endpoint.to_string(),
            keys: SubscriptionKeys {
                p256dh: p256dh.to_string(),
                auth: auth.to_string(),
            },
        };

        // Build the JSON payload. Service worker's `push` event handler
        // unpacks the same shape and feeds it to `showNotification()`.
        let payload = format_payload(notification);
        let payload_bytes = payload.as_bytes();

        // VAPID signature: re-parse the PEM each call because
        // `.build()` consumes the builder, and constructing one is
        // cheap (<1ms for ECDSA P-256). The `sub` claim is mandatory
        // per RFC 8292; relays return 401 without it.
        let mut sig_builder = VapidSignatureBuilder::from_pem(
            self.vapid.private_key_pem.as_bytes(),
            &sub_info,
        )
        .map_err(|e| ChannelError::Send(format!("VAPID builder ({}): {}", device_id, e)))?;
        sig_builder.add_claim("sub", VAPID_SUB);
        let sig = sig_builder
            .build()
            .map_err(|e| ChannelError::Send(format!("VAPID sign ({}): {}", device_id, e)))?;

        let mut builder = WebPushMessageBuilder::new(&sub_info);
        builder.set_payload(ContentEncoding::Aes128Gcm, payload_bytes);
        builder.set_vapid_signature(sig);
        // 12h TTL — enough to cover overnight outages; long-stale
        // alerts aren't useful when the user finally reconnects.
        builder.set_ttl(43_200);

        let msg = builder
            .build()
            .map_err(|e| ChannelError::Send(format!("build push msg ({}): {}", device_id, e)))?;

        match self.client.send(msg).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // 404 / 410 from the relay means the subscription is
                // permanently gone (user revoked permission, cleared
                // site data, uninstalled the PWA, etc.). Clear the row
                // so the next fan-out doesn't keep paying the encrypt+
                // post cost just to fail again.
                if matches!(
                    e,
                    WebPushError::EndpointNotFound(_) | WebPushError::EndpointNotValid(_)
                ) {
                    if let Err(db_err) = DeviceRepository::new(self.pool.clone())
                        .set_web_push_subscription(device_id, None, None, None)
                        .await
                    {
                        warn!("web-push: failed to clear dead subscription for {}: {}", device_id, db_err);
                    } else {
                        log::info!("web-push: cleared dead subscription for {}", device_id);
                    }
                }
                Err(ChannelError::Send(format!("relay ({}): {}", device_id, e)))
            }
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
            // SAFETY: WebPushChannel is Sync (fields are Sync); we clone
            // the cheap parts and reference-share via Arc semantics
            // implicit in the channel layer.
            let vapid = Arc::clone(&self.vapid);
            let pool = self.pool.clone();
            let client = self.client.clone();
            let notif = notification.clone();
            join_set.spawn(async move {
                let chan = WebPushChannel { vapid, pool, client };
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

    fn type_name(&self) -> &'static str {
        "web-push"
    }
}

fn format_payload(n: &Notification) -> String {
    let prefix = match n.severity {
        Severity::Crit => "[Critical] ",
        Severity::Warn => "[Warning] ",
    };
    let title = match n.event {
        NotificationEvent::Fired => format!("{}{}", prefix, n.title),
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
