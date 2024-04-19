use self::models::{ProcessInfo, ServerDescription};

use log::debug;
use sysinfo::{CpuRefreshKind, ProcessRefreshKind, RefreshKind, System};

use std::time::Instant;

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

pub fn get_process_list() -> Vec<ProcessInfo> {
    let system = System::new_with_specifics(
        RefreshKind::new().with_processes(
            ProcessRefreshKind::new()
                .with_cpu()
                .with_memory()
                .with_cmd(sysinfo::UpdateKind::Always),
        ),
    );

    let mut processes = Vec::new();
    for (pid, process) in system.processes() {
        processes.push(models::ProcessInfo {
            pid: pid.as_u32(),
            name: process.name().to_string(),
            cpu: process.cpu_usage(),
            mem: process.memory(),
            status: process.status().to_string(),
            cmd: process.cmd().to_vec(),
        });
    }

    processes
}

// for testing
fn get_process_list_wmetrics() -> Vec<models::ProcessInfo> {
    let start_total_time = Instant::now();

    // Timing metrics for fetching process information
    let start_process_time = Instant::now();
    let system = System::new_with_specifics(
        RefreshKind::new().with_processes(
            ProcessRefreshKind::new()
                .with_cpu()
                .with_memory()
                .with_cmd(sysinfo::UpdateKind::Always),
        ),
    );
    let process_time = start_process_time.elapsed();

    // Timing metrics for processing process information
    let start_process_processing_time = Instant::now();
    let mut processes = Vec::new();
    for (pid, process) in system.processes() {
        processes.push(models::ProcessInfo {
            pid: pid.as_u32(),
            name: process.name().to_string(),
            cpu: process.cpu_usage(),
            mem: process.memory(),
            status: process.status().to_string(),
            cmd: process.cmd().to_vec(),
        });
    }
    let process_processing_time = start_process_processing_time.elapsed();

    let total_time = start_total_time.elapsed();

    debug!("get_process_list took: {:?}", total_time);
    debug!("get_process_list process took: {:?}", process_time);
    debug!(
        "get_process_list process processing took: {:?}",
        process_processing_time
    );

    processes
}
