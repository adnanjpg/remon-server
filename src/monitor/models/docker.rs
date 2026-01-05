use serde::{Deserialize, Serialize};

/// Request for getting Docker stats with date range
#[derive(Debug, Deserialize, Serialize)]
pub struct GetDockerStatsRequest {
    pub start_time: i64,
    pub end_time: i64,
}

/// Request for getting container logs
#[derive(Debug, Deserialize, Serialize)]
pub struct GetContainerLogsRequest {
    pub tail: Option<usize>,
}

/// Container basic information (stored in DB)
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct ContainerInfo {
    pub id: i64,
    pub container_id: String,
    pub name: String,
    pub image: String,
    pub created_at: i64,
    pub last_seen: i64,
}

/// Container current state
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerState {
    pub status: String,        // running, exited, paused, restarting, dead, created
    pub running: bool,
    pub paused: bool,
    pub restarting: bool,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i64>,
}

/// Container resource stats (per container in a frame)
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct ContainerStats {
    pub id: i64,
    pub frame_id: i64,
    pub container_id: String,
    pub cpu_percent: f64,
    pub memory_usage: i64,
    pub memory_limit: i64,
    pub network_rx_bytes: i64,
    pub network_tx_bytes: i64,
}

/// Docker stats frame (timestamp + all container stats)
#[derive(Debug, Serialize, Deserialize)]
pub struct DockerStatsFrame {
    pub id: i64,
    pub last_check: i64,
    pub container_stats: Vec<ContainerStats>,
}

/// Full container details for API response
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerDetails {
    pub info: ContainerInfo,
    pub state: ContainerState,
}

/// API response for container list
#[derive(Debug, Serialize, Deserialize)]
pub struct ListContainersResponse {
    pub containers: Vec<ContainerDetails>,
}

/// API response for Docker stats
#[derive(Debug, Serialize, Deserialize)]
pub struct GetDockerStatsResponse {
    pub frames: Vec<DockerStatsFrame>,
}

/// API response for container logs
#[derive(Debug, Serialize, Deserialize)]
pub struct GetContainerLogsResponse {
    pub logs: String,
}

/// Docker action result
#[derive(Debug, Serialize, Deserialize)]
pub struct DockerActionResponse {
    pub success: bool,
    pub message: String,
}

/// Docker availability status
#[derive(Debug, Serialize, Deserialize)]
pub struct DockerStatusResponse {
    pub available: bool,
    pub version: Option<String>,
}
