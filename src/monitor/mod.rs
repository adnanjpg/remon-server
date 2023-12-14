use log::debug;
use serde::{Deserialize, Serialize};
use sysinfo::{CpuExt, System, SystemExt};

pub mod persistence;
pub mod system_monitor;

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

// get-cpu-status

#[derive(Debug, Deserialize, Serialize)]
pub struct GetCpuStatusRequest {
    pub start_time: i64,
    pub end_time: i64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct CpuCoreInfo {
    // the id of the cpu chip, consists from key info like vendor_id, brand, etc.
    pub cpu_id: String,
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

// get-disk-status

#[derive(Debug, Deserialize, Serialize)]
pub struct GetDiskStatusRequest {
    pub start_time: i64,
    pub end_time: i64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct SingleDiskInfo {
    // the id of the disk, consists from key info like name, fs, etc.
    pub disk_id: String,
    pub total: f64,
    pub available: f64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct DiskFrameStatus {
    // usage for each disk
    pub disks_usage: Vec<SingleDiskInfo>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct DiskStatusData {
    // usage for each frame, the size
    // of the frame is defined in the config
    // where the user picks the frequency
    // of the monitoring
    pub frames: Vec<DiskFrameStatus>,
}

// get-hardware-info
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct HardwareCpuInfo {
    // the id of the cpu chip, consists from key info like vendor_id, brand, etc.
    pub cpu_id: String,
    pub core_count: i32,
    pub vendor_id: String,
    pub brand: String,
    pub last_check: i64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct HardwareDiskInfo {
    // the id of the disk, consists from key info like name, fs, etc.
    pub disk_id: String,
    pub name: String,
    pub last_check: i64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct HardwareInfo {
    pub cpu_info: Vec<HardwareCpuInfo>,
    pub disks_info: Vec<HardwareDiskInfo>,
}

pub async fn init() -> Result<(), ()> {
    match persistence::init_db().await {
        Ok(val) => val,
        Err(e) => {
            debug!("Database initialization failed: {:?}", e);
            return Err(());
        }
    };

    let monitor = system_monitor::SystemMonitor::new();
    monitor.start_monitoring().await;
    debug!("System monitor started");

    Ok(())
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
