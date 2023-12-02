use sysinfo::{CpuExt, System, SystemExt};

pub use sys_status::{ServerDescription, MonitorConfig};
pub mod sys_status;
pub use persistence::{insert_monitor_config, fetch_monitor_status};
mod persistence;

pub async fn init() {
    persistence::init_db().await;
    sys_status::init_sys_status_check();
}

pub fn get_default_server_desc() -> ServerDescription {
    const LE_DOT: &str = " • ";
    
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

