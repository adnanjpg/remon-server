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
    pub start_time: Option<i64>, // ms epoch
    pub end_time: Option<i64>,   // ms epoch
    pub tail: Option<usize>,
}

/// Request for delete operations with force flag
#[derive(Debug, Deserialize, Serialize)]
pub struct ForceDeleteRequest {
    #[serde(default)]
    pub force: bool,
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
    pub status: String, // running, exited, paused, restarting, dead, created
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
    pub block_read_bytes: i64,
    pub block_write_bytes: i64,
    pub pids: i64,
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

/// Real-time container stats (single snapshot)
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerRealtimeStats {
    pub container_id: String,
    pub cpu_percent: f64,
    pub memory_usage: i64,
    pub memory_limit: i64,
    pub memory_percent: f64,
    pub network_rx_bytes: i64,
    pub network_tx_bytes: i64,
    pub block_read_bytes: i64,
    pub block_write_bytes: i64,
    pub pids: i64,
    pub timestamp: i64,
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
    /// Container runtime backend: "docker" or "podman"
    pub backend: Option<String>,
    /// API version
    pub api_version: Option<String>,
    /// Host operating system
    pub os: Option<String>,
    /// Host architecture
    pub arch: Option<String>,
}

// ============ New models for extended Docker features ============

/// Port binding information
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PortBinding {
    pub container_port: u16,
    pub host_port: Option<u16>,
    pub host_ip: Option<String>,
    pub protocol: String, // tcp, udp
}

/// Volume mount information
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VolumeMount {
    pub source: String,
    pub destination: String,
    pub mode: String,
    pub rw: bool,
    pub mount_type: String, // bind, volume, tmpfs
}

/// Network settings for a container
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerNetworkSettings {
    pub network_name: String,
    pub network_id: String,
    pub ip_address: Option<String>,
    pub gateway: Option<String>,
    pub mac_address: Option<String>,
}

/// Health check details
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HealthDetails {
    pub status: String, // healthy, unhealthy, starting, none
    pub failing_streak: i64,
    pub log: Vec<HealthLogEntry>,
}

/// Health check log entry
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HealthLogEntry {
    pub start: String,
    pub end: String,
    pub exit_code: i64,
    pub output: String,
}

/// Restart policy
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RestartPolicy {
    pub name: String, // no, always, on-failure, unless-stopped
    pub maximum_retry_count: i64,
}

/// Full container inspect details
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContainerInspectDetails {
    pub info: ContainerInfo,
    pub state: ContainerState,
    pub ports: Vec<PortBinding>,
    pub volumes: Vec<VolumeMount>,
    pub env: Vec<String>,
    pub cmd: Vec<String>,
    pub entrypoint: Vec<String>,
    pub networks: Vec<ContainerNetworkSettings>,
    pub restart_policy: Option<RestartPolicy>,
    pub health: Option<HealthDetails>,
    pub labels: std::collections::HashMap<String, String>,
}

/// Response for container inspect endpoint
#[derive(Debug, Serialize, Deserialize)]
pub struct ContainerInspectResponse {
    pub container: ContainerInspectDetails,
}

/// Docker image information
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ImageInfo {
    pub id: String,
    pub repo_tags: Vec<String>,
    pub repo_digests: Vec<String>,
    pub created: i64,
    pub size: i64,
    pub virtual_size: Option<i64>,
    pub labels: std::collections::HashMap<String, String>,
}

/// Response for image list endpoint
#[derive(Debug, Serialize, Deserialize)]
pub struct ListImagesResponse {
    pub images: Vec<ImageInfo>,
}

/// Result of a prune operation
#[derive(Debug, Serialize, Deserialize)]
pub struct PruneResult {
    pub deleted_count: usize,
    pub deleted_items: Vec<String>,
    pub space_reclaimed: i64,
}
