use log::{debug, error};
use std::error::Error;
use sysinfo::{CpuRefreshKind, Pid, ProcessRefreshKind, RefreshKind, System};

use self::models::{ProcessInfo, ServerDescription};

mod config_exceeds;
pub mod models;
pub mod persistence;
pub mod system_monitor;

pub async fn init() -> Result<(), Box<dyn Error>> {
    let monitor = system_monitor::SystemMonitor::new();
    monitor.start_monitoring().await;
    if !sysinfo::IS_SUPPORTED_SYSTEM {
        return Err("System not supported".into());
    } else {
        Ok(())
    }
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

pub async fn get_process_list() -> Vec<ProcessInfo> {
    let mut system = System::new_with_specifics(
        RefreshKind::new().with_processes(
            ProcessRefreshKind::new()
                .with_cpu()
                .with_memory()
                .with_cmd(sysinfo::UpdateKind::Always),
        ),
    );

    system.refresh_processes();
    // wait for a while to get the updated process list
    tokio::time::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL).await;
    system.refresh_processes();

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

    // sort list descending order by cpu and return
    processes.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap());

    return processes;
}

pub async fn kill_process(pid: u32) -> Result<(), String> {
    let system =
        System::new_with_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));

    let process = system.process(Pid::from_u32(pid));

    match process {
        Some(process) => {
            debug!(
                "Killing process with pid {} , name {}",
                process.pid(),
                process.name()
            );
            let success = process.kill();
            if success {
                debug!("Process with pid {} killed successfully", pid);
                Ok(())
            } else {
                error!("Failed to kill process with pid {}", pid);
                Err(format!("Failed to kill process with pid {}", pid))
            }
        }
        None => Err(format!("Process with pid {} not found", pid)),
    }
}
