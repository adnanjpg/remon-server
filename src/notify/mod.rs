pub mod channel;
pub mod channels;
pub mod types;
pub mod url_policy;

pub use types::{Notification, NotificationEvent, Severity};

use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use sqlx::SqlitePool;
use tokio::sync::RwLock;

use crate::config::NotificationsConfig;
use crate::notify::channel::NotificationChannel;
use crate::notify::url_policy::{WebhookPolicy, channel_check_url, check_url};
use crate::services::webpush::VapidKeyPair;
use crate::storage::repositories::NotificationChannelRepository;

struct ChannelSlot {
    id: i64,
    name: String,
    min_severity: Option<Severity>,
    inner: Arc<dyn NotificationChannel>,
}

pub struct NotificationManager {
    pool: SqlitePool,
    http: reqwest::Client,
    credentials: Arc<NotificationsConfig>,
    vapid: Arc<VapidKeyPair>,
    channels: RwLock<Vec<ChannelSlot>>,
}

impl NotificationManager {
    pub async fn new(
        pool: SqlitePool,
        credentials: NotificationsConfig,
        vapid: Arc<VapidKeyPair>,
    ) -> anyhow::Result<Arc<Self>> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?;

        let manager = Arc::new(Self {
            pool,
            http,
            credentials: Arc::new(credentials),
            vapid,
            channels: RwLock::new(Vec::new()),
        });

        manager.reload().await;
        Ok(manager)
    }

    /// Reload enabled channels from DB. Called at startup and after any
    /// channel create / update / delete via the REST API.
    pub async fn reload(&self) {
        match self.build_slots().await {
            Ok(slots) => {
                let count = slots.len();
                *self.channels.write().await = slots;
                info!("Notification channels loaded: {}", count);
            }
            Err(e) => warn!("Failed to load notification channels: {}", e),
        }
    }

    async fn build_slots(&self) -> anyhow::Result<Vec<ChannelSlot>> {
        let repo = NotificationChannelRepository::new(self.pool.clone());
        let rows = repo.list_enabled().await?;

        // One policy per reload pass — cheap to clone the Arc into each
        // webhook channel.
        let webhook_policy = Arc::new(WebhookPolicy::from_credentials(&self.credentials.webhook));

        let mut slots = Vec::new();
        for row in rows {
            let config: serde_json::Value = serde_json::from_str(&row.config).unwrap_or_default();

            // Boot-time audit: surface channels whose outbound URL the current
            // SSRF policy would block (webhook + ntfy), before the first alert
            // fires. The channel is still skipped (not loaded) — operator must
            // fix config or delete the channel.
            if let Some(url) = channel_check_url(&row.r#type, &config)
                && let Err(e) = check_url(&url, &webhook_policy).await
            {
                warn!(
                    "Skipping {} channel '{}': {} — \
                     see CONFIG.md [notifications.webhook] for allow-list options",
                    row.r#type, row.name, e
                );
                continue;
            }

            match channels::build_channel(
                &row.r#type,
                &config,
                &self.credentials,
                self.http.clone(),
                self.pool.clone(),
                &self.vapid,
                &webhook_policy,
            )
            .await
            {
                Ok(ch) => slots.push(ChannelSlot {
                    id: row.id,
                    name: row.name,
                    min_severity: parse_severity(row.min_severity.as_deref()),
                    inner: Arc::from(ch),
                }),
                Err(e) => warn!("Skipping channel '{}' ({}): {}", row.name, row.r#type, e),
            }
        }
        Ok(slots)
    }

    /// Fan-out a notification to all applicable, enabled channels.
    ///
    /// Each channel runs concurrently with a 10-second hard timeout.
    /// One channel failing never affects the others.
    /// Returns the total number of successful deliveries.
    pub async fn fanout(&self, notification: &Notification) -> usize {
        // Collect Arc clones while holding the read lock (fast — just pointer bumps).
        let targets: Vec<(String, Arc<dyn NotificationChannel>)> = {
            let slots = self.channels.read().await;
            slots
                .iter()
                .filter(|s| {
                    s.min_severity
                        .is_none_or(|min| severity_gte(notification.severity, min))
                })
                .map(|s| (s.name.clone(), Arc::clone(&s.inner)))
                .collect()
        };

        if targets.is_empty() {
            debug!(
                "fanout: no channels for severity {:?}",
                notification.severity
            );
            return 0;
        }

        let mut join_set = tokio::task::JoinSet::new();
        let notif = Arc::new(notification.clone());

        for (name, channel) in targets {
            let notif = Arc::clone(&notif);
            join_set.spawn(async move { send_with_retry(&name, &*channel, &notif).await });
        }

        let mut total = 0usize;
        while let Some(res) = join_set.join_next().await {
            total += res.unwrap_or(0);
        }
        total
    }

    /// Whether at least one loaded channel would receive a notification of
    /// this severity. Lets the alert evaluator avoid arming a rule's cooldown
    /// on a fire that has nowhere to go (no channels configured).
    pub async fn has_channel_for(&self, severity: Severity) -> bool {
        self.channels
            .read()
            .await
            .iter()
            .any(|s| s.min_severity.is_none_or(|min| severity_gte(severity, min)))
    }

    /// Snapshot of the current webhook SSRF policy, derived from server
    /// config. Used by REST create / update handlers to fail-fast at 400
    /// before persisting a channel row that would be blocked anyway.
    pub fn webhook_policy(&self) -> WebhookPolicy {
        WebhookPolicy::from_credentials(&self.credentials.webhook)
    }

    /// Send a test notification to a single channel by ID.
    pub async fn test_channel(&self, id: i64) -> Result<usize, String> {
        let channel: Option<Arc<dyn NotificationChannel>> = {
            let slots = self.channels.read().await;
            slots
                .iter()
                .find(|s| s.id == id)
                .map(|s| Arc::clone(&s.inner))
        };

        let channel = channel.ok_or_else(|| {
            "channel not found or not loaded (check server logs for config errors)".to_string()
        })?;

        let test_notif = Notification {
            title: "Remon — Test Notification".to_string(),
            body: "Your notification channel is working correctly.".to_string(),
            severity: Severity::Warn,
            event: NotificationEvent::Fired,
        };

        // Self-retrying channels can take a full fan-out budget; single-shot
        // channels are bounded tighter for a snappy "test" button.
        let budget = if channel.self_retries() {
            FANOUT_TIMEOUT
        } else {
            Duration::from_secs(10)
        };
        tokio::time::timeout(budget, channel.send(&test_notif))
            .await
            .map_err(|_| "channel timed out".to_string())?
            .map_err(|e| e.to_string())
    }
}

/// Per-attempt timeout for a single channel.send() call. Tightened from
/// the prior 10 s so the retry budget (2 attempts + 500 ms backoff)
/// stays under ~11 s total in the worst case.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause between the first failed attempt and the retry. Short enough
/// that a real transient hiccup (DNS blip, TCP reset) is over, long
/// enough that we don't hammer a struggling upstream.
const RETRY_BACKOFF: Duration = Duration::from_millis(500);
/// Per-`send()` budget for self-retrying multi-target channels (FCM, Web
/// Push). One `send()` fans out to every subscriber, each bounded by its own
/// per-device timeout + one retry, all concurrent — so this must comfortably
/// exceed a single device's worst case (~16.5 s) without re-running the whole
/// fan-out (which would double-notify already-delivered devices).
const FANOUT_TIMEOUT: Duration = Duration::from_secs(30);

/// Best-effort send with one retry on transient failures (TCP reset, DNS
/// blip, 503). Permanent failures (auth, malformed config) burn the same
/// per-attempt timeout twice — acceptable since channels fan out concurrently.
async fn send_with_retry(
    name: &str,
    channel: &dyn NotificationChannel,
    notif: &Notification,
) -> usize {
    // Multi-target channels (FCM, Web Push) retry per-target internally and
    // re-deliver to every subscriber on each send(); a whole-channel retry
    // here would double-notify already-reached devices. Run them exactly once
    // under a budget that covers the full fan-out, and skip the outer retry.
    if channel.self_retries() {
        return match tokio::time::timeout(FANOUT_TIMEOUT, channel.send(notif)).await {
            Ok(Ok(n)) => {
                debug!("Channel '{}' delivered {} notification(s)", name, n);
                n
            }
            Ok(Err(e)) => {
                warn!("Channel '{}' failed: {}", name, e);
                0
            }
            Err(_) => {
                warn!("Channel '{}' timed out after {:?}", name, FANOUT_TIMEOUT);
                0
            }
        };
    }

    let attempt_once = || async {
        match tokio::time::timeout(SEND_TIMEOUT, channel.send(notif)).await {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(e)) => Err(format!("channel error: {}", e)),
            Err(_) => Err(format!("timed out after {:?}", SEND_TIMEOUT)),
        }
    };

    match attempt_once().await {
        Ok(n) => {
            debug!("Channel '{}' delivered {} notification(s)", name, n);
            n
        }
        Err(first_err) => {
            debug!(
                "Channel '{}' first attempt failed ({}) — retrying after {:?}",
                name, first_err, RETRY_BACKOFF
            );
            tokio::time::sleep(RETRY_BACKOFF).await;
            match attempt_once().await {
                Ok(n) => {
                    debug!(
                        "Channel '{}' delivered {} notification(s) on retry",
                        name, n
                    );
                    n
                }
                Err(second_err) => {
                    warn!(
                        "Channel '{}' failed both attempts: {} / {}",
                        name, first_err, second_err
                    );
                    0
                }
            }
        }
    }
}

fn parse_severity(s: Option<&str>) -> Option<Severity> {
    match s {
        Some("warn") => Some(Severity::Warn),
        Some("crit") => Some(Severity::Crit),
        _ => None,
    }
}

/// True if `actual` meets the `minimum` threshold.
/// warn ≥ warn, crit ≥ warn, crit ≥ crit, warn < crit.
fn severity_gte(actual: Severity, minimum: Severity) -> bool {
    matches!(
        (actual, minimum),
        (Severity::Warn, Severity::Warn)
            | (Severity::Crit, Severity::Warn)
            | (Severity::Crit, Severity::Crit)
    )
}
