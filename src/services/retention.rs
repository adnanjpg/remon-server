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
    loop {
        let tick_ms = {
            let cfg = state.effective_config.read().await;
            cfg.retention_tick_interval_ms.max(60_000)
        };
        tokio::time::sleep(Duration::from_millis(tick_ms)).await;

        if let Err(e) = run_once(&state).await {
            warn!("Retention tick failed: {:?}", e);
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
                        "Retention: resource={} resolution={} deleted={} cutoff={}",
                        p.resource, p.resolution, n, cutoff
                    );
                }
            }
            Err(e) => warn!(
                "Retention DELETE failed for resource={} resolution={}: {:?}",
                p.resource, p.resolution, e
            ),
        }
    }

    if total_deleted > 0 {
        debug!("Retention tick: total rows deleted = {}", total_deleted);
    }

    Ok(())
}
