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
    pub rollup_tick_interval_ms: u64,
    pub retention_tick_interval_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct UpdateConfigRequest {
    pub server_name: Option<String>,
    pub collector_stats_interval_ms: Option<u64>,
    pub collector_processes_interval_ms: Option<u64>,
    pub collector_docker_interval_ms: Option<u64>,
    pub rollup_tick_interval_ms: Option<u64>,
    pub retention_tick_interval_ms: Option<u64>,
}
