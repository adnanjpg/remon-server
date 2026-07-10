//! Retention task — deletes rows older than the per-bucket policy.
//!
//! Reads `retention_policy` every tick; the table is the source of truth
//! and can be updated from the UI without restarting. Deletions for each
//! (resource, resolution) pair run sequentially — a hot path could be
//! tightened later, but this runs once per `retention_tick_interval_ms`
//! (default 1 hour) so it's not in the critical path.

use std::sync::Arc;
use std::time::Duration;

use log::{debug, warn};

use crate::state::AppState;
use crate::storage::repositories::{MetricsRepository, RetentionRepository};

pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        run(state).await;
    });
}

async fn run(state: Arc<AppState>) {
    let mut current_interval_ms = state
        .effective_config
        .read()
        .await
        .retention_tick_interval_ms
        .max(60_000);
    let mut ticker = tokio::time::interval(Duration::from_millis(current_interval_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        if let Err(e) = run_once(&state).await {
            warn!("retention tick failed: {:?}", e);
        }

        let new_interval_ms = state
            .effective_config
            .read()
            .await
            .retention_tick_interval_ms
            .max(60_000);
        if new_interval_ms != current_interval_ms {
            current_interval_ms = new_interval_ms;
            let next = tokio::time::Instant::now() + Duration::from_millis(current_interval_ms);
            ticker = tokio::time::interval_at(next, Duration::from_millis(current_interval_ms));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        }
    }
}

async fn run_once(state: &AppState) -> anyhow::Result<()> {
    let policy_repo = RetentionRepository::new(state.db.clone());
    let metrics_repo = MetricsRepository::new(state.db.clone());

    let policies = policy_repo.list_all().await?;
    let now = chrono::Utc::now().timestamp();
    let mut total_deleted: u64 = 0;

    for p in &policies {
        let cutoff = now - p.keep_seconds;
        match metrics_repo
            .delete_older_than(&p.resource, &p.resolution, cutoff)
            .await
        {
            Ok(n) => {
                total_deleted += n;
                if n > 0 {
                    debug!(
                        "retention: resource={} resolution={} deleted={} cutoff={}",
                        p.resource, p.resolution, n, cutoff
                    );
                }
            }
            Err(e) => warn!(
                "retention DELETE failed for resource={} resolution={}: {:?}",
                p.resource, p.resolution, e
            ),
        }
    }

    if total_deleted > 0 {
        debug!("retention tick: total rows deleted = {}", total_deleted);
    }

    Ok(())
}
