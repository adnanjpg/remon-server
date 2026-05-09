//! Time-series history endpoints.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::routes::dtos::metrics::{
    ComponentPoint, ComponentsHistoryResponse, CpuCorePoint, CpuCoresHistoryResponse,
    CpuHistoryResponse, CpuPoint, DiskHistoryResponse, DiskPoint, MemoryHistoryResponse,
    MemoryPoint, MetricsRangeQuery, NetworkHistoryResponse, NetworkPoint,
    PressureHistoryResponse, PressurePoint,
};
use crate::routes::extractors::Claims;
use crate::state::AppState;
use crate::storage::repositories::MetricsRepository;

/// Default span when client omits start/end: last hour.
const DEFAULT_SPAN_SECS: i64 = 3600;
/// Default points per response when client omits `limit`. Sized to fit
/// the worst case for each resolution band's typical use:
/// - raw   @2s × 2h  = 3 600 points
/// - 1m    × 24h    = 1 440 points
/// - 5m    × 7d     = 2 016 points
/// - 1h    × ~6 mo  = 4 320 points
/// Anything larger than 5 000 forces the user to either narrow the range
/// or override `?limit=` — the cap below stays in place to keep a single
/// rogue client from materialising the whole table at once.
const DEFAULT_LIMIT: u32 = 5000;
/// Hard cap regardless of `limit` query param.
const MAX_LIMIT: u32 = 5000;
/// Resolutions accepted as override values.
const KNOWN_RESOLUTIONS: &[&str] = &["raw", "1m", "5m", "1h"];

/// Pick the coarsest resolution that still gives reasonable detail for the
/// span. Raw 2-second samples capture every kernel context switch, which
/// reads as visual zigzag once the chart is wider than ~30 minutes — the
/// 1-minute rollup buckets average 30 raw samples each, smoothing the
/// per-task spikes while keeping real trends. Operators who want the
/// uncooked stream can still pass `?resolution=raw` to override.
fn pick_resolution(span_secs: i64) -> &'static str {
    if span_secs <= 1800 {
        "raw" //   ≤ 30 min
    } else if span_secs <= 86400 {
        "1m" //    ≤ 1 day
    } else if span_secs <= 604800 {
        "5m" //    ≤ 7 days
    } else {
        "1h"
    }
}

/// Resolve (start, end, resolution, limit) from the optional query params.
/// Validates `resolution` against the known set.
fn resolve_range(q: &MetricsRangeQuery) -> AppResult<(i64, i64, String, u32)> {
    let now = chrono::Utc::now().timestamp();
    let end = q.end.unwrap_or(now);
    let start = q.start.unwrap_or(end - DEFAULT_SPAN_SECS);

    if end < start {
        return Err(AppError::BadRequest(
            "end must be >= start".to_string(),
        ));
    }

    let resolution = match q.resolution.as_deref() {
        Some(r) if KNOWN_RESOLUTIONS.contains(&r) => r.to_string(),
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "unknown resolution '{}'; expected one of {:?}",
                other, KNOWN_RESOLUTIONS
            )));
        }
        None => pick_resolution(end - start).to_string(),
    };

    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    Ok((start, end, resolution, limit))
}

/// GET /metrics/cpu — host-level CPU usage and load average.
pub async fn cpu_history(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Query(q): Query<MetricsRangeQuery>,
) -> AppResult<Json<CpuHistoryResponse>> {
    let (start, end, resolution, limit) = resolve_range(&q)?;
    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo
        .read_cpu(&resolution, start, end, limit)
        .await?;

    let points: Vec<CpuPoint> = rows
        .into_iter()
        .map(
            |(ts, usage, l1, l5, l15, steal, iowait, guest, ctxt, forks)| CpuPoint {
                timestamp: ts,
                usage_percent: usage,
                load_1m: l1,
                load_5m: l5,
                load_15m: l15,
                steal_percent: steal,
                iowait_percent: iowait,
                guest_percent: guest,
                context_switches_per_sec: ctxt,
                process_forks_per_sec: forks,
            },
        )
        .collect();

    Ok(Json(CpuHistoryResponse {
        resolution,
        points,
    }))
}

/// GET /metrics/cpu/cores — per-core usage. Always raw (the per-core
/// breakdown is not rolled up; aggregates use host-level metrics_cpu).
pub async fn cpu_cores_history(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Query(q): Query<MetricsRangeQuery>,
) -> AppResult<Json<CpuCoresHistoryResponse>> {
    let (start, end, _resolution, limit) = resolve_range(&q)?;
    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo
        .read_cpu_cores(start, end, limit)
        .await?;

    let points: Vec<CpuCorePoint> = rows
        .into_iter()
        .map(|(ts, idx, usage, freq)| CpuCorePoint {
            timestamp: ts,
            core_index: idx,
            usage_percent: usage,
            freq_mhz: freq,
        })
        .collect();

    Ok(Json(CpuCoresHistoryResponse {
        resolution: "raw".to_string(),
        points,
    }))
}

/// GET /metrics/memory
pub async fn memory_history(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Query(q): Query<MetricsRangeQuery>,
) -> AppResult<Json<MemoryHistoryResponse>> {
    let (start, end, resolution, limit) = resolve_range(&q)?;
    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo
        .read_memory(&resolution, start, end, limit)
        .await?;

    let points: Vec<MemoryPoint> = rows
        .into_iter()
        .map(
            |(ts, used, avail, cached, swap, pf_min, pf_maj, sw_in, sw_out)| MemoryPoint {
                timestamp: ts,
                used_bytes: used,
                available_bytes: avail,
                cached_bytes: cached,
                swap_used_bytes: swap,
                page_faults_minor_per_sec: pf_min,
                page_faults_major_per_sec: pf_maj,
                swap_in_pages_per_sec: sw_in,
                swap_out_pages_per_sec: sw_out,
            },
        )
        .collect();

    Ok(Json(MemoryHistoryResponse {
        resolution,
        points,
    }))
}

/// GET /metrics/disk — points per (timestamp, mount_point); client groups
/// by mount_point if it wants per-volume series.
pub async fn disk_history(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Query(q): Query<MetricsRangeQuery>,
) -> AppResult<Json<DiskHistoryResponse>> {
    let (start, end, resolution, limit) = resolve_range(&q)?;
    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo
        .read_disk(&resolution, start, end, limit)
        .await?;

    let points: Vec<DiskPoint> = rows
        .into_iter()
        .map(|(ts, mp, used, avail, rbps, wbps, inode)| DiskPoint {
            timestamp: ts,
            mount_point: mp,
            used_bytes: used,
            available_bytes: avail,
            read_bytes_per_sec: rbps,
            write_bytes_per_sec: wbps,
            inode_used_percent: inode,
        })
        .collect();

    Ok(Json(DiskHistoryResponse {
        resolution,
        points,
    }))
}

/// GET /metrics/network — points per (timestamp, interface_name).
pub async fn network_history(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Query(q): Query<MetricsRangeQuery>,
) -> AppResult<Json<NetworkHistoryResponse>> {
    let (start, end, resolution, limit) = resolve_range(&q)?;
    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo
        .read_network(&resolution, start, end, limit)
        .await?;

    let points: Vec<NetworkPoint> = rows
        .into_iter()
        .map(|(ts, iface, rx, tx, rxp, txp, ein, eout)| NetworkPoint {
            timestamp: ts,
            interface_name: iface,
            rx_bytes_per_sec: rx,
            tx_bytes_per_sec: tx,
            rx_packets_per_sec: rxp,
            tx_packets_per_sec: txp,
            errors_in_per_sec: ein,
            errors_out_per_sec: eout,
        })
        .collect();

    Ok(Json(NetworkHistoryResponse {
        resolution,
        points,
    }))
}

const VALID_PRESSURE_RESOURCES: &[&str] = &["cpu", "memory", "io"];

/// GET /metrics/pressure/{resource} — Linux PSI history for one resource.
///
/// `resource` is `cpu`, `memory`, or `io`. On non-Linux hosts (or pre-4.20
/// kernels) the underlying table simply has no rows, so the response is a
/// well-formed empty `points: []` rather than an error.
pub async fn pressure_history(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(resource): Path<String>,
    Query(q): Query<MetricsRangeQuery>,
) -> AppResult<Json<PressureHistoryResponse>> {
    if !VALID_PRESSURE_RESOURCES.contains(&resource.as_str()) {
        return Err(AppError::BadRequest(format!(
            "unknown pressure resource '{}'; expected one of {:?}",
            resource, VALID_PRESSURE_RESOURCES
        )));
    }

    let (start, end, resolution, limit) = resolve_range(&q)?;
    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo
        .read_pressure(&resource, &resolution, start, end, limit)
        .await?;

    let points: Vec<PressurePoint> = rows
        .into_iter()
        .map(|(ts, s10, s60, s300, f10, f60, f300)| PressurePoint {
            timestamp: ts,
            some_avg10: s10,
            some_avg60: s60,
            some_avg300: s300,
            full_avg10: f10,
            full_avg60: f60,
            full_avg300: f300,
        })
        .collect();

    Ok(Json(PressureHistoryResponse {
        resolution,
        resource,
        points,
    }))
}

/// GET /metrics/components — per-sensor temperature history. Flat row per
/// (timestamp, label); group client-side by `label` for per-sensor lines.
///
/// Available coverage is OS-dependent: Linux (lm-sensors / hwmon), macOS
/// (IOKit), Windows (varies — sysinfo's Windows backend exposes a limited
/// set; some boxes return no sensors at all). On a host without sensors
/// the response is `points: []` rather than an error.
pub async fn components_history(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Query(q): Query<MetricsRangeQuery>,
) -> AppResult<Json<ComponentsHistoryResponse>> {
    let (start, end, resolution, limit) = resolve_range(&q)?;
    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo
        .read_components(&resolution, start, end, limit)
        .await?;

    let points: Vec<ComponentPoint> = rows
        .into_iter()
        .map(|(ts, label, temp, max, crit)| ComponentPoint {
            timestamp: ts,
            label,
            temperature_c: temp,
            max_c: max,
            critical_c: crit,
        })
        .collect();

    Ok(Json(ComponentsHistoryResponse {
        resolution,
        points,
    }))
}
