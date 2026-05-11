use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use log::{info, warn};

use crate::notify::{Notification, NotificationEvent, Severity};
use crate::platform::services::{ServiceFilter, ServiceState};
use crate::state::AppState;

const WATCHER_TICK_SECS: u64 = 60;

pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(run(state));
}

async fn run(state: Arc<AppState>) {
    let mut known_failed: HashSet<String> = HashSet::new();
    let mut interval = tokio::time::interval(Duration::from_secs(WATCHER_TICK_SECS));
    interval.tick().await; // skip immediate first tick so boot noise is ignored

    loop {
        interval.tick().await;
        if let Err(e) = tick(&state, &mut known_failed).await {
            warn!("Service watcher tick error: {:?}", e);
        }
    }
}

async fn tick(state: &AppState, known_failed: &mut HashSet<String>) -> anyhow::Result<()> {
    let filter = ServiceFilter {
        state: Some(ServiceState::Failed),
    };

    let failed_services = match state.service_manager.list(filter).await {
        Ok(s) => s,
        Err(e) => {
            // NotSupported is expected on platforms without an impl — don't spam logs.
            use crate::platform::services::ServiceError;
            if !matches!(e, ServiceError::NotSupported) {
                warn!("Service watcher: list failed: {}", e);
            }
            return Ok(());
        }
    };

    let failed_names: HashSet<String> = failed_services.iter().map(|s| s.name.clone()).collect();

    // Notify newly-failed services (not seen in the previous tick).
    for svc in &failed_services {
        if !known_failed.contains(&svc.name) {
            let n = Notification {
                title: format!("Service failed: {}", svc.name),
                body: svc
                    .description
                    .as_deref()
                    .unwrap_or("No description available")
                    .to_string(),
                severity: Severity::Crit,
                event: NotificationEvent::Fired,
            };
            let sent = state.notify.fanout(&n).await;
            info!(
                "Service '{}' entered failed state; delivered to {} channel(s)",
                svc.name, sent
            );
        }
    }

    // Evict recovered services so they can be re-notified if they fail again.
    known_failed.retain(|name| failed_names.contains(name));
    for name in &failed_names {
        known_failed.insert(name.clone());
    }

    Ok(())
}
