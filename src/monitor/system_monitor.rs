use crate::monitor::persistence::{fetch_monitor_configs, insert_hardware_info};
use crate::monitor::{
    CpuCoreInfo, CpuFrameStatus, DiskFrameStatus, HardwareCpuInfo, HardwareDiskInfo, HardwareInfo,
    MemFrameStatus, MonitorConfig, SingleDiskInfo,
};
use log::{debug, error, warn};
use std::vec;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use sysinfo::{CpuExt, CpuRefreshKind, DiskExt, RefreshKind, SystemExt};
use tokio::time;

use super::{CpuStatusData, DiskStatusData, MemStatusData};

// TODO(isaidsari): make it configurable
pub fn get_check_interval() -> Duration {
    Duration::from_secs(30)
}

pub struct SystemMonitor {
    should_exit: Arc<Mutex<bool>>,
    check_interval: Duration,
}

trait CpuId {
    fn get_cpu_id(&self) -> String;
}

impl CpuId for sysinfo::Cpu {
    fn get_cpu_id(&self) -> String {
        let the_str: String = format!("{}{}", self.vendor_id(), self.brand());
        let mut hasher = blake3::Hasher::new();
        hasher.update(the_str.as_bytes());

        let hash = hasher.finalize();

        let hashed = hash.to_string();

        hashed
    }
}

trait DiskId {
    fn get_disk_id(&self) -> String;
}

impl DiskId for sysinfo::Disk {
    fn get_disk_id(&self) -> String {
        let the_str: String = format!(
            "{}{}{}{}{}{}",
            self.name().to_string_lossy(),
            self.file_system()
                .iter()
                .map(|c| *c as char)
                .collect::<Vec<_>>()
                .iter()
                .collect::<String>(),
            format!("{:?}", self.kind()),
            if self.is_removable() { "yes" } else { "no" },
            self.mount_point().to_string_lossy(),
            self.total_space(),
        );

        let mut hasher = blake3::Hasher::new();

        hasher.update(the_str.as_bytes());

        let hash = hasher.finalize();

        let hashed = hash.to_string();

        hashed
    }
}

impl SystemMonitor {
    pub fn new() -> Self {
        let should_exit = Arc::new(Mutex::new(false));
        Self {
            should_exit,
            check_interval: get_check_interval(),
        }
    }

    pub async fn start_monitoring(&self) {
        fn get_last_check() -> i64 {
            chrono::Utc::now().timestamp()
        }

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

                // storage
                let mut storage_usage: DiskFrameStatus = DiskFrameStatus {
                    disks_usage: vec![],
                };
                let mut storage_info: Vec<HardwareDiskInfo> = vec![];
                for disk in system.disks() {
                    let disk_id = &disk.get_disk_id();

                    storage_usage.disks_usage.push(SingleDiskInfo {
                        disk_id: disk_id.to_string(),
                        total: disk.total_space() as f64,
                        available: disk.available_space() as f64,
                    });

                    storage_info.push(HardwareDiskInfo {
                        disk_id: disk_id.to_string(),
                        name: disk.name().to_string_lossy().to_string(),
                        last_check: get_last_check(),
                    });
                }

                // cpu
                let mut cpu_usage: CpuFrameStatus = CpuFrameStatus {
                    cores_usage: vec![],
                };
                let mut cpu_info: Vec<HardwareCpuInfo> = vec![];
                let all_cpus = system.cpus();
                for cpu in all_cpus {
                    let cpu_id = &cpu.get_cpu_id();

                    cpu_usage.cores_usage.push(CpuCoreInfo {
                        cpu_id: cpu_id.to_string(),
                        freq: cpu.frequency() as f64,
                        usage: cpu.cpu_usage() as f64,
                    });

                    let cpu_id_owned = cpu_id.to_owned();
                    if cpu_info.iter().any(|c| c.cpu_id == cpu_id_owned) {
                        continue;
                    } else {
                        let core_count = all_cpus
                            .iter()
                            .filter(|c| c.get_cpu_id() == cpu_id_owned)
                            .count();

                        let new_info = HardwareCpuInfo {
                            cpu_id: cpu_id_owned,
                            core_count: core_count as i32,
                            brand: cpu.brand().to_string(),
                            vendor_id: cpu.vendor_id().to_string(),
                            last_check: get_last_check(),
                        };

                        cpu_info.push(new_info);
                    }
                }

                // mem
                let mem_usage: MemFrameStatus = MemFrameStatus {
                    total: system.total_memory(),
                    available: system.free_memory(),
                };

                let hardware_info = HardwareInfo {
                    cpu_info,
                    disks_info: storage_info,
                };

                let elapsed_time = start_time.elapsed();

                debug!("ms amount it took to refresh: {:?}", elapsed_time);

                if let Err(e) = insert_hardware_info(&hardware_info).await {
                    error!("failed to insert hardware info: {}", e);
                };

                check_thresholds(
                    &CpuStatusData {
                        frames: vec![cpu_usage],
                    },
                    &MemStatusData {
                        frames: vec![mem_usage],
                    },
                    &DiskStatusData {
                        frames: vec![storage_usage],
                    },
                )
                .await;

                // make it configurable
                let duration = get_check_interval() - elapsed_time;
                time::sleep(duration).await;
            }
        });
    }

    pub fn stop_monitoring(&self) {
        *self.should_exit.lock().unwrap() = true;
    }
}

async fn check_thresholds(
    cpu_status: &CpuStatusData,
    mem_status: &MemStatusData,
    storage_status: &DiskStatusData,
) {
    let configs = fetch_monitor_configs().await.unwrap_or_else(|e| {
        error!("failed to fetch monitor configs: {}", e);
        vec![]
    });

    for config in configs {
        let (cpu, mem, storage) = compare_status(&config, cpu_status, mem_status, storage_status);

        if cpu || mem || storage {
            warn!(
                "thresholds exceeded for {:?} : cpu: {}, mem: {}, storage: {}",
                config, cpu, mem, storage
            );
            // TODO(isaidsari): send notification
        }
    }
}

fn compare_cpu_status(config: &MonitorConfig, status: &CpuStatusData) -> bool {
    true
}
fn compare_mem_status(config: &MonitorConfig, status: &MemStatusData) -> bool {
    true
}
fn compare_storage_status(config: &MonitorConfig, status: &DiskStatusData) -> bool {
    true
}
fn compare_status(
    config: &MonitorConfig,
    cpu_status: &CpuStatusData,
    mem_status: &MemStatusData,
    storage_status: &DiskStatusData,
) -> (bool, bool, bool) {
    (
        compare_cpu_status(config, cpu_status),
        compare_mem_status(config, mem_status),
        compare_storage_status(config, storage_status),
    )
}
