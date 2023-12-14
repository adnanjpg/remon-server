use crate::monitor::persistence::{fetch_monitor_configs, insert_monitor_status};
use crate::monitor::{CoreInfo, CpuStatus, DiskStatus, MemStatus, MonitorConfig, MonitorStatus};
use log::{debug, error, info, warn};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use sysinfo::{CpuExt, CpuRefreshKind, DiskExt, RefreshKind, SystemExt};
use tokio::time;

// TODO(isaidsari): make it configurable
const CHECK_INTERVAL: Duration = Duration::from_secs(30);

pub struct SystemMonitor {
    should_exit: Arc<Mutex<bool>>,
    check_interval: Duration,
}

impl SystemMonitor {
    pub fn new() -> Self {
        let should_exit = Arc::new(Mutex::new(false));
        Self {
            should_exit,
            check_interval: CHECK_INTERVAL,
        }
    }

    pub async fn start_monitoring(&self) {
        let should_exit_clone = Arc::clone(&self.should_exit);
        tokio::spawn(async move {
            let mut system = sysinfo::System::new();

            while !*should_exit_clone.lock().unwrap() {
                let start_time = std::time::Instant::now();

                // refresh all system info WARN: this takes too much time
                // let mut system = sysinfo::System::new_all();
                // system.refresh_all();

                // Refresh system information
                system.refresh_specifics(
                    RefreshKind::new()
                        .with_cpu(CpuRefreshKind::everything())
                        .with_memory()
                        .with_disks_list()
                        .with_disks(),
                );

                let mut storage_usage: Vec<DiskStatus> = vec![];
                for disk in system.disks() {
                    storage_usage.push(DiskStatus {
                        name: disk.name().to_string_lossy().to_string(),
                        total: disk.total_space(),
                        available: disk.available_space(),
                    });
                }

                let mut cpu_usage: CpuStatus = CpuStatus {
                    vendor_id: system.global_cpu_info().vendor_id().to_string(),
                    brand: system.global_cpu_info().brand().to_string(),
                    cpu_usage: vec![],
                };
                for cpu in system.cpus() {
                    cpu_usage.cpu_usage.push(CoreInfo {
                        cpu_freq: cpu.frequency() as f64,
                        cpu_usage: cpu.cpu_usage() as f64 / 100.0,
                    });
                }

                let mem_usage: MemStatus = MemStatus {
                    total: system.total_memory(),
                    available: system.free_memory(),
                };

                let status = MonitorStatus {
                    cpu_usage: cpu_usage,
                    mem_usage: mem_usage,
                    storage_usage,
                    last_check: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64,
                };

                let elapsed_time = start_time.elapsed();

                debug!("time to refresh: {:?}", elapsed_time);

                info!("status: {:?}", status);

                if let Err(e) = insert_monitor_status(&status).await {
                    error!("failed to insert monitor status: {}", e);
                }

                check_thresholds(&status).await;

                // make it configurable
                let duration = CHECK_INTERVAL - elapsed_time;
                time::sleep(duration).await;
            }
        });
    }

    pub fn stop_monitoring(&self) {
        *self.should_exit.lock().unwrap() = true;
    }
}

async fn check_thresholds(status: &MonitorStatus) {
    let configs = fetch_monitor_configs().await.unwrap_or_else(|e| {
        error!("failed to fetch monitor configs: {}", e);
        vec![]
    });

    for config in configs {
        let (cpu, mem, storage) = compare_status(&config, status);

        if cpu || mem || storage {
            warn!(
                "thresholds exceeded for {:?} : cpu: {}, mem: {}, storage: {}",
                config, cpu, mem, storage
            );
            // TODO(isaidsari): send notification
        }
    }
}

fn compare_status(config: &MonitorConfig, status: &MonitorStatus) -> (bool, bool, bool) {
    (true, true, true)
}
