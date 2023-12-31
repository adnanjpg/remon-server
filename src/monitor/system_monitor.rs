use super::{
    models::{
        get_cpu_status::{CpuCoreInfo, CpuFrameStatus, CpuStatusData},
        get_disk_status::{DiskFrameStatus, SingleDiskInfo, DiskStatusData},
        get_hardware_info::{HardwareCpuInfo, HardwareDiskInfo, HardwareInfo, HardwareMemInfo},
        get_mem_status::{MemFrameStatus, MemStatusData, SingleMemInfo},
    },
    persistence::{
        insert_cpu_status_frame, insert_disk_status_frame, insert_hardware_info,
        insert_mem_status_frame,
    },
    config_exceeds::check_thresholds,
};

use blake3::Hasher;
use chrono::Utc;
use log::{debug, error};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
    vec,
};
use sysinfo::{Cpu, CpuRefreshKind, Disk, Disks, MemoryRefreshKind, RefreshKind, System};
use tokio::time;

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

impl CpuId for Cpu {
    fn get_cpu_id(&self) -> String {
        let the_str: String = format!("{}{}", self.vendor_id(), self.brand());
        let mut hasher = Hasher::new();
        hasher.update(the_str.as_bytes());

        let hash = hasher.finalize();

        let hashed = hash.to_string();

        hashed
    }
}

trait DiskId {
    fn get_disk_id(&self) -> String;
}

impl DiskId for Disk {
    fn get_disk_id(&self) -> String {
        let the_str: String = format!(
            "{}{}{}{}{}{}",
            self.name().to_string_lossy(),
            self.file_system().to_str().unwrap_or_default(),
            format!("{:?}", self.kind()),
            if self.is_removable() { "yes" } else { "no" },
            self.mount_point().to_string_lossy(),
            self.total_space(),
        );

        debug!("the str: {}", the_str);

        let mut hasher = Hasher::new();

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
        // TODO(isaidsari): put it more convenient place
        if !sysinfo::IS_SUPPORTED_SYSTEM {
            error!("sysinfo is not supported on this system");
            return;
        }

        fn get_last_check() -> i64 {
            Utc::now().timestamp_millis()
        }

        let should_exit_clone = Arc::clone(&self.should_exit);
        // rust doesn't allow us to move self into the closure, so we have to clone it
        let check_interval = self.check_interval;

        tokio::spawn(async move {
            let mut system = System::new();
            let mut disks = Disks::new_with_refreshed_list();

            while !*should_exit_clone.lock().unwrap() {
                let start_time = Instant::now();

                // refresh all system info WARN: this takes too much time
                // let mut system = sysinfo::System::new_all();
                // system.refresh_all();

                // Refresh system information
                system.refresh_specifics(
                    RefreshKind::new()
                        // TODO(isaidsari): check if we need to refresh all of them
                        .with_cpu(CpuRefreshKind::everything())
                        .with_memory(MemoryRefreshKind::everything()),
                );

                // Refresh disks information, since with sysinfo v0.30 it's not refreshed with the System
                // NOTE: if a disk is added or removed, this method won't take it into account
                disks.refresh();

                // disks
                let mut disk_usage: DiskFrameStatus = DiskFrameStatus {
                    id: -1,
                    last_check: get_last_check(),
                    disks_usage: vec![],
                };
                let mut disks_info: Vec<HardwareDiskInfo> = vec![];
                for disk in &disks {
                    let disk_id = disk.get_disk_id();
                    let disk_name = match disk.name().to_os_string().into_string() {
                        Ok(name) => {
                            if name.is_empty() {
                                // if the name is empty, it's probably a local disk
                                // TODO(isaidsari): add C: etc
                                "Local Disk".to_string()
                            } else {
                                name
                            }
                        }
                        Err(_) => "".to_string(),
                    };

                    disk_usage.disks_usage.push(SingleDiskInfo {
                        id: -1,
                        frame_id: -1,
                        disk_id: disk_id.to_string(),
                        // sqlx doesn't support u64
                        available: disk.available_space() as i64,
                    });

                    disks_info.push(HardwareDiskInfo {
                        id: -1,
                        fs_type: disk
                            .file_system()
                            .to_os_string()
                            .into_string()
                            .unwrap_or_default(),
                        is_removable: disk.is_removable(),
                        kind: format!("{:?}", disk.kind()),
                        mount_point: disk.mount_point().to_string_lossy().to_string(),
                        // sqlx doesn't support u64
                        total_space: disk.total_space() as i64,
                        disk_id: disk_id.to_string(),
                        name: disk_name,
                        last_check: get_last_check(),
                    });
                }

                // cpu
                let all_cpus = system.cpus();
                let mut cpu_usage: CpuFrameStatus = CpuFrameStatus {
                    id: -1,
                    last_check: get_last_check(),
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
                let mem_info: Vec<HardwareMemInfo> = vec![HardwareMemInfo {
                    id: -1,
                    mem_id: "1".to_string(),
                    last_check: get_last_check(),
                    total_space: system.total_memory() as i64,
                }];

                let mem_usage: MemFrameStatus = MemFrameStatus {
                    id: -1,
                    last_check: get_last_check(),
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
                    disks_info: disks_info.clone(),
                    mem_info: mem_info.clone(),
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
                if let Err(e) = insert_disk_status_frame(&disk_usage).await {
                    error!("failed to insert disk status: {}", e);
                }
                if let Err(e) = insert_mem_status_frame(&mem_usage).await {
                    error!("failed to insert mem status: {}", e);
                }

                let cpu_status = &CpuStatusData {
                    frames: vec![cpu_usage],
                };
                let mem_status = &MemStatusData {
                    frames: vec![mem_usage],
                };
                let disk_status = &DiskStatusData {
                    frames: vec![disk_usage],
                };

                // TODO(adnanjpg): run on a different thread with a different interval
                check_thresholds(cpu_status, mem_status, &mem_info, disk_status, &disks_info).await;

                let duration = match check_interval.checked_sub(elapsed_time) {
                    Some(duration) => duration,
                    None => {
                        error!("check interval is less than elapsed time");
                        Duration::default()
                    }
                };
                time::sleep(duration).await;
            }
        });
    }

    // TODO(isaidsari): graceful shutdown
    #[allow(dead_code)]
    pub fn stop_monitoring(&self) {
        *self.should_exit.lock().unwrap() = true;
    }
}
