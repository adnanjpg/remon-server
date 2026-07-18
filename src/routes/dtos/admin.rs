//! Admin / runtime-config DTOs.
//!
//! `PATCH /config` accepts a partial body — every field is `Option<T>` so
//! the client only sends what they want to change. Server merges into the
//! current state and writes the merged row back.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    pub server_name: String,
    pub collector_stats_interval_ms: u64,
    pub collector_processes_interval_ms: u64,
    pub collector_docker_interval_ms: u64,
    pub collector_smart_interval_ms: u64,
    pub rollup_tick_interval_ms: u64,
    pub retention_tick_interval_ms: u64,
    /// Unix seconds of the last persisted change to `server_config`.
    pub updated_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct UpdateConfigRequest {
    pub server_name: Option<String>,
    pub collector_stats_interval_ms: Option<u64>,
    pub collector_processes_interval_ms: Option<u64>,
    pub collector_docker_interval_ms: Option<u64>,
    pub collector_smart_interval_ms: Option<u64>,
    pub rollup_tick_interval_ms: Option<u64>,
    pub retention_tick_interval_ms: Option<u64>,
}

// ─── Retention policy ───────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct RetentionPolicyDto {
    pub resource: String,
    pub resolution: String,
    pub keep_seconds: i64,
}

#[derive(Debug, Serialize)]
pub struct RetentionResponse {
    pub policies: Vec<RetentionPolicyDto>,
}

/// Batch update: every entry must name an existing (resource, resolution)
/// pair — the seeded set is fixed, this endpoint only tunes `keep_seconds`.
#[derive(Debug, Deserialize)]
pub struct UpdateRetentionRequest {
    pub policies: Vec<RetentionPolicyUpdate>,
}

#[derive(Debug, Deserialize)]
pub struct RetentionPolicyUpdate {
    pub resource: String,
    pub resolution: String,
    pub keep_seconds: i64,
}

// ─── Resolutions ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ResolutionDto {
    pub name: String,
    pub interval_seconds: i64,
    pub rollup_from: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct ResolutionsResponse {
    pub resolutions: Vec<ResolutionDto>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateResolutionRequest {
    pub enabled: bool,
}
