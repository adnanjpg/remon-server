//! REST endpoints for the probe engine.
//!
//! - `GET    /probes`                                 — list with run-meta preview
//! - `GET    /probes/{name}`                          — single probe + last run + last metrics
//! - `GET    /probes/{name}/history?limit=`           — paged run-meta history
//! - `POST   /probes/reload`                          — re-scan `probes/`
//! - `GET    /metrics/probe/{probe}/{metric}?…`       — probe metric time-series

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::probes::scheduler;
use crate::routes::dtos::probes::{
    ListProbesResponse, ProbeDetail, ProbeHistoryResponse, ProbeListEntry,
    ProbeMetricHistoryResponse, ProbeMetricPoint, ProbeMetricRangeQuery, ProbeRunDto,
    ReloadFailure, ReloadProbesResponse,
};
use crate::routes::extractors::Claims;
use crate::state::AppState;
use crate::storage::repositories::ProbeRepository;

const DEFAULT_HISTORY_LIMIT: u32 = 100;
const MAX_HISTORY_LIMIT: u32 = 1000;
/// Sanity cap on `?offset=` — see alerts.rs MAX_EVENT_OFFSET for the
/// reasoning. probe_runs follows the same retention model.
const MAX_HISTORY_OFFSET: u32 = 100_000;
const PROBES_DIR: &str = "probes";

const DEFAULT_METRIC_SPAN_SECS: i64 = 3600;
const DEFAULT_METRIC_LIMIT: u32 = 1000;
const MAX_METRIC_LIMIT: u32 = 5000;

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// Same charset rule as `/services` — kept consistent so any future
/// `/probes/{name}` mutating endpoints inherit it without surprises.
fn validate_name(name: &str) -> AppResult<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(AppError::BadRequest(
            "probe name must be 1–64 characters".into(),
        ));
    }
    let ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
    if !ok {
        return Err(AppError::BadRequest(
            "probe name may only contain alphanumerics, `_`, `-`".into(),
        ));
    }
    Ok(())
}

/// Metric names follow the same rules as probe names — keeps URL paths
/// predictable across `/metrics/probe/{probe}/{metric}`.
fn validate_metric_name(name: &str) -> AppResult<()> {
    validate_name(name)
}

/// `GET /probes` — list every probe currently in the registry.
pub async fn list_probes(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ListProbesResponse>> {
    let reg = state.probe_registry.read().await;
    let mut entries: Vec<ProbeListEntry> = reg
        .probes
        .values()
        .map(|e| ProbeListEntry {
            name: e.manifest.name.clone(),
            description: e.manifest.description.clone(),
            enabled: e.manifest.enabled,
            schedule: e.manifest.schedule.as_db_string(),
            timeout_ms: e.manifest.timeout.as_millis() as u64,
            last_run_at: e.last_run.as_ref().map(|r| r.timestamp),
            last_message: e.last_run.as_ref().and_then(|r| r.message.clone()),
            last_parse_ok: e.last_run.as_ref().map(|r| r.parse_ok),
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(ListProbesResponse { probes: entries }))
}

/// `GET /probes/{name}` — single probe + last run-meta + last metrics.
pub async fn get_probe(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ProbeDetail>> {
    validate_name(&name)?;
    let reg = state.probe_registry.read().await;
    let entry = reg
        .probes
        .get(&name)
        .ok_or_else(|| AppError::NotFound(format!("Probe '{}'", name)))?;
    Ok(Json(ProbeDetail {
        name: entry.manifest.name.clone(),
        description: entry.manifest.description.clone(),
        enabled: entry.manifest.enabled,
        schedule: entry.manifest.schedule.as_db_string(),
        timeout_ms: entry.manifest.timeout.as_millis() as u64,
        command: entry.manifest.command.clone(),
        platforms: entry.manifest.platforms.clone(),
        last_run: entry.last_run.as_ref().cloned().map(ProbeRunDto::from),
        last_metrics: entry.last_metrics.clone(),
    }))
}

/// `GET /probes/{name}/history` — paged run-meta log, newest first.
pub async fn get_probe_history(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> AppResult<Json<ProbeHistoryResponse>> {
    validate_name(&name)?;
    let limit = q
        .limit
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .min(MAX_HISTORY_LIMIT);
    let offset = q.offset.unwrap_or(0).min(MAX_HISTORY_OFFSET);
    let repo = ProbeRepository::new(state.db.clone());
    let runs = repo.run_history(&name, limit, offset).await?;
    Ok(Json(ProbeHistoryResponse {
        probe_name: name,
        runs: runs.into_iter().map(ProbeRunDto::from).collect(),
    }))
}

/// `POST /probes/reload` — rescan manifests and sync registry.
pub async fn reload_probes(
    claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ReloadProbesResponse>> {
    let dir = PathBuf::from(PROBES_DIR);
    let report =
        scheduler::load_and_spawn(&dir, Arc::clone(&state.probe_registry), state.db.clone()).await;
    crate::services::events::record_operator(
        &state,
        &claims.device_id,
        "probes_reloaded",
        format!(
            "Probe definitions reloaded ({} loaded, {} failed)",
            report.loaded.len(),
            report.failed.len()
        ),
        None,
        None,
        Some(serde_json::json!({
            "loaded": report.loaded.len(),
            "skipped_disabled": report.skipped_disabled,
            "skipped_platform": report.skipped_platform,
            "failed": report.failed.len(),
        })),
    );
    Ok(Json(ReloadProbesResponse {
        loaded: report.loaded,
        skipped_disabled: report.skipped_disabled,
        skipped_platform: report.skipped_platform,
        failed: report
            .failed
            .into_iter()
            .map(ReloadFailure::from_pair)
            .collect(),
    }))
}

/// `GET /metrics/probe/{probe_name}/{metric_name}` — time-series view.
///
/// Mirrors the shape of `/metrics/cpu` etc. — `start`/`end` (Unix secs;
/// default last hour), `limit` (default 1000, hard cap 5000),
/// `resolution` (only `"raw"` supported today), and an optional
/// canonical-JSON `labels` filter for narrowing to one labelled stream.
pub async fn get_probe_metric_history(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path((probe_name, metric_name)): Path<(String, String)>,
    Query(q): Query<ProbeMetricRangeQuery>,
) -> AppResult<Json<ProbeMetricHistoryResponse>> {
    validate_name(&probe_name)?;
    validate_metric_name(&metric_name)?;

    let now = chrono::Utc::now().timestamp();
    let end = q.end.unwrap_or(now);
    let start = q.start.unwrap_or(end - DEFAULT_METRIC_SPAN_SECS);
    if end < start {
        return Err(AppError::BadRequest("end must be >= start".into()));
    }
    let resolution = q.resolution.unwrap_or_else(|| "raw".to_string());
    if resolution != "raw" {
        // Rollup for probe metrics not in MVP. Surface the limitation
        // explicitly rather than return empty silently.
        return Err(AppError::BadRequest(
            "only resolution=raw is supported for probe metrics today".into(),
        ));
    }
    let limit = q
        .limit
        .unwrap_or(DEFAULT_METRIC_LIMIT)
        .min(MAX_METRIC_LIMIT);

    let repo = ProbeRepository::new(state.db.clone());
    let rows = repo
        .read_metric_history(
            &probe_name,
            &metric_name,
            q.labels.as_deref(),
            &resolution,
            start,
            end,
            limit,
        )
        .await?;

    let points: Vec<ProbeMetricPoint> = rows
        .into_iter()
        .map(|s| {
            // The labels column is canonical JSON we wrote ourselves;
            // return it as a parsed JSON value so clients don't need a
            // second-level parse. Fallback to {} on the (impossible)
            // parse error path.
            let labels: serde_json::Value = serde_json::from_str(&s.labels_json)
                .unwrap_or(serde_json::Value::Object(Default::default()));
            ProbeMetricPoint {
                timestamp: s.timestamp,
                labels,
                value: s.value,
            }
        })
        .collect();

    Ok(Json(ProbeMetricHistoryResponse {
        probe_name,
        metric_name,
        resolution,
        points,
    }))
}
