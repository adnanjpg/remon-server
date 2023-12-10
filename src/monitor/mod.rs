use log::debug;
use serde::{Deserialize, Serialize};
use sysinfo::{CpuExt, System, SystemExt};

pub mod persistence;
pub mod system_monitor;
pub use persistence::{fetch_monitor_status, insert_monitor_config};

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerDescription {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct MonitorConfig {
    pub cpu_threshold: f64,
    pub mem_threshold: f64,
    pub storage_threshold: f64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::Type)]
pub struct DiskStatus {
    pub name: String,
    pub total: u64,
    pub available: u64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::Type)]
pub struct CoreInfo {
    pub cpu_freq: f64,
    pub cpu_usage: f64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct CpuStatus {
    pub vendor_id: String,
    pub brand: String,
    pub cpu_usage: Vec<CoreInfo>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct MemStatus {
    pub total: u64,
    pub available: u64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct MonitorStatus {
    pub cpu_usage: CpuStatus,
    pub mem_usage: MemStatus,
    pub storage_usage: Vec<DiskStatus>,
    pub last_check: i64,
}

// get-cpu-status

#[derive(Debug, Deserialize, Serialize)]
pub struct GetCpuStatusRequest {
    pub start_time: i64,
    pub end_time: i64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::Type)]
pub struct CpuCoreInfo {
    pub freq: f64,
    pub usage: f64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct CpuFrameStatus {
    pub cores_usage: Vec<CpuCoreInfo>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct CpuStatusData {
    pub frames: Vec<CpuFrameStatus>,
}

// get-mem-status

#[derive(Debug, Deserialize, Serialize)]
pub struct GetMemStatusRequest {
    pub start_time: i64,
    pub end_time: i64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct MemFrameStatus {
    pub total: u64,
    pub available: u64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct MemStatusData {
    pub frames: Vec<MemFrameStatus>,
}

// get-hardware-info
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct HardwareCpuInfo {
    pub vendor_id: String,
    pub brand: String,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct HardwareDiskInfo {
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct HardwareInfo {
    pub cpu_info: HardwareCpuInfo,
    pub disks_info: Vec<HardwareDiskInfo>,
    pub last_check: i64,
}

pub async fn init() {
    persistence::init_db().await;

    let monitor = system_monitor::SystemMonitor::new();
    monitor.start_monitoring().await;
    debug!("System monitor started");
}

const LE_DOT: &str = " • ";

pub fn get_default_server_desc() -> ServerDescription {
    let mut system = System::new_all();
    system.refresh_all();

    // TODO: add pc name / user name
    let cpu = system.cpus()[0].brand();
    let mem = (system.total_memory() as f64) / 1024.0 / 1024.0 / 1024.0;
    let name = system.name().unwrap_or("Unknown".to_string())
        + LE_DOT
        + match system.global_cpu_info().vendor_id() {
            "GenuineIntel" => "Intel",
            other => other,
        };
    let description = system.long_os_version().unwrap_or("Unknown".to_string())
        + LE_DOT
        + cpu
        + LE_DOT
        + &format!("{:.1}GB", &mem);

    ServerDescription { name, description }
}
