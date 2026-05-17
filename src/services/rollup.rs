//! Rollup task — populates coarser metric resolutions from finer ones.
//!
//! Behavior:
//! 1. Read `resolutions` to discover (parent → child) chains. The shape is
//!    purely data-driven, so a config change does not need a code change.
//! 2. For every (resource, child_resolution) target, pick up where we left
//!    off via `rollup_state.last_bucket_ts` and aggregate complete buckets
//!    that are now in the past.
//! 3. Use `INSERT OR REPLACE` keyed on the bucket timestamp so a re-run
//!    over the same window is idempotent.
//!
//! After extended downtime we deliberately clamp how far back we look
//! (`MAX_BACKFILL_BUCKETS`) — we'd rather lose old aggregates than spend
//! minutes back-filling on every restart.

use std::sync::Arc;
use std::time::Duration;

use log::{debug, warn};

use crate::state::AppState;
use crate::storage::repositories::{Resolution, ResolutionRepository, RollupStateRepository};

/// How many child buckets to back-fill in a single tick after a long gap.
/// 1h × 720 = 30 days; usually we only do 1-2 per tick.
const MAX_BACKFILL_BUCKETS: i64 = 720;

/// Resources that get full rollup coverage (per-bucket aggregation).
/// `cpu_cores` is intentionally omitted — per-core data is raw-only.
/// `probe` aggregates on (host, probe_name, metric_name, labels) so two
/// probes emitting the same metric_name with different labels stay
/// separate streams through every resolution.
const ROLLUP_RESOURCES: &[&str] = &[
    "cpu",
    "memory",
    "disk",
    "network",
    "docker",
    "pressure",
    "components",
    "probe",
];

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
        .rollup_tick_interval_ms
        .max(1000);
    let mut ticker = tokio::time::interval(Duration::from_millis(current_interval_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        if let Err(e) = run_once(&state).await {
            warn!("Rollup tick failed: {:?}", e);
        }

        let new_interval_ms = state
            .effective_config
            .read()
            .await
            .rollup_tick_interval_ms
            .max(1000);
        if new_interval_ms != current_interval_ms {
            current_interval_ms = new_interval_ms;
            let next = tokio::time::Instant::now() + Duration::from_millis(current_interval_ms);
            ticker = tokio::time::interval_at(next, Duration::from_millis(current_interval_ms));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        }
    }
}

async fn run_once(state: &AppState) -> anyhow::Result<()> {
    let res_repo = ResolutionRepository::new(state.db.clone());
    let cursor_repo = RollupStateRepository::new(state.db.clone());

    let targets = res_repo.list_rollup_targets().await?;
    if targets.is_empty() {
        return Ok(());
    }

    let now = chrono::Utc::now().timestamp();

    for target in &targets {
        for resource in ROLLUP_RESOURCES {
            if let Err(e) = rollup_resource(state, &cursor_repo, target, resource, now).await {
                warn!(
                    "Rollup failed for resource={} resolution={}: {:?}",
                    resource, target.name, e
                );
            }
        }
    }

    Ok(())
}

async fn rollup_resource(
    state: &AppState,
    cursor_repo: &RollupStateRepository,
    target: &Resolution,
    resource: &str,
    now: i64,
) -> anyhow::Result<()> {
    let parent = target
        .rollup_from
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("rollup target missing parent"))?;

    let bucket = target.interval_seconds;
    if bucket <= 0 {
        return Ok(());
    }

    // Latest fully-closed bucket that is now in the past.
    let latest_closed_start = (now / bucket - 1) * bucket;

    let cursor = cursor_repo.get(resource, &target.name).await?;
    let start_from = if cursor.last_bucket_ts == 0 {
        // First time: only roll up the most recent bucket; don't back-fill
        // the entire history of the parent table.
        latest_closed_start
    } else {
        cursor.last_bucket_ts + bucket
    };

    if start_from > latest_closed_start {
        return Ok(());
    }

    // Clamp the back-fill range so a restart after long downtime doesn't
    // try to rebuild months of aggregates in one tick.
    let max_start = latest_closed_start;
    let min_start = max_start.saturating_sub(MAX_BACKFILL_BUCKETS.saturating_mul(bucket));
    let mut bucket_start = start_from.max(min_start);

    let mut last_written = cursor.last_bucket_ts;

    while bucket_start <= latest_closed_start {
        let bucket_end = bucket_start + bucket;
        match aggregate_one_bucket(
            state,
            resource,
            parent,
            &target.name,
            bucket_start,
            bucket_end,
        )
        .await
        {
            Ok(true) => last_written = bucket_start,
            Ok(false) => {}
            Err(e) => {
                // Stop at the first error. If we kept going and a later
                // bucket succeeded, the cursor would advance past the
                // failed one and that bucket would never be retried. The
                // next tick picks up here and retries.
                warn!(
                    "Rollup bucket failed: resource={} resolution={} bucket_start={}: {:?} \
                     (stopping for this resource; will retry next tick)",
                    resource, target.name, bucket_start, e
                );
                break;
            }
        }
        bucket_start = bucket_end;
    }

    if last_written != cursor.last_bucket_ts {
        cursor_repo
            .set(resource, &target.name, last_written)
            .await?;
        debug!(
            "Rollup committed: resource={} resolution={} last_bucket_ts={}",
            resource, target.name, last_written
        );
    }

    Ok(())
}

/// Aggregate a single closed bucket `[start, end)` from the parent
/// resolution into the target resolution, using INSERT OR REPLACE so re-runs
/// are idempotent. Returns true if any rows were produced.
async fn aggregate_one_bucket(
    state: &AppState,
    resource: &str,
    parent: &str,
    target: &str,
    bucket_start: i64,
    bucket_end: i64,
) -> anyhow::Result<bool> {
    let sql = match resource {
        "cpu" => {
            r#"
            INSERT OR REPLACE INTO metrics_cpu
              (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m,
               steal_percent, iowait_percent, guest_percent,
               user_percent, system_percent,
               context_switches_per_sec, process_forks_per_sec)
            SELECT ?, ?,
                   AVG(usage_percent), AVG(load_1m), AVG(load_5m), AVG(load_15m),
                   AVG(steal_percent), AVG(iowait_percent), AVG(guest_percent),
                   AVG(user_percent), AVG(system_percent),
                   CAST(AVG(context_switches_per_sec) AS INTEGER),
                   CAST(AVG(process_forks_per_sec)    AS INTEGER)
              FROM metrics_cpu
             WHERE resolution = ?
               AND timestamp >= ?
               AND timestamp <  ?
            HAVING COUNT(*) > 0
            "#
        }
        "memory" => {
            r#"
            INSERT OR REPLACE INTO metrics_memory
              (resolution, timestamp,
               used_bytes, available_bytes, cached_bytes, swap_used_bytes,
               page_faults_minor_per_sec, page_faults_major_per_sec,
               swap_in_pages_per_sec, swap_out_pages_per_sec)
            SELECT ?, ?,
                   CAST(AVG(used_bytes) AS INTEGER),
                   CAST(AVG(available_bytes) AS INTEGER),
                   CAST(AVG(cached_bytes) AS INTEGER),
                   CAST(AVG(swap_used_bytes) AS INTEGER),
                   CAST(AVG(page_faults_minor_per_sec) AS INTEGER),
                   CAST(AVG(page_faults_major_per_sec) AS INTEGER),
                   CAST(AVG(swap_in_pages_per_sec)     AS INTEGER),
                   CAST(AVG(swap_out_pages_per_sec)    AS INTEGER)
              FROM metrics_memory
             WHERE resolution = ? AND timestamp >= ? AND timestamp < ?
            HAVING COUNT(*) > 0
            "#
        }
        "disk" => {
            r#"
            INSERT OR REPLACE INTO metrics_disk
              (resolution, timestamp, mount_point,
               used_bytes, available_bytes, read_bytes_per_sec, write_bytes_per_sec,
               inode_used_percent, read_iops, write_iops, io_util_percent)
            SELECT ?, ?, mount_point,
                   CAST(AVG(used_bytes) AS INTEGER),
                   CAST(AVG(available_bytes) AS INTEGER),
                   CAST(AVG(read_bytes_per_sec) AS INTEGER),
                   CAST(AVG(write_bytes_per_sec) AS INTEGER),
                   AVG(inode_used_percent),
                   CAST(AVG(read_iops) AS INTEGER),
                   CAST(AVG(write_iops) AS INTEGER),
                   AVG(io_util_percent)
              FROM metrics_disk
             WHERE resolution = ? AND timestamp >= ? AND timestamp < ?
             GROUP BY mount_point
            "#
        }
        "network" => {
            r#"
            INSERT OR REPLACE INTO metrics_network
              (resolution, timestamp, interface_name,
               rx_bytes_per_sec, tx_bytes_per_sec,
               rx_packets_per_sec, tx_packets_per_sec,
               errors_in_per_sec, errors_out_per_sec)
            SELECT ?, ?, interface_name,
                   CAST(AVG(rx_bytes_per_sec)   AS INTEGER),
                   CAST(AVG(tx_bytes_per_sec)   AS INTEGER),
                   CAST(AVG(rx_packets_per_sec) AS INTEGER),
                   CAST(AVG(tx_packets_per_sec) AS INTEGER),
                   CAST(AVG(errors_in_per_sec)  AS INTEGER),
                   CAST(AVG(errors_out_per_sec) AS INTEGER)
              FROM metrics_network
             WHERE resolution = ? AND timestamp >= ? AND timestamp < ?
             GROUP BY interface_name
            "#
        }
        "docker" => {
            r#"
            INSERT OR REPLACE INTO metrics_docker
              (resolution, timestamp, container_id,
               cpu_percent, memory_used_bytes, memory_limit_bytes,
               network_rx_bytes, network_tx_bytes,
               block_read_bytes, block_write_bytes, pids)
            SELECT ?, ?, container_id,
                   AVG(cpu_percent),
                   CAST(AVG(memory_used_bytes)  AS INTEGER),
                   CAST(AVG(memory_limit_bytes) AS INTEGER),
                   CAST(AVG(network_rx_bytes)   AS INTEGER),
                   CAST(AVG(network_tx_bytes)   AS INTEGER),
                   CAST(AVG(block_read_bytes)   AS INTEGER),
                   CAST(AVG(block_write_bytes)  AS INTEGER),
                   CAST(AVG(pids)               AS INTEGER)
              FROM metrics_docker
             WHERE resolution = ? AND timestamp >= ? AND timestamp < ?
             GROUP BY container_id
            "#
        }
        "pressure" => {
            r#"
            INSERT OR REPLACE INTO metrics_pressure
              (resolution, timestamp, resource,
               some_avg10, some_avg60, some_avg300,
               full_avg10, full_avg60, full_avg300)
            SELECT ?, ?, resource,
                   AVG(some_avg10), AVG(some_avg60), AVG(some_avg300),
                   AVG(full_avg10), AVG(full_avg60), AVG(full_avg300)
              FROM metrics_pressure
             WHERE resolution = ? AND timestamp >= ? AND timestamp < ?
             GROUP BY resource
            "#
        }
        "components" => {
            r#"
            INSERT OR REPLACE INTO metrics_components
              (resolution, timestamp, label,
               temperature_c, max_c, critical_c)
            SELECT ?, ?, label,
                   AVG(temperature_c), AVG(max_c), AVG(critical_c)
              FROM metrics_components
             WHERE resolution = ? AND timestamp >= ? AND timestamp < ?
             GROUP BY label
            "#
        }
        "probe" => {
            r#"
            INSERT OR REPLACE INTO metrics_probe
              (resolution, timestamp, probe_name, metric_name, labels, value)
            SELECT ?, ?, probe_name, metric_name, labels,
                   AVG(value)
              FROM metrics_probe
             WHERE resolution = ? AND timestamp >= ? AND timestamp < ?
             GROUP BY probe_name, metric_name, labels
            "#
        }
        _ => return Ok(false),
    };

    let result = sqlx::query(sql)
        .bind(target)
        .bind(bucket_start)
        .bind(parent)
        .bind(bucket_start)
        .bind(bucket_end)
        .execute(&state.db)
        .await?;

    Ok(result.rows_affected() > 0)
}
