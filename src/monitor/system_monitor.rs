use crate::monitor::persistence::{
    fetch_monitor_configs, insert_cpu_status_frame, insert_disk_status_frame, insert_hardware_info,
    insert_mem_status_frame,
};
use crate::monitor::{
    CpuCoreInfo, CpuFrameStatus, DiskFrameStatus, HardwareCpuInfo, HardwareDiskInfo, HardwareInfo,
    HardwareMemInfo, MemFrameStatus, MonitorConfig, SingleDiskInfo, SingleMemInfo,
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
    Duration::from_millis(10000)
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

        debug!("the str: {}", the_str);

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
                let all_disks = system.disks();
                let disks_last_check = chrono::Utc::now().timestamp();
                let mut storage_usage: DiskFrameStatus = DiskFrameStatus {
                    id: -1,
                    last_check: disks_last_check,
                    disks_usage: vec![],
                };
                let mut disks_info: Vec<HardwareDiskInfo> = vec![];
                for disk in all_disks {
                    let disk_id = &disk.get_disk_id();

                    storage_usage.disks_usage.push(SingleDiskInfo {
                        id: -1,
                        frame_id: -1,
                        disk_id: disk_id.to_string(),
                        // sqlx doesn't support u64
                        available: disk.available_space() as i64,
                    });

                    disks_info.push(HardwareDiskInfo {
                        id: -1,
                        fs_type: disk.file_system().iter().map(|c| *c as char).collect(),
                        is_removable: disk.is_removable(),
                        kind: format!("{:?}", disk.kind()),
                        mount_point: disk.mount_point().to_string_lossy().to_string(),
                        // sqlx doesn't support u64
                        total_space: disk.total_space() as i64,
                        disk_id: disk_id.to_string(),
                        name: disk.name().to_string_lossy().to_string(),
                        last_check: get_last_check(),
                    });
                }

                // cpu
                let all_cpus = system.cpus();
                let cpu_last_check = chrono::Utc::now().timestamp();
                let mut cpu_usage: CpuFrameStatus = CpuFrameStatus {
                    id: -1,
                    last_check: cpu_last_check,
                    cores_usage: vec![],
                };
                let mut cpu_info: Vec<HardwareCpuInfo> = vec![];
                for cpu in all_cpus {
                    let cpu_id = &cpu.get_cpu_id();

                    cpu_usage.cores_usage.push(CpuCoreInfo {
                        id: -1,
                        frame_id: -1,
                        cpu_id: cpu_id.to_string(),
                        freq: cpu.frequency() as i64,
                        usage: cpu.cpu_usage() as i64,
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
                            id: -1,
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
                let mem_last_check = chrono::Utc::now().timestamp();
                let mem_info: Vec<HardwareMemInfo> = vec![HardwareMemInfo {
                    id: -1,
                    mem_id: "1".to_string(),
                    last_check: mem_last_check,
                    total_space: system.total_memory() as i64,
                }];

                let mem_usage: MemFrameStatus = MemFrameStatus {
                    id: -1,
                    last_check: mem_last_check,
                    mems_usage: vec![SingleMemInfo {
                        id: -1,
                        frame_id: -1,
                        // constant, as there's only one mem
                        mem_id: "1".to_string(),
                        // sqlx doesn't support u64
                        available: system.free_memory() as i64,
                    }],
                };

                let hardware_info = HardwareInfo {
                    cpu_info,
                    disks_info,
                    mem_info,
                };

                let elapsed_time = start_time.elapsed();

                debug!("ms amount it took to refresh: {:?}", elapsed_time);

                // TODO(adnanjpg): make this one run only once on startup, not every time
                if let Err(e) = insert_hardware_info(&hardware_info).await {
                    error!("failed to insert hardware info: {}", e);
                };

                if let Err(e) = insert_cpu_status_frame(&cpu_usage).await {
                    error!("failed to insert cpu status: {}", e);
                }
                if let Err(e) = insert_disk_status_frame(&storage_usage).await {
                    error!("failed to insert disk status: {}", e);
                }
                if let Err(e) = insert_mem_status_frame(&mem_usage).await {
                    error!("failed to insert mem status: {}", e);
                }

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
