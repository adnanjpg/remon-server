pub mod channel;
pub mod channels;
pub mod types;

pub use types::{Notification, NotificationEvent, Severity};

use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use sqlx::SqlitePool;
use tokio::sync::RwLock;

use crate::config::NotificationsConfig;
use crate::notify::channel::NotificationChannel;
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

        let mut slots = Vec::new();
        for row in rows {
            let config: serde_json::Value =
                serde_json::from_str(&row.config).unwrap_or_default();

            match channels::build_channel(
                &row.r#type,
                &config,
                &self.credentials,
                self.http.clone(),
                self.pool.clone(),
                &self.vapid,
            ) {
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
                        .map_or(true, |min| severity_gte(notification.severity, min))
                })
                .map(|s| (s.name.clone(), Arc::clone(&s.inner)))
                .collect()
        };

        if targets.is_empty() {
            debug!("fanout: no channels for severity {:?}", notification.severity);
            return 0;
        }

        let mut join_set = tokio::task::JoinSet::new();

        for (name, channel) in targets {
            let notif = notification.clone();
            join_set.spawn(async move {
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    channel.send(&notif),
                )
                .await
                {
                    Ok(Ok(n)) => {
                        debug!("Channel '{}' delivered {} notification(s)", name, n);
                        n
                    }
                    Ok(Err(e)) => {
                        warn!("Channel '{}' error: {}", name, e);
                        0
                    }
                    Err(_) => {
                        warn!("Channel '{}' timed out after 10s", name);
                        0
                    }
                }
            });
        }

        let mut total = 0usize;
        while let Some(res) = join_set.join_next().await {
            total += res.unwrap_or(0);
        }
        total
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

        tokio::time::timeout(Duration::from_secs(10), channel.send(&test_notif))
            .await
            .map_err(|_| "channel timed out".to_string())?
            .map_err(|e| e.to_string())
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
