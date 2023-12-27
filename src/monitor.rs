use log::debug;
use sysinfo::{CpuExt, System, SystemExt};

use self::models::ServerDescription;

mod config_exceeds;
pub mod models;
pub mod persistence;
pub mod system_monitor;

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
