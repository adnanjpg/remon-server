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
//! After extended downtime we bound how much a single tick will aggregate
//! (`MAX_BACKFILL_BUCKETS`) rather than spending minutes back-filling on the
//! first tick after a restart. What is left over waits for the next tick; the
//! cursor never steps past a bucket it did not aggregate.

use std::sync::Arc;
use std::time::Duration;

use log::{debug, warn};

use crate::state::AppState;
use crate::storage::repositories::{Resolution, ResolutionRepository, RollupStateRepository};

/// How many child buckets one tick will aggregate before leaving the rest to
/// the next one. Usually 1-2 are due; this bounds a tick that finds a long gap.
///
/// A budget, not a horizon. It used to clamp how far *back* a tick would reach,
/// and the buckets older than the clamp were not deferred but skipped — the
/// cursor resumed ahead of them and nothing ever came back. 720 buckets is 30
/// days at 1h, which is where the "30 days" reading came from, but only 12
/// hours at 1m, and `raw` — what the 1m tier is built from — is kept for a day.
/// So a gap between 12 and 24 hours punched a permanent hole in the 1m tier
/// with the raw rows to fill it sitting right there. Stopping where the budget
/// runs out and resuming there next tick costs several ticks to catch up and
/// loses nothing.
const MAX_BACKFILL_BUCKETS: i64 = 720;

/// Resources that get full rollup coverage (per-bucket aggregation).
///
/// `cpu_cores` is intentionally omitted — per-core data is raw-only.
///
/// `probe` is omitted too, for a different reason: nothing can read it.
/// `GET /probes/{name}/metrics/{metric}/history` rejects any resolution but
/// `raw`, and the assistant's `history_summary` has no `probe` namespace, so
/// rolled-up probe buckets were written, retained and indexed with no path
/// back out. Reinstating it means adding the read side in the same change.
const ROLLUP_RESOURCES: &[&str] = &[
    "cpu",
    "memory",
    "disk",
    "network",
    "docker",
    "process",
    "pressure",
    "components",
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
    let mut shutdown = state.shutdown.subscribe();

    while crate::shutdown::tick_or_stop(&mut ticker, &mut shutdown).await {
        if let Err(e) = run_once(&state).await {
            warn!("rollup tick failed: {:?}", e);
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

/// One full sweep: every rollup target × every resource. Visible to the crate
/// so the measurement suite can drive a tick directly instead of waiting on
/// the interval.
pub(crate) async fn run_once(state: &AppState) -> anyhow::Result<()> {
    let res_repo = ResolutionRepository::new(state.db.clone());
    let cursor_repo = RollupStateRepository::new(state.db.clone());

    let targets = res_repo.list_rollup_targets().await?;
    if targets.is_empty() {
        return Ok(());
    }

    let now = chrono::Utc::now().timestamp();
    // Read once, then kept current as cursors advance: a child tier reads its
    // parent's cursor, and the parent may have moved earlier in this same tick.
    let mut cursors = cursor_repo.load_all().await?;

    for target in &targets {
        for resource in ROLLUP_RESOURCES {
            if let Err(e) =
                rollup_resource(state, &cursor_repo, &mut cursors, target, resource, now).await
            {
                warn!(
                    "rollup failed for resource={} resolution={}: {:?}",
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
    cursors: &mut std::collections::HashMap<(String, String), i64>,
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

    let key = (resource.to_string(), target.name.clone());
    let cursor_at = cursors.get(&key).copied().unwrap_or(0);
    let start_from = if cursor_at == 0 {
        // First time: only roll up the most recent bucket; don't back-fill
        // the entire history of the parent table.
        latest_closed_start
    } else {
        cursor_at + bucket
    };

    if start_from > latest_closed_start {
        return Ok(());
    }

    let mut bucket_start = start_from;

    // The end of what the parent has finished writing. A bucket is only ready
    // to aggregate once the parent has covered all of it — rows present are not
    // the same as a complete range, and averaging a window the parent is still
    // filling freezes a partial value the cursor then moves past.
    //
    // `raw` is written live by the collectors, so every closed bucket is
    // complete. A rolled parent is trusted only to the start of its own last
    // written bucket, which trails by one parent bucket and never overstates.
    let parent_settled_through = if parent == "raw" {
        latest_closed_start + bucket
    } else {
        cursors
            .get(&(resource.to_string(), parent.to_string()))
            .copied()
            .unwrap_or(0)
    };

    let mut commit_through = cursor_at;
    let mut wrote_any = false;

    // Bounds the work of one tick without bounding how far back the cursor can
    // eventually reach: whatever is left is still ahead of `commit_through`,
    // and the next tick starts there.
    let mut budget = MAX_BACKFILL_BUCKETS;

    while bucket_start <= latest_closed_start && budget > 0 {
        let bucket_end = bucket_start + bucket;
        // The parent fills ascending, so the first bucket it has not covered
        // means none above it is ready either.
        if bucket_end > parent_settled_through {
            break;
        }
        budget -= 1;
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
            // Complete either way: an empty closed bucket stays empty, and
            // parking the cursor on one puts the sweep on a treadmill.
            Ok(wrote) => {
                wrote_any |= wrote;
                commit_through = bucket_start;
            }
            Err(e) => {
                // Stop at the first error. If we kept going and a later
                // bucket succeeded, the cursor would advance past the
                // failed one and that bucket would never be retried. The
                // next tick picks up here and retries.
                warn!(
                    "rollup bucket failed: resource={} resolution={} bucket_start={}: {:?} \
                     (stopping for this resource; will retry next tick)",
                    resource, target.name, bucket_start, e
                );
                break;
            }
        }
        bucket_start = bucket_end;
    }

    if commit_through != cursor_at {
        cursor_repo
            .set(resource, &target.name, commit_through)
            .await?;
        cursors.insert(key, commit_through);
        debug!(
            "rollup committed: resource={} resolution={} last_bucket_ts={} wrote_rows={}",
            resource, target.name, commit_through, wrote_any
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
               total_bytes, used_bytes, available_bytes, cached_bytes, swap_used_bytes,
               page_faults_minor_per_sec, page_faults_major_per_sec,
               swap_in_pages_per_sec, swap_out_pages_per_sec)
            SELECT ?, ?,
                   CAST(AVG(total_bytes) AS INTEGER),
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
               total_bytes, used_bytes, available_bytes, read_bytes_per_sec, write_bytes_per_sec,
               inode_used_percent, read_iops, write_iops, io_util_percent)
            SELECT ?, ?, mount_point,
                   CAST(AVG(total_bytes) AS INTEGER),
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
        // The four byte columns are Docker's own counters, cumulative since the
        // container started — the collector stores `networks.*.rx_bytes` and the
        // blkio totals as read. Averaging a monotonic counter produces a number
        // the container never reported, somewhere between the bucket's first and
        // last reading, and every coarser tier then averages that again. MAX is
        // the bucket's end value, and the one reading worth keeping when a
        // restart resets the counter mid-bucket. The rest are gauges (memory,
        // pids) or an already-derived rate (cpu_percent), where AVG is right.
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
                   MAX(network_rx_bytes),
                   MAX(network_tx_bytes),
                   MAX(block_read_bytes),
                   MAX(block_write_bytes),
                   CAST(AVG(pids)               AS INTEGER)
              FROM metrics_docker
             WHERE resolution = ? AND timestamp >= ? AND timestamp < ?
             GROUP BY container_id
            "#
        }
        "process" => {
            r#"
            INSERT OR REPLACE INTO metrics_process
              (resolution, timestamp, name,
               pid_count, cpu_percent, memory_bytes, disk_read_bps, disk_write_bps)
            SELECT ?, ?, name,
                   CAST(AVG(pid_count) AS INTEGER),
                   AVG(cpu_percent),
                   CAST(AVG(memory_bytes)   AS INTEGER),
                   CAST(AVG(disk_read_bps)  AS INTEGER),
                   CAST(AVG(disk_write_bps) AS INTEGER)
              FROM metrics_process
             WHERE resolution = ? AND timestamp >= ? AND timestamp < ?
             GROUP BY name
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
