pub mod get_cpu_status;
pub mod get_disk_status;
pub mod get_mem_status;

pub mod get_hardware_info;

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerDescription {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateInfoRequest {
    pub cpu_threshold: f64,
    pub mem_threshold: f64,
    pub disk_threshold: f64,
}

#[derive(Debug, sqlx::FromRow)]
pub struct MonitorConfig {
    pub id: i64,
    pub device_id: String,
    pub cpu_threshold: f64,
    pub mem_threshold: f64,
    pub disk_threshold: f64,
    pub updated_at: i64,
}
