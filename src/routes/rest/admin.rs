//! Admin endpoints — runtime configuration read/write.
//!
//! `GET  /config` returns the live effective config (TOML defaults already
//! merged with DB overrides at boot, plus any subsequent PATCH).
//!
//! `PATCH /config` accepts a partial body, merges into the current row in
//! `server_config`, persists, then refreshes AppState in-place: the
//! `Arc<RwLock<EffectiveConfig>>` swaps and the collector intervals'
//! `AtomicU64` handles update on the next tick — no restart required.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::{
    Json,
    extract::{Path, State},
};
use log::info;

use crate::error::{AppError, AppResult};
use crate::routes::dtos::admin::{
    ConfigResponse, ResolutionDto, ResolutionsResponse, RetentionPolicyDto, RetentionResponse,
    UpdateConfigRequest, UpdateResolutionRequest, UpdateRetentionRequest,
};
use crate::routes::extractors::Claims;
use crate::state::{AppState, EffectiveConfig};
use crate::storage::repositories::{
    ConfigRepository, ResolutionRepository, RetentionRepository, RuntimeOverrides,
};

/// GET /config — current effective configuration.
pub async fn get_config(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ConfigResponse>> {
    let effective = state.effective_config.read().await.clone();
    // Live values come from runtime state; only the audit timestamp needs
    // the DB row.
    let updated_at = ConfigRepository::new(state.db.clone())
        .load()
        .await?
        .updated_at;
    Ok(Json(ConfigResponse {
        server_name: effective.server_name,
        collector_stats_interval_ms: state.collector_stats_interval_ms.load(Ordering::Relaxed),
        collector_processes_interval_ms: state.processes_cache_ttl_ms.load(Ordering::Relaxed),
        collector_docker_interval_ms: state.collector_docker_interval_ms.load(Ordering::Relaxed),
        collector_smart_interval_ms: state.collector_smart_interval_ms.load(Ordering::Relaxed),
        rollup_tick_interval_ms: effective.rollup_tick_interval_ms,
        retention_tick_interval_ms: effective.retention_tick_interval_ms,
        updated_at,
    }))
}

/// PATCH /config — apply a partial update.
///
/// Order matters here: persist first, then update in-memory state. If the
/// DB write fails the runtime stays on the old values (no torn state).
pub async fn patch_config(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpdateConfigRequest>,
) -> AppResult<Json<ConfigResponse>> {
    let repo = ConfigRepository::new(state.db.clone());
    let current = repo.load().await?;

    // Which fields the caller actually sent — the audit row names them.
    let mut changed: Vec<&'static str> = Vec::new();
    if req.server_name.is_some() {
        changed.push("server_name");
    }
    if req.collector_stats_interval_ms.is_some() {
        changed.push("collector_stats_interval_ms");
    }
    if req.collector_processes_interval_ms.is_some() {
        changed.push("collector_processes_interval_ms");
    }
    if req.collector_docker_interval_ms.is_some() {
        changed.push("collector_docker_interval_ms");
    }
    if req.collector_smart_interval_ms.is_some() {
        changed.push("collector_smart_interval_ms");
    }
    if req.rollup_tick_interval_ms.is_some() {
        changed.push("rollup_tick_interval_ms");
    }
    if req.retention_tick_interval_ms.is_some() {
        changed.push("retention_tick_interval_ms");
    }

    // Merge: every Some(x) wins over current.
    let merged = RuntimeOverrides {
        server_name: req.server_name.unwrap_or(current.server_name),
        collector_stats_interval_ms: req
            .collector_stats_interval_ms
            .unwrap_or(current.collector_stats_interval_ms),
        processes_cache_ttl_ms: req
            .collector_processes_interval_ms
            .unwrap_or(current.processes_cache_ttl_ms),
        collector_docker_interval_ms: req
            .collector_docker_interval_ms
            .unwrap_or(current.collector_docker_interval_ms),
        collector_smart_interval_ms: req
            .collector_smart_interval_ms
            .unwrap_or(current.collector_smart_interval_ms),
        rollup_tick_interval_ms: req
            .rollup_tick_interval_ms
            .unwrap_or(current.rollup_tick_interval_ms),
        retention_tick_interval_ms: req
            .retention_tick_interval_ms
            .unwrap_or(current.retention_tick_interval_ms),
        updated_at: current.updated_at,
    };

    const MAX_SERVER_NAME_LEN: usize = 128;
    if merged.server_name.len() > MAX_SERVER_NAME_LEN {
        return Err(AppError::BadRequest(format!(
            "server_name length {} exceeds maximum {}",
            merged.server_name.len(),
            MAX_SERVER_NAME_LEN
        )));
    }

    // Sub-second intervals collide on second-resolution metric PKs.
    const MIN_COLLECTOR_INTERVAL_MS: u64 = 1000;
    if merged.collector_stats_interval_ms < MIN_COLLECTOR_INTERVAL_MS {
        return Err(AppError::BadRequest(format!(
            "collector_stats_interval_ms must be >= {} (sub-second sampling \
             collides with second-resolution timestamps)",
            MIN_COLLECTOR_INTERVAL_MS
        )));
    }
    if merged.processes_cache_ttl_ms < MIN_COLLECTOR_INTERVAL_MS {
        return Err(AppError::BadRequest(format!(
            "collector_processes_interval_ms must be >= {}",
            MIN_COLLECTOR_INTERVAL_MS
        )));
    }
    if merged.collector_docker_interval_ms < MIN_COLLECTOR_INTERVAL_MS {
        return Err(AppError::BadRequest(format!(
            "collector_docker_interval_ms must be >= {}",
            MIN_COLLECTOR_INTERVAL_MS
        )));
    }

    // Each SMART tick shells out to smartctl for every disk — sub-minute
    // cadences are never sensible (mirrors the collector's own floor).
    const MIN_SMART_INTERVAL_MS: u64 = 60_000;
    if merged.collector_smart_interval_ms < MIN_SMART_INTERVAL_MS {
        return Err(AppError::BadRequest(format!(
            "collector_smart_interval_ms must be >= {}",
            MIN_SMART_INTERVAL_MS
        )));
    }

    let updated_at = repo.update(&merged).await?;

    // Refresh in-memory: collector loops re-read `AtomicU64` each tick;
    // background tasks re-read the `RwLock` each tick.
    state
        .collector_stats_interval_ms
        .store(merged.collector_stats_interval_ms, Ordering::Relaxed);
    state
        .processes_cache_ttl_ms
        .store(merged.processes_cache_ttl_ms, Ordering::Relaxed);
    state
        .collector_docker_interval_ms
        .store(merged.collector_docker_interval_ms, Ordering::Relaxed);
    state
        .collector_smart_interval_ms
        .store(merged.collector_smart_interval_ms, Ordering::Relaxed);

    {
        let mut effective = state.effective_config.write().await;
        *effective = EffectiveConfig {
            server_name: merged.server_name.clone(),
            rollup_tick_interval_ms: merged.rollup_tick_interval_ms,
            retention_tick_interval_ms: merged.retention_tick_interval_ms,
        };
    }

    info!(
        "runtime config updated: stats={}ms processes={}ms docker={}ms smart={}ms rollup={}ms retention={}ms",
        merged.collector_stats_interval_ms,
        merged.processes_cache_ttl_ms,
        merged.collector_docker_interval_ms,
        merged.collector_smart_interval_ms,
        merged.rollup_tick_interval_ms,
        merged.retention_tick_interval_ms,
    );

    if !changed.is_empty() {
        crate::services::events::record_operator(
            &state,
            &claims.device_id,
            "config_changed",
            format!("Runtime configuration updated ({})", changed.join(", ")),
            None,
            None,
            Some(serde_json::json!({ "fields": changed })),
        );
    }

    Ok(Json(ConfigResponse {
        server_name: merged.server_name,
        collector_stats_interval_ms: state.collector_stats_interval_ms.load(Ordering::Relaxed),
        collector_processes_interval_ms: merged.processes_cache_ttl_ms,
        collector_docker_interval_ms: merged.collector_docker_interval_ms,
        collector_smart_interval_ms: merged.collector_smart_interval_ms,
        rollup_tick_interval_ms: merged.rollup_tick_interval_ms,
        retention_tick_interval_ms: merged.retention_tick_interval_ms,
        updated_at,
    }))
}

// ─── Retention policy ───────────────────────────────────────────────────────

/// Keeping less than an hour of anything guts the incident post-mortem
/// story; more than ten years is a typo.
const MIN_KEEP_SECONDS: i64 = 3600;
const MAX_KEEP_SECONDS: i64 = 10 * 365 * 86400;

/// GET /config/retention — per-(resource, resolution) keep windows.
pub async fn get_retention(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<RetentionResponse>> {
    let policies = RetentionRepository::new(state.db.clone())
        .list_all()
        .await?
        .into_iter()
        .map(|p| RetentionPolicyDto {
            resource: p.resource,
            resolution: p.resolution,
            keep_seconds: p.keep_seconds,
        })
        .collect();
    Ok(Json(RetentionResponse { policies }))
}

/// PATCH /config/retention — batch-update keep windows. The next retention
/// tick picks the new values up automatically (the task re-reads the table).
pub async fn patch_retention(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpdateRetentionRequest>,
) -> AppResult<Json<RetentionResponse>> {
    if req.policies.is_empty() {
        return Err(AppError::BadRequest("policies must not be empty".into()));
    }

    let repo = RetentionRepository::new(state.db.clone());

    // Validate the whole batch before writing anything so a bad entry can't
    // leave the batch half-applied.
    let existing: std::collections::HashSet<(String, String)> = repo
        .list_all()
        .await?
        .into_iter()
        .map(|p| (p.resource, p.resolution))
        .collect();

    for p in &req.policies {
        if !(MIN_KEEP_SECONDS..=MAX_KEEP_SECONDS).contains(&p.keep_seconds) {
            return Err(AppError::BadRequest(format!(
                "keep_seconds for {}/{} must be between {} and {}",
                p.resource, p.resolution, MIN_KEEP_SECONDS, MAX_KEEP_SECONDS
            )));
        }
        if !existing.contains(&(p.resource.clone(), p.resolution.clone())) {
            return Err(AppError::NotFound(format!(
                "retention policy {}/{}",
                p.resource, p.resolution
            )));
        }
    }

    for p in &req.policies {
        repo.set_keep(&p.resource, &p.resolution, p.keep_seconds)
            .await?;
    }

    info!("retention policy updated: {} row(s)", req.policies.len());
    let touched: Vec<String> = req
        .policies
        .iter()
        .map(|p| format!("{}/{}", p.resource, p.resolution))
        .collect();
    crate::services::events::record_operator(
        &state,
        &claims.device_id,
        "config_changed",
        format!("Retention policy updated ({} entries)", touched.len()),
        None,
        None,
        Some(serde_json::json!({ "retention": touched })),
    );
    get_retention(claims, State(state)).await
}

// ─── Resolutions ────────────────────────────────────────────────────────────

/// GET /config/resolutions — rollup bucket chain with enabled flags.
pub async fn get_resolutions(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ResolutionsResponse>> {
    let resolutions = ResolutionRepository::new(state.db.clone())
        .list_all()
        .await?
        .into_iter()
        .map(|r| ResolutionDto {
            name: r.name,
            interval_seconds: r.interval_seconds,
            rollup_from: r.rollup_from,
            enabled: r.enabled,
        })
        .collect();
    Ok(Json(ResolutionsResponse { resolutions }))
}

/// PATCH /config/resolutions/{name} — enable/disable one rollup bucket.
///
/// The chain must stay contiguous: disabling a bucket that a still-enabled
/// child rolls up from would silently starve the child, and enabling a
/// bucket under a disabled parent would never receive data. `raw` is what
/// collectors write directly — it can't be turned off here.
pub async fn patch_resolution(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<UpdateResolutionRequest>,
) -> AppResult<Json<ResolutionsResponse>> {
    let repo = ResolutionRepository::new(state.db.clone());
    let all = repo.list_all().await?;

    let target = all
        .iter()
        .find(|r| r.name == name)
        .ok_or_else(|| AppError::NotFound(format!("resolution '{}'", name)))?;

    if !req.enabled {
        if target.rollup_from.is_none() {
            return Err(AppError::BadRequest(format!(
                "'{}' is written directly by collectors and cannot be disabled",
                name
            )));
        }
        if let Some(child) = all
            .iter()
            .find(|r| r.enabled && r.rollup_from.as_deref() == Some(name.as_str()))
        {
            return Err(AppError::BadRequest(format!(
                "'{}' feeds enabled resolution '{}'; disable that first",
                name, child.name
            )));
        }
    } else if let Some(parent) = target
        .rollup_from
        .as_deref()
        .and_then(|p| all.iter().find(|r| r.name == p))
        && !parent.enabled
    {
        return Err(AppError::BadRequest(format!(
            "'{}' rolls up from disabled resolution '{}'; enable that first",
            name, parent.name
        )));
    }

    repo.set_enabled(&name, req.enabled).await?;
    info!("resolution '{}' enabled={}", name, req.enabled);
    crate::services::events::record_operator(
        &state,
        &claims.device_id,
        "config_changed",
        format!(
            "Rollup resolution '{}' {}",
            name,
            if req.enabled { "enabled" } else { "disabled" }
        ),
        None,
        None,
        Some(serde_json::json!({ "resolution": name, "enabled": req.enabled })),
    );
    get_resolutions(claims, State(state)).await
}
