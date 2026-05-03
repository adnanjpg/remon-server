//! REST-specific Docker DTOs.
//!
//! These types form an explicit boundary between bollard's wire schemas and
//! the public HTTP API. They serve two purposes:
//! 1. Stability: bollard struct field changes do not become breaking API changes.
//! 2. Confidentiality: sensitive inspect output (env vars, mount paths,
//!    secrets in driver options, full network detail) is excluded by default.

use serde::{Deserialize, Serialize};

// ===== Status =====

#[derive(Debug, Serialize)]
pub struct DockerStatusResponse {
    pub available: bool,
    pub version: Option<String>,
    pub backend: Option<String>,
    pub api_version: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
}

// ===== Containers =====

#[derive(Debug, Serialize)]
pub struct ContainerInfo {
    pub id: String,
    pub names: Vec<String>,
    pub image: String,
    pub state: String,
    pub status: String,
    pub created: i64,
}

#[derive(Debug, Serialize)]
pub struct ListContainersResponse {
    pub containers: Vec<ContainerInfo>,
}

#[derive(Debug, Serialize)]
pub struct DockerActionResponse {
    pub success: bool,
    pub message: String,
}

// ===== Inspect (whitelist) =====

/// Operational view of a container. Intentionally narrow:
/// - environment variables, mounts, secrets, security opts are NOT exposed
/// - network details limited to id and name; no IP / driver_opts
#[derive(Debug, Serialize)]
pub struct ContainerInspectInfo {
    pub id: Option<String>,
    pub name: Option<String>,
    pub image: Option<String>,
    pub created: Option<String>,
    pub state: Option<ContainerStateInfo>,
    pub ports: Vec<PortMapping>,
    pub networks: Vec<NetworkSummary>,
    pub restart_count: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ContainerStateInfo {
    pub status: Option<String>,
    pub running: Option<bool>,
    pub paused: Option<bool>,
    pub restarting: Option<bool>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub exit_code: Option<i64>,
    pub health: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PortMapping {
    pub container_port: u16,
    pub protocol: String,
    pub host_port: Option<u16>,
}

#[derive(Debug, Serialize)]
pub struct NetworkSummary {
    pub name: String,
    pub network_id: Option<String>,
}

// ===== Logs =====

#[derive(Debug, Deserialize)]
pub struct GetContainerLogsRequest {
    pub tail: Option<usize>,
    pub since: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct GetContainerLogsResponse {
    pub logs: Vec<String>,
}

// ===== Images =====

#[derive(Debug, Serialize)]
pub struct ImageInfo {
    pub id: String,
    pub tags: Vec<String>,
    pub size: i64,
    pub created: i64,
}

#[derive(Debug, Serialize)]
pub struct ListImagesResponse {
    pub images: Vec<ImageInfo>,
}

#[derive(Debug, Deserialize)]
pub struct ForceDeleteRequest {
    #[serde(default)]
    pub force: bool,
}
