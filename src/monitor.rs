use self::models::ServerDescription;

use log::debug;
use sysinfo::{CpuRefreshKind, RefreshKind, System};

mod config_exceeds;
pub mod models;
pub mod persistence;
pub mod system_monitor;

pub async fn init() -> Result<(), ()> {
    let monitor = system_monitor::SystemMonitor::new();
    monitor.start_monitoring().await;
    debug!("System monitor started");

    // TODO(isaidsari): Check sysinfo library has support for current platform
    Ok(())
}

pub fn get_default_server_desc() -> ServerDescription {
    let mut system = System::new_all();
    system.refresh_specifics(RefreshKind::new().with_cpu(CpuRefreshKind::everything()));

    let cpu = system.cpus().first().unwrap().brand();
    let mem = (system.total_memory() as f64) / 1024.0 / 1024.0 / 1024.0;
    let name = System::host_name().unwrap_or("Unknown".to_string());

    let description = System::long_os_version().unwrap_or("Unknown".to_string())
        + " • "
        + System::cpu_arch().unwrap_or_default().as_str()
        + " • "
        + cpu
        + " • "
        + &format!("{:.1}GB", &mem);

    ServerDescription { name, description }
}
