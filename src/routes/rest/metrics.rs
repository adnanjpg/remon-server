//! Time-series history endpoints.

use axum::{
    Json,
    extract::{Path, State},
};
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::routes::dtos::metrics::{
    BatchMetricsQuery, BatchMetricsResponse, BatchSeries, ComponentPoint,
    ComponentsHistoryResponse, CpuCorePoint, CpuCoresHistoryResponse, CpuHistoryResponse, CpuPoint,
    DiskForecastMount, DiskForecastResponse, DiskHistoryResponse, DiskPoint, DockerHistoryResponse,
    DockerPoint, MemoryHistoryResponse, MemoryPoint, MetricsRangeQuery, NetworkHistoryResponse,
    NetworkPoint, NetworkUsageInterface, NetworkUsageResponse, PressureHistoryResponse,
    PressurePoint,
};
use crate::routes::extractors::{Claims, ValidatedQuery};
use crate::services::forecast;
use crate::state::AppState;
use crate::storage::repositories::{MetricsRepository, ResolutionRepository};

/// Default span when client omits start/end: last hour.
const DEFAULT_SPAN_SECS: i64 = 3600;
/// Default points per response when client omits `limit`. Sized to fit
/// the worst case for each resolution band's typical use:
/// - raw   @2s × 2h  = 3 600 points
/// - 1m    × 24h    = 1 440 points
/// - 5m    × 7d     = 2 016 points
/// - 1h    × ~6 mo  = 4 320 points
///
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
        return Err(AppError::BadRequest("end must be >= start".to_string()));
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
    ValidatedQuery(q): ValidatedQuery<MetricsRangeQuery>,
) -> AppResult<Json<CpuHistoryResponse>> {
    let (start, end, resolution, limit) = resolve_range(&q)?;
    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo.read_cpu(&resolution, start, end, limit).await?;

    let points = rows.into_iter().map(CpuPoint::from).collect();

    Ok(Json(CpuHistoryResponse { resolution, points }))
}

/// GET /metrics/cpu/cores — per-core usage. Always raw (the per-core
/// breakdown is not rolled up; aggregates use host-level metrics_cpu).
pub async fn cpu_cores_history(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    ValidatedQuery(q): ValidatedQuery<MetricsRangeQuery>,
) -> AppResult<Json<CpuCoresHistoryResponse>> {
    let (start, end, _resolution, limit) = resolve_range(&q)?;
    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo.read_cpu_cores(start, end, limit).await?;

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
    ValidatedQuery(q): ValidatedQuery<MetricsRangeQuery>,
) -> AppResult<Json<MemoryHistoryResponse>> {
    let (start, end, resolution, limit) = resolve_range(&q)?;
    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo.read_memory(&resolution, start, end, limit).await?;

    let points = rows.into_iter().map(MemoryPoint::from).collect();

    Ok(Json(MemoryHistoryResponse { resolution, points }))
}

/// GET /metrics/disk — points per (timestamp, mount_point); client groups
/// by mount_point if it wants per-volume series.
pub async fn disk_history(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    ValidatedQuery(q): ValidatedQuery<MetricsRangeQuery>,
) -> AppResult<Json<DiskHistoryResponse>> {
    let (start, end, resolution, limit) = resolve_range(&q)?;
    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo.read_disk(&resolution, start, end, limit).await?;

    let points = rows.into_iter().map(DiskPoint::from).collect();

    Ok(Json(DiskHistoryResponse { resolution, points }))
}

/// GET /metrics/network — points per (timestamp, interface_name).
pub async fn network_history(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    ValidatedQuery(q): ValidatedQuery<MetricsRangeQuery>,
) -> AppResult<Json<NetworkHistoryResponse>> {
    let (start, end, resolution, limit) = resolve_range(&q)?;
    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo.read_network(&resolution, start, end, limit).await?;

    let points = rows.into_iter().map(NetworkPoint::from).collect();
    let totals = repo
        .read_network_totals(&resolution, start, end, limit)
        .await?
        .into_iter()
        .map(NetworkPoint::from)
        .collect();

    Ok(Json(NetworkHistoryResponse {
        resolution,
        points,
        totals,
    }))
}

/// GET /metrics/network/usage — bytes moved over a window, not bytes per second.
///
/// The question this answers ("how much traffic has this host used this month")
/// has no other source. `NetworkStats::rx_bytes_total` carries the kernel's
/// cumulative counters, but those are live-only, and they restart at every
/// reboot — on a box up for 200 days they overstate a month, and on one that
/// rebooted last night they are near useless. The stored rates, integrated,
/// are bounded by whatever window the caller asks for.
pub async fn network_usage(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    ValidatedQuery(q): ValidatedQuery<MetricsRangeQuery>,
) -> AppResult<Json<NetworkUsageResponse>> {
    let (start, end, resolution, _) = resolve_range(&q)?;

    // Bucket width comes from the table rather than a constant: an operator who
    // retunes a resolution would otherwise silently scale every total they read.
    let bucket_secs = ResolutionRepository::new(state.db.clone())
        .list_all()
        .await?
        .into_iter()
        .find(|r| r.name == resolution)
        .map(|r| r.interval_seconds)
        .filter(|s| *s > 0)
        .ok_or_else(|| {
            AppError::Internal(format!("resolution '{}' has no interval", resolution))
        })?;

    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo
        .read_network_usage(&resolution, start, end, bucket_secs)
        .await?;

    let interfaces: Vec<NetworkUsageInterface> = rows
        .into_iter()
        .map(|(name, rx, tx)| NetworkUsageInterface {
            is_tunnel: crate::services::system::is_tunnel_interface(&name),
            name,
            rx_bytes: rx,
            tx_bytes: tx,
        })
        .collect();

    let (total_rx_bytes, total_tx_bytes) = interfaces
        .iter()
        .filter(|i| !i.is_tunnel)
        .fold((0i64, 0i64), |(rx, tx), i| {
            (rx.saturating_add(i.rx_bytes), tx.saturating_add(i.tx_bytes))
        });

    // A zero-length window is one instant, which either has a bucket or does
    // not; expressing that as a ratio would divide by zero.
    let expected = ((end - start) / bucket_secs).max(1);
    let observed = repo.count_network_buckets(&resolution, start, end).await?;
    let coverage = (observed as f64 / expected as f64).clamp(0.0, 1.0);

    Ok(Json(NetworkUsageResponse {
        start,
        end,
        resolution,
        total_rx_bytes,
        total_tx_bytes,
        coverage,
        interfaces,
    }))
}

/// Fit window when the caller names none. Two weeks of hourly rows averages
/// out the daily backup-and-delete cycle that dominates a shorter window,
/// without reaching so far back that last month's cleanup still counts.
const FORECAST_WINDOW_DEFAULT_DAYS: f64 = 14.0;
const FORECAST_WINDOW_MAX_DAYS: f64 = 90.0;
/// Past this, a date implies a precision two weeks of samples cannot support.
const FORECAST_HORIZON_DEFAULT_DAYS: f64 = 60.0;
const FORECAST_HORIZON_MAX_DAYS: f64 = 365.0;

#[derive(Debug, serde::Deserialize)]
pub struct DiskForecastQuery {
    /// Days of history to fit through. Default 14, max 90.
    pub window_days: Option<f64>,
    /// Report a date only when it falls inside this many days. Default 60.
    pub horizon_days: Option<f64>,
}

/// GET /metrics/disk/forecast — when each volume runs out of room.
///
/// "78% full" is a fact about now; "fills in eleven days" is the one an
/// operator can act on, and the stored history is the only place it exists.
/// The fit is Theil-Sen rather than least squares because a filesystem's
/// outliers are not noise around a line — a log rotation, or a backup that
/// writes and then deletes, is a real excursion that OLS would let drag the
/// date by weeks. See `services::forecast` for the estimator and, more to the
/// point, for the rule that decides when there is no honest answer.
pub async fn disk_forecast(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    ValidatedQuery(q): ValidatedQuery<DiskForecastQuery>,
) -> AppResult<Json<DiskForecastResponse>> {
    let window_days = q
        .window_days
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(FORECAST_WINDOW_DEFAULT_DAYS)
        .min(FORECAST_WINDOW_MAX_DAYS);
    let horizon_days = q
        .horizon_days
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(FORECAST_HORIZON_DEFAULT_DAYS)
        .min(FORECAST_HORIZON_MAX_DAYS);

    let end = chrono::Utc::now().timestamp();
    let window_secs = (window_days * 86_400.0) as i64;
    let start = end - window_secs;
    // Same banding as the charts: a fortnight lands on hourly rows, which is
    // also the tier retained long enough for the widest window on offer.
    let resolution = pick_resolution(window_secs).to_string();

    let rows = MetricsRepository::new(state.db.clone())
        .read_disk_capacity_series(&resolution, start, end)
        .await?;

    // Rows arrive grouped and ordered by (mount_point, timestamp), so one pass
    // splits them without sorting again.
    let mut mounts: Vec<DiskForecastMount> = Vec::new();
    let mut cursor = 0usize;
    while cursor < rows.len() {
        let mount = rows[cursor].1.clone();
        let mut series: Vec<(f64, f64)> = Vec::new();
        let (mut used, mut total) = (0i64, 0i64);
        while cursor < rows.len() && rows[cursor].1 == mount {
            let (ts, _, u, t) = &rows[cursor];
            series.push((*ts as f64, *u as f64));
            // Ordered by timestamp, so the last write wins and capacity is read
            // as of now — a volume grown mid-window forecasts against the size
            // it has, not the size it had.
            used = *u;
            total = *t;
            cursor += 1;
        }

        let Some(trend) = forecast::theil_sen(&series) else {
            mounts.push(DiskForecastMount {
                mount_point: mount,
                used_bytes: used,
                total_bytes: total,
                verdict: forecast::Verdict::Unclear.as_str().to_string(),
                bytes_per_day: 0,
                days_until_full: None,
                days_until_full_low: None,
                days_until_full_high: None,
                points: series.len(),
            });
            continue;
        };

        // The span actually covered, not the span requested: a daemon that was
        // down for half the window must not have its trend judged against time
        // it never observed.
        let covered = series
            .last()
            .zip(series.first())
            .map(|(b, f)| b.0 - f.0)
            .unwrap_or(0.0);
        let f = forecast::forecast_full(&trend, used as f64, total as f64, covered, horizon_days);

        mounts.push(DiskForecastMount {
            mount_point: mount,
            used_bytes: used,
            total_bytes: total,
            verdict: f.verdict.as_str().to_string(),
            bytes_per_day: f.bytes_per_day as i64,
            days_until_full: f.days_until_full,
            days_until_full_low: f.days_until_full_low,
            days_until_full_high: f.days_until_full_high,
            points: trend.points,
        });
    }

    Ok(Json(DiskForecastResponse {
        start,
        end,
        resolution,
        horizon_days,
        mounts,
    }))
}

/// GET /metrics/docker/{container} — per-tick resource history for one
/// container, keyed by name. Empty when the docker collector never ran.
pub async fn docker_history(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(container): Path<String>,
    ValidatedQuery(q): ValidatedQuery<MetricsRangeQuery>,
) -> AppResult<Json<DockerHistoryResponse>> {
    let (start, end, resolution, limit) = resolve_range(&q)?;
    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo
        .read_docker(&container, &resolution, start, end, limit)
        .await?;

    let points: Vec<DockerPoint> = rows
        .into_iter()
        .map(|(ts, cpu, mu, ml, rx, tx, br, bw, pids)| DockerPoint {
            timestamp: ts,
            cpu_percent: cpu,
            memory_used_bytes: mu,
            memory_limit_bytes: ml,
            network_rx_bytes: rx,
            network_tx_bytes: tx,
            block_read_bytes: br,
            block_write_bytes: bw,
            pids,
        })
        .collect();

    Ok(Json(DockerHistoryResponse { resolution, points }))
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
    ValidatedQuery(q): ValidatedQuery<MetricsRangeQuery>,
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
    ValidatedQuery(q): ValidatedQuery<MetricsRangeQuery>,
) -> AppResult<Json<ComponentsHistoryResponse>> {
    let (start, end, resolution, limit) = resolve_range(&q)?;
    let repo = MetricsRepository::new(state.db.clone());
    let rows = repo.read_components(&resolution, start, end, limit).await?;

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

    Ok(Json(ComponentsHistoryResponse { resolution, points }))
}

// ===== Batch =====

/// Whitelist for `?resources=`. Pressure/probe stay out of MVP — they
/// need a sub-key (`pressure:cpu`, `probe:nginx:rps`) the simple
/// comma-list shape can't express cleanly.
pub const BATCH_RESOURCES: &[&str] = &[
    "cpu",
    "cpu_cores",
    "memory",
    "disk",
    "network",
    "components",
];

/// Hard cap on how many series one batch can carry. Worst case = 8 ×
/// MAX_LIMIT rows; well past dashboard needs and prevents accidental
/// `resources=...` strings that fan out the SQL pool.
const MAX_BATCH_RESOURCES: usize = 8;

/// Parse `1h`, `30m`, `24h`, `7d`, `60s`, or raw seconds.
fn parse_span(s: &str) -> AppResult<i64> {
    let s = s.trim();
    let (num_part, mult) = match s.chars().last() {
        Some('s') | Some('S') => (&s[..s.len() - 1], 1i64),
        Some('m') | Some('M') => (&s[..s.len() - 1], 60),
        Some('h') | Some('H') => (&s[..s.len() - 1], 3600),
        Some('d') | Some('D') => (&s[..s.len() - 1], 86400),
        Some(c) if c.is_ascii_digit() => (s, 1),
        _ => return Err(AppError::BadRequest(format!("invalid span '{}'", s))),
    };
    let n: i64 = num_part
        .parse()
        .map_err(|_| AppError::BadRequest(format!("invalid span '{}'", s)))?;
    if n <= 0 {
        return Err(AppError::BadRequest("span must be > 0".into()));
    }
    n.checked_mul(mult)
        .ok_or_else(|| AppError::BadRequest(format!("span too large: {}", s)))
}

/// GET /metrics/batch — fetch many resources in one round trip.
pub async fn batch_history(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    ValidatedQuery(q): ValidatedQuery<BatchMetricsQuery>,
) -> AppResult<Json<BatchMetricsResponse>> {
    // Window: span XOR start/end. Both supplied is ambiguous → 400.
    if q.span.is_some() && (q.start.is_some() || q.end.is_some()) {
        return Err(AppError::BadRequest(
            "use either `span` or `start`/`end`, not both".into(),
        ));
    }
    let now = chrono::Utc::now().timestamp();
    let (start, end) = if let Some(span_str) = q.span.as_deref() {
        let span = parse_span(span_str)?;
        (now - span, now)
    } else {
        let end = q.end.unwrap_or(now);
        let start = q.start.unwrap_or(end - DEFAULT_SPAN_SECS);
        (start, end)
    };
    if end < start {
        return Err(AppError::BadRequest("end must be >= start".into()));
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

    // Validate `resources`: non-empty, <= cap, no dupes, all whitelisted.
    // Order preserved so `series` mirrors the request — easier to debug.
    let requested: Vec<&str> = q
        .resources
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if requested.is_empty() {
        return Err(AppError::BadRequest("`resources` must not be empty".into()));
    }
    if requested.len() > MAX_BATCH_RESOURCES {
        return Err(AppError::BadRequest(format!(
            "too many resources (max {})",
            MAX_BATCH_RESOURCES
        )));
    }
    for (i, r) in requested.iter().enumerate() {
        if !BATCH_RESOURCES.contains(r) {
            return Err(AppError::BadRequest(format!(
                "unknown resource '{}'; expected one of {:?}",
                r, BATCH_RESOURCES
            )));
        }
        if requested[..i].contains(r) {
            return Err(AppError::BadRequest(format!("duplicate resource '{}'", r)));
        }
    }

    // Run per-resource reads in parallel. Cores has no rollup so it
    // ignores `resolution` — repo handles that.
    let repo = MetricsRepository::new(state.db.clone());
    let mut futs: Vec<futures_util::future::BoxFuture<'_, AppResult<BatchSeries>>> =
        Vec::with_capacity(requested.len());
    for r in &requested {
        let res = resolution.clone();
        let repo = &repo;
        futs.push(match *r {
            "cpu" => Box::pin(async move {
                let rows = repo.read_cpu(&res, start, end, limit).await?;
                Ok(BatchSeries::Cpu {
                    points: rows.into_iter().map(CpuPoint::from).collect(),
                })
            }),
            "cpu_cores" => Box::pin(async move {
                let rows = repo.read_cpu_cores(start, end, limit).await?;
                Ok(BatchSeries::CpuCores {
                    points: rows
                        .into_iter()
                        .map(|(ts, idx, usage, freq)| CpuCorePoint {
                            timestamp: ts,
                            core_index: idx,
                            usage_percent: usage,
                            freq_mhz: freq,
                        })
                        .collect(),
                })
            }),
            "memory" => Box::pin(async move {
                let rows = repo.read_memory(&res, start, end, limit).await?;
                Ok(BatchSeries::Memory {
                    points: rows.into_iter().map(MemoryPoint::from).collect(),
                })
            }),
            "disk" => Box::pin(async move {
                let rows = repo.read_disk(&res, start, end, limit).await?;
                Ok(BatchSeries::Disk {
                    points: rows.into_iter().map(DiskPoint::from).collect(),
                })
            }),
            "network" => Box::pin(async move {
                let rows = repo.read_network(&res, start, end, limit).await?;
                Ok(BatchSeries::Network {
                    points: rows.into_iter().map(NetworkPoint::from).collect(),
                    totals: repo
                        .read_network_totals(&res, start, end, limit)
                        .await?
                        .into_iter()
                        .map(NetworkPoint::from)
                        .collect(),
                })
            }),
            "components" => Box::pin(async move {
                let rows = repo.read_components(&res, start, end, limit).await?;
                Ok(BatchSeries::Components {
                    points: rows
                        .into_iter()
                        .map(|(ts, label, temp, max, crit)| ComponentPoint {
                            timestamp: ts,
                            label,
                            temperature_c: temp,
                            max_c: max,
                            critical_c: crit,
                        })
                        .collect(),
                })
            }),
            // Whitelist already checked above.
            other => unreachable!("unvalidated resource slipped through: {}", other),
        });
    }

    let series = futures_util::future::try_join_all(futs).await?;
    Ok(Json(BatchMetricsResponse {
        start,
        end,
        resolution,
        series,
    }))
}
