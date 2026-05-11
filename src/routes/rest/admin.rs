//! Admin endpoints — runtime configuration read/write.
//!
//! `GET  /config` returns the live effective config (TOML defaults already
//! merged with DB overrides at boot, plus any subsequent PATCH).
//!
//! `PATCH /config` accepts a partial body, merges into the current row in
//! `server_config`, persists, then refreshes AppState in-place: the
//! `Arc<RwLock<EffectiveConfig>>` swaps and the collector intervals'
//! `AtomicU64` handles update on the next tick — no restart required.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::{Json, extract::State};
use log::info;

use crate::error::{AppError, AppResult};
use crate::routes::dtos::admin::{ConfigResponse, UpdateConfigRequest};
use crate::routes::extractors::Claims;
use crate::state::{AppState, EffectiveConfig};
use crate::storage::repositories::{ConfigRepository, RuntimeOverrides};

/// GET /config — current effective configuration.
pub async fn get_config(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ConfigResponse>> {
    let effective = state.effective_config.read().await.clone();
    Ok(Json(ConfigResponse {
        server_name: effective.server_name,
        collector_stats_base_interval_ms: effective.collector_stats_base_interval_ms,
        collector_stats_interval_ms: state.collector_stats_interval_ms.load(Ordering::Relaxed),
        collector_processes_interval_ms: state
            .collector_processes_interval_ms
            .load(Ordering::Relaxed),
        #[cfg(feature = "docker")]
        collector_docker_interval_ms: state.collector_docker_interval_ms.load(Ordering::Relaxed),
        #[cfg(not(feature = "docker"))]
        collector_docker_interval_ms: 0,
        rollup_tick_interval_ms: effective.rollup_tick_interval_ms,
        retention_tick_interval_ms: effective.retention_tick_interval_ms,
    }))
}

/// PATCH /config — apply a partial update.
///
/// Order matters here: persist first, then update in-memory state. If the
/// DB write fails the runtime stays on the old values (no torn state).
pub async fn patch_config(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpdateConfigRequest>,
) -> AppResult<Json<ConfigResponse>> {
    let repo = ConfigRepository::new(state.db.clone());
    let current = repo.load().await?;

    // Merge: every Some(x) wins over current.
    let merged = RuntimeOverrides {
        server_name: req.server_name.unwrap_or(current.server_name),
        collector_stats_interval_ms: req
            .collector_stats_interval_ms
            .unwrap_or(current.collector_stats_interval_ms),
        collector_processes_interval_ms: req
            .collector_processes_interval_ms
            .unwrap_or(current.collector_processes_interval_ms),
        collector_docker_interval_ms: req
            .collector_docker_interval_ms
            .unwrap_or(current.collector_docker_interval_ms),
        rollup_tick_interval_ms: req
            .rollup_tick_interval_ms
            .unwrap_or(current.rollup_tick_interval_ms),
        retention_tick_interval_ms: req
            .retention_tick_interval_ms
            .unwrap_or(current.retention_tick_interval_ms),
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
    if merged.collector_processes_interval_ms < MIN_COLLECTOR_INTERVAL_MS {
        return Err(AppError::BadRequest(format!(
            "collector_processes_interval_ms must be >= {}",
            MIN_COLLECTOR_INTERVAL_MS
        )));
    }

    repo.update(&merged).await?;

    // Refresh in-memory: collector loops re-read `AtomicU64` each tick;
    // background tasks re-read the `RwLock` each tick.
    state
        .collector_stats_interval_ms
        .store(merged.collector_stats_interval_ms, Ordering::Relaxed);
    state
        .collector_processes_interval_ms
        .store(merged.collector_processes_interval_ms, Ordering::Relaxed);
    #[cfg(feature = "docker")]
    state
        .collector_docker_interval_ms
        .store(merged.collector_docker_interval_ms, Ordering::Relaxed);

    {
        let mut effective = state.effective_config.write().await;
        *effective = EffectiveConfig {
            server_name: merged.server_name.clone(),
            collector_stats_base_interval_ms: merged.collector_stats_interval_ms,
            rollup_tick_interval_ms: merged.rollup_tick_interval_ms,
            retention_tick_interval_ms: merged.retention_tick_interval_ms,
        };
    }

    #[cfg(feature = "docker")]
    info!(
        "Runtime config updated: stats(base)={}ms processes={}ms docker={}ms rollup={}ms retention={}ms",
        merged.collector_stats_interval_ms,
        merged.collector_processes_interval_ms,
        merged.collector_docker_interval_ms,
        merged.rollup_tick_interval_ms,
        merged.retention_tick_interval_ms,
    );
    #[cfg(not(feature = "docker"))]
    info!(
        "Runtime config updated: stats(base)={}ms processes={}ms rollup={}ms retention={}ms",
        merged.collector_stats_interval_ms,
        merged.collector_processes_interval_ms,
        merged.rollup_tick_interval_ms,
        merged.retention_tick_interval_ms,
    );

    Ok(Json(ConfigResponse {
        server_name: merged.server_name,
        collector_stats_base_interval_ms: merged.collector_stats_interval_ms,
        collector_stats_interval_ms: state
            .collector_stats_interval_ms
            .load(Ordering::Relaxed),
        collector_processes_interval_ms: merged.collector_processes_interval_ms,
        #[cfg(feature = "docker")]
        collector_docker_interval_ms: merged.collector_docker_interval_ms,
        #[cfg(not(feature = "docker"))]
        collector_docker_interval_ms: 0,
        rollup_tick_interval_ms: merged.rollup_tick_interval_ms,
        retention_tick_interval_ms: merged.retention_tick_interval_ms,
    }))
}
