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

/// Child buckets one tick will aggregate before leaving the rest to the next.
/// A per-tick budget, not a reach limit: the same count spans 30 days at 1h but
/// 12 hours at 1m, so clamping how far back a tick looks would skip buckets the
/// retained raw could still fill.
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

/// Raw samples a row stands for. Raw rows carry NULL — they are one sample and
/// predate the column — so every read of it goes through this.
const SAMPLES: &str = "COALESCE(sample_count, 1)";

/// What a column means once several child buckets are folded into one.
#[derive(Clone, Copy, PartialEq)]
enum Agg {
    /// Weighted mean, kept as REAL.
    Mean,
    /// Weighted mean, stored back into an INTEGER column.
    MeanInt,
    /// The bucket's end value — for counters cumulative since the producer
    /// started, where a mean is a number nothing ever reported.
    Max,
}

/// One rolled-up column and how it folds.
type Field = (&'static str, Agg);

/// Fold `col` across child buckets, weighting each by the samples behind it.
///
/// `AVG(col)` averages the children's averages, which equals the true mean only
/// when every child covered the same number of samples. They do not: a restart,
/// a stalled collector, or a bucket only partly filled when the tier ran all
/// leave a child standing for fewer samples than its siblings, and chaining
/// raw→1m→5m→1h compounds the error at every tier. The weighting cannot be
/// recovered after the fact, which is the whole reason `sample_count` is stored.
///
/// The denominator counts only children where `col` is non-NULL, so an optional
/// field is not diluted by buckets that never carried it.
fn fold(col: &str, agg: Agg) -> String {
    if agg == Agg::Max {
        return format!("MAX({col})");
    }
    let mean = format!(
        "SUM({col} * {SAMPLES}) / \
         NULLIF(SUM(CASE WHEN {col} IS NULL THEN 0 ELSE {SAMPLES} END), 0)"
    );
    if agg == Agg::MeanInt {
        format!("CAST({mean} AS INTEGER)")
    } else {
        mean
    }
}

/// The `INSERT … SELECT` that folds one bucket of `table` into `target`.
///
/// Placeholder order is (target resolution, bucket start, parent resolution,
/// bucket start, bucket end) — the binds below depend on it.
///
/// Unkeyed tables need `HAVING COUNT(*) > 0` so an empty bucket does not insert
/// a row of NULLs; a keyed table's `GROUP BY` already yields nothing.
fn fold_sql(table: &str, key: Option<&str>, fields: &[Field]) -> String {
    if matches!(
        table,
        "metrics_cpu"
            | "metrics_memory"
            | "metrics_disk"
            | "metrics_network"
            | "metrics_network_total"
    ) {
        return fold_gauge_sql(table, key, fields);
    }
    let cols: Vec<&str> = fields.iter().map(|(c, _)| *c).collect();
    let aggs: Vec<String> = fields.iter().map(|(c, a)| fold(c, *a)).collect();
    let (key_col, tail) = match key {
        Some(k) => (format!("{k}, "), format!("GROUP BY {k}")),
        None => (String::new(), "HAVING COUNT(*) > 0".to_string()),
    };
    format!(
        "INSERT OR REPLACE INTO {table}
           (resolution, timestamp, {key_col}{cols}, sample_count)
         SELECT ?, ?, {key_sel}{aggs}, SUM({SAMPLES})
           FROM {table}
          WHERE resolution = ? AND timestamp >= ? AND timestamp < ?
         {tail}",
        cols = cols.join(", "),
        key_sel = key_col,
        aggs = aggs.join(", "),
    )
}

/// Preserve the aggregate state, not rounded child means. Legacy/mixed buckets
/// retain the previous mean semantics but cannot claim observed extrema.
fn fold_gauge_sql(table: &str, key: Option<&str>, fields: &[Field]) -> String {
    let complete = "MIN(CASE WHEN resolution = 'raw' OR summary_version = 1 THEN 1 ELSE 0 END) = 1";
    let mut cols = Vec::new();
    let mut values = Vec::new();
    let (key_col, tail) = match key {
        Some(k) => (format!("{k}, "), format!("GROUP BY {k}")),
        None => (String::new(), "HAVING COUNT(*) > 0".to_string()),
    };
    for &(col, agg) in fields {
        let raw = match (table, col) {
            ("metrics_memory", "used_percent") => {
                "100.0 * MAX(total_bytes - available_bytes, 0) / NULLIF(total_bytes, 0)"
            }
            ("metrics_disk", "used_percent") => "100.0 * used_bytes / NULLIF(total_bytes, 0)",
            _ => col,
        };
        let sum = format!(
            "SUM(CASE WHEN resolution = 'raw' THEN COALESCE(1.0 * ({raw}), 0.0) ELSE {col}_sum END)"
        );
        let count = format!(
            "SUM(CASE WHEN resolution = 'raw' THEN CASE WHEN ({raw}) IS NULL THEN 0 ELSE 1 END ELSE {col}_valid_count END)"
        );
        let mean = format!("1.0 * {sum} / NULLIF({count}, 0)");
        let visible = if agg == Agg::MeanInt {
            format!("CAST({mean} AS INTEGER)")
        } else {
            mean
        };
        cols.push(col.to_string());
        values.push(format!(
            "CASE WHEN {complete} THEN {visible} ELSE {} END",
            fold(col, agg)
        ));
        for (suffix, expr) in [
            (
                "min",
                format!("MIN(CASE WHEN resolution = 'raw' THEN ({raw}) ELSE {col}_min END)"),
            ),
            (
                "max",
                format!("MAX(CASE WHEN resolution = 'raw' THEN ({raw}) ELSE {col}_max END)"),
            ),
            ("sum", sum),
            ("valid_count", count),
        ] {
            cols.push(format!("{col}_{suffix}"));
            values.push(format!("CASE WHEN {complete} THEN {expr} END"));
        }
    }
    format!("INSERT OR REPLACE INTO {table} (resolution, timestamp, {key_col}{}, sample_count, summary_version)
        SELECT ?, ?, {key_col}{}, SUM({SAMPLES}), CASE WHEN {complete} THEN 1 END
        FROM {table} WHERE resolution = ? AND timestamp >= ? AND timestamp < ? {tail}",
        cols.join(", "), values.join(", "))
}

pub fn spawn(state: Arc<AppState>) {
    crate::supervision::supervise(
        "rollup pass",
        tokio::spawn(async move {
            run(state).await;
        }),
    );
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
    use Agg::{Max, Mean, MeanInt};

    let sql = match resource {
        "cpu" => fold_sql(
            "metrics_cpu",
            None,
            &[
                ("usage_percent", Mean),
                ("load_1m", Mean),
                ("load_5m", Mean),
                ("load_15m", Mean),
                ("steal_percent", Mean),
                ("iowait_percent", Mean),
                ("guest_percent", Mean),
                ("user_percent", Mean),
                ("system_percent", Mean),
                ("context_switches_per_sec", MeanInt),
                ("process_forks_per_sec", MeanInt),
            ],
        ),
        "memory" => fold_sql(
            "metrics_memory",
            None,
            &[
                ("total_bytes", MeanInt),
                ("used_bytes", MeanInt),
                ("available_bytes", MeanInt),
                ("cached_bytes", MeanInt),
                ("swap_used_bytes", MeanInt),
                ("page_faults_minor_per_sec", MeanInt),
                ("page_faults_major_per_sec", MeanInt),
                ("swap_in_pages_per_sec", MeanInt),
                ("swap_out_pages_per_sec", MeanInt),
                ("used_percent", Mean),
            ],
        ),
        "disk" => fold_sql(
            "metrics_disk",
            Some("mount_point"),
            &[
                ("total_bytes", MeanInt),
                ("used_bytes", MeanInt),
                ("available_bytes", MeanInt),
                ("read_bytes_per_sec", MeanInt),
                ("write_bytes_per_sec", MeanInt),
                ("inode_used_percent", Mean),
                ("read_iops", MeanInt),
                ("write_iops", MeanInt),
                ("io_util_percent", Mean),
                ("used_percent", Mean),
            ],
        ),
        "network" => fold_sql(
            "metrics_network",
            Some("interface_name"),
            &[
                ("rx_bytes_per_sec", MeanInt),
                ("tx_bytes_per_sec", MeanInt),
                ("rx_packets_per_sec", MeanInt),
                ("tx_packets_per_sec", MeanInt),
                ("errors_in_per_sec", MeanInt),
                ("errors_out_per_sec", MeanInt),
            ],
        ),
        // The four byte columns are Docker's own counters, cumulative since the
        // container started — the collector stores `networks.*.rx_bytes` and the
        // blkio totals as read. Averaging a monotonic counter produces a number
        // the container never reported, somewhere between the bucket's first and
        // last reading, and every coarser tier then averages that again. MAX is
        // the bucket's end value, and the one reading worth keeping when a
        // restart resets the counter mid-bucket. The rest are gauges (memory,
        // pids) or an already-derived rate (cpu_percent), where a mean is right.
        "docker" => fold_sql(
            "metrics_docker",
            Some("container_id"),
            &[
                ("cpu_percent", Mean),
                ("memory_used_bytes", MeanInt),
                ("memory_limit_bytes", MeanInt),
                ("network_rx_bytes", Max),
                ("network_tx_bytes", Max),
                ("block_read_bytes", Max),
                ("block_write_bytes", Max),
                ("pids", MeanInt),
            ],
        ),
        "process" => fold_sql(
            "metrics_process",
            Some("name"),
            &[
                ("pid_count", MeanInt),
                ("cpu_percent", Mean),
                ("memory_bytes", MeanInt),
                ("disk_read_bps", MeanInt),
                ("disk_write_bps", MeanInt),
            ],
        ),
        "pressure" => fold_sql(
            "metrics_pressure",
            Some("resource"),
            &[
                ("some_avg10", Mean),
                ("some_avg60", Mean),
                ("some_avg300", Mean),
                ("full_avg10", Mean),
                ("full_avg60", Mean),
                ("full_avg300", Mean),
            ],
        ),
        "components" => fold_sql(
            "metrics_components",
            Some("label"),
            &[
                ("temperature_c", Mean),
                ("max_c", Mean),
                ("critical_c", Mean),
            ],
        ),
        _ => return Ok(false),
    };
    let sql = sqlx::AssertSqlSafe(sql);

    let result = sqlx::query(sql)
        .bind(target)
        .bind(bucket_start)
        .bind(parent)
        .bind(bucket_start)
        .bind(bucket_end)
        .execute(&state.db)
        .await?;

    if resource == "network" {
        let totals = fold_sql(
            "metrics_network_total",
            None,
            &[
                ("rx_bytes_per_sec", MeanInt),
                ("tx_bytes_per_sec", MeanInt),
                ("rx_packets_per_sec", MeanInt),
                ("tx_packets_per_sec", MeanInt),
                ("errors_in_per_sec", MeanInt),
                ("errors_out_per_sec", MeanInt),
            ],
        );
        sqlx::query(sqlx::AssertSqlSafe(totals))
            .bind(target)
            .bind(bucket_start)
            .bind(parent)
            .bind(bucket_start)
            .bind(bucket_end)
            .execute(&state.db)
            .await?;
    }
    Ok(result.rows_affected() > 0)
}
