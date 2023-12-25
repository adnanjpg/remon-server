use crate::monitor::models::get_cpu_status::{CpuCoreInfo, CpuFrameStatus, CpuFrameStatusTrait};

use crate::monitor::models::get_disk_status::{DiskFrameStatus, SingleDiskInfo};
use crate::monitor::models::get_hardware_info::{
    HardwareCpuInfo, HardwareDiskInfo, HardwareInfo, HardwareMemInfo,
};
use crate::monitor::models::get_mem_status::{MemFrameStatus, MemStatusData, SingleMemInfo};
use crate::monitor::persistence::{
    fetch_monitor_configs, insert_cpu_status_frame, insert_disk_status_frame, insert_hardware_info,
    insert_mem_status_frame,
};
use log::{debug, error, warn};
use std::collections::HashMap;
use std::vec;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use sysinfo::{CpuExt, CpuRefreshKind, DiskExt, RefreshKind, SystemExt};
use tokio::time;

use super::models::get_cpu_status::CpuStatusData;
use super::models::get_disk_status::{DiskStatusData, DiskStatusDataTrait};
use super::models::get_mem_status::MemStatusDataTrait;
use super::models::MonitorConfig;

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
            chrono::Utc::now().timestamp_millis()
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

                // disk
                let all_disks = system.disks();
                let disks_last_check = chrono::Utc::now().timestamp_millis();
                let mut disk_usage: DiskFrameStatus = DiskFrameStatus {
                    id: -1,
                    last_check: disks_last_check,
                    disks_usage: vec![],
                };
                let mut disks_info: Vec<HardwareDiskInfo> = vec![];
                for disk in all_disks {
                    let disk_id = &disk.get_disk_id();

                    disk_usage.disks_usage.push(SingleDiskInfo {
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
                let cpu_last_check = chrono::Utc::now().timestamp_millis();
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
                let mem_last_check = chrono::Utc::now().timestamp_millis();
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

                check_thresholds(cpu_status, mem_status, &mem_info, disk_status, &disks_info).await;

                // make it configurable
                let duration = get_check_interval() - elapsed_time;
                time::sleep(duration).await;
            }
        });
    }

    // TODO(issaidsari): graceful shutdown
    #[allow(dead_code)]
    pub fn stop_monitoring(&self) {
        *self.should_exit.lock().unwrap() = true;
    }
}

async fn check_thresholds(
    cpu_status: &CpuStatusData,
    mem_status: &MemStatusData,
    mems_info: &Vec<HardwareMemInfo>,
    disk_status: &DiskStatusData,
    disks_info: &Vec<HardwareDiskInfo>,
) {
    let configs = fetch_monitor_configs().await.unwrap_or_else(|e| {
        error!("failed to fetch monitor configs: {}", e);
        vec![]
    });

    for config in configs {
        let (cpu, mem, disk) = statuses_exceeds(
            &config,
            cpu_status,
            mem_status,
            mems_info,
            disk_status,
            disks_info,
        );

        if cpu || mem || disk {
            warn!(
                "thresholds exceeded for {:?} : cpu: {}, mem: {}, disk: {}",
                config, cpu, mem, disk
            );
            // TODO(isaidsari): send notification
        }
    }
}

fn cpu_status_exceeds(config: &MonitorConfig, status: &CpuStatusData) -> bool {
    let means = status
        .frames
        .iter()
        .map(|f| {
            let val = f.cores_usage_mean();

            if let Some(val) = val {
                val
            } else {
                -1.0
            }
        })
        .filter(|&val| val != -1.0);

    for mean in means {
        if mean >= config.cpu_threshold {
            return true;
        }

        continue;
    }

    false
}

fn mem_status_exceeds(
    config: &MonitorConfig,
    status: &MemStatusData,
    mems_info: &Vec<HardwareMemInfo>,
) -> bool {
    let mut totals_map = HashMap::new() as super::models::get_mem_status::MemTotalSpaceMap;
    mems_info.iter().for_each(|f| {
        totals_map.insert(f.mem_id.to_string(), f.total_space);
    });

    let means = status.mems_usage_means_percentages(&totals_map);

    for mean in means {
        if mean.1 as f64 >= config.mem_threshold {
            return true;
        }

        continue;
    }

    false
}

fn disk_status_exceeds(
    config: &MonitorConfig,
    status: &DiskStatusData,
    disks_info: &Vec<HardwareDiskInfo>,
) -> bool {
    let mut totals_map = HashMap::new() as super::models::get_disk_status::DiskTotalSpaceMap;
    disks_info.iter().for_each(|f| {
        totals_map.insert(f.disk_id.to_string(), f.total_space);
    });

    let means = status.disks_usage_means_percentages(&totals_map);

    for mean in means {
        if mean.1 as f64 >= config.disk_threshold {
            return true;
        }

        continue;
    }

    false
}
fn statuses_exceeds(
    config: &MonitorConfig,
    cpu_status: &CpuStatusData,
    mem_status: &MemStatusData,
    mems_info: &Vec<HardwareMemInfo>,
    disk_status: &DiskStatusData,
    disks_info: &Vec<HardwareDiskInfo>,
) -> (bool, bool, bool) {
    (
        cpu_status_exceeds(config, cpu_status),
        mem_status_exceeds(config, mem_status, mems_info),
        disk_status_exceeds(config, disk_status, disks_info),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_status_exceeds_test() {
        let data = CpuStatusData {
            frames: vec![
                // 20
                CpuFrameStatus {
                    id: -1,
                    last_check: -1,
                    cores_usage: vec![
                        CpuCoreInfo {
                            id: -1,
                            cpu_id: "".to_string(),
                            frame_id: -1,
                            freq: -1,
                            usage: 30,
                        },
                        CpuCoreInfo {
                            id: -1,
                            cpu_id: "".to_string(),
                            frame_id: -1,
                            freq: -1,
                            usage: 20,
                        },
                        CpuCoreInfo {
                            id: -1,
                            cpu_id: "".to_string(),
                            frame_id: -1,
                            freq: -1,
                            usage: 10,
                        },
                    ],
                },
                // 50
                CpuFrameStatus {
                    id: -1,
                    last_check: -1,
                    cores_usage: vec![
                        CpuCoreInfo {
                            id: -1,
                            cpu_id: "".to_string(),
                            frame_id: -1,
                            freq: -1,
                            usage: 40,
                        },
                        CpuCoreInfo {
                            id: -1,
                            cpu_id: "".to_string(),
                            frame_id: -1,
                            freq: -1,
                            usage: 45,
                        },
                        CpuCoreInfo {
                            id: -1,
                            cpu_id: "".to_string(),
                            frame_id: -1,
                            freq: -1,
                            usage: 65,
                        },
                    ],
                },
            ],
        };

        assert_eq!(
            cpu_status_exceeds(
                &MonitorConfig {
                    id: -1,
                    device_id: "".to_string(),
                    updated_at: -1,
                    disk_threshold: 0.0,
                    mem_threshold: 0.0,
                    cpu_threshold: 60.0
                },
                &data
            ),
            false
        );
        assert_eq!(
            cpu_status_exceeds(
                &MonitorConfig {
                    id: -1,
                    device_id: "".to_string(),
                    updated_at: -1,
                    disk_threshold: 0.0,
                    mem_threshold: 0.0,
                    cpu_threshold: 30.0
                },
                &data
            ),
            true
        );
    }

    #[test]
    fn disk_status_exceeds_test() {
        let disk1id = "disk1id";
        let disk2id = "disk2id";

        let disks = vec![
            HardwareDiskInfo {
                id: -1,
                last_check: -1,
                name: "".to_string(),
                fs_type: "".to_string(),
                kind: "".to_string(),
                mount_point: "".to_string(),
                is_removable: true,
                total_space: 280,
                disk_id: disk1id.to_string(),
            },
            HardwareDiskInfo {
                id: -1,
                last_check: -1,
                name: "".to_string(),
                fs_type: "".to_string(),
                kind: "".to_string(),
                mount_point: "".to_string(),
                is_removable: true,
                total_space: 120,
                disk_id: disk2id.to_string(),
            },
        ];

        let data = DiskStatusData {
            frames: vec![
                DiskFrameStatus {
                    id: -1,
                    last_check: -1,
                    disks_usage: vec![
                        SingleDiskInfo {
                            id: -1,
                            frame_id: -1,
                            disk_id: disk1id.to_string(),
                            // usage: 200
                            available: 80,
                        },
                        SingleDiskInfo {
                            id: -1,
                            frame_id: -1,
                            disk_id: disk2id.to_string(),
                            // usage: 10
                            available: 110,
                        },
                    ],
                },
                DiskFrameStatus {
                    id: -1,
                    last_check: -1,
                    disks_usage: vec![
                        SingleDiskInfo {
                            id: -1,
                            frame_id: -1,
                            disk_id: disk1id.to_string(),
                            // usage: 60
                            available: 220,
                        },
                        SingleDiskInfo {
                            id: -1,
                            frame_id: -1,
                            disk_id: disk2id.to_string(),
                            // usage: 90
                            available: 30,
                        },
                    ],
                },
            ],
        };

        // disk1 usage: 200 + 60 / 2 = 260 / 2 = 130 = 46%
        // disk2 usage: 90 + 10 / 2 = 100 / 2 = 50 = 41%

        assert_eq!(
            disk_status_exceeds(
                &MonitorConfig {
                    id: -1,
                    device_id: "".to_string(),
                    updated_at: -1,
                    cpu_threshold: 0.0,
                    mem_threshold: 0.0,
                    disk_threshold: 46.1,
                },
                &data,
                &disks,
            ),
            false
        );
        assert_eq!(
            disk_status_exceeds(
                &MonitorConfig {
                    id: -1,
                    device_id: "".to_string(),
                    updated_at: -1,
                    cpu_threshold: 0.0,
                    mem_threshold: 0.0,
                    disk_threshold: 45.0,
                },
                &data,
                &disks,
            ),
            true
        );
        assert_eq!(
            disk_status_exceeds(
                &MonitorConfig {
                    id: -1,
                    device_id: "".to_string(),
                    updated_at: -1,
                    cpu_threshold: 0.0,
                    mem_threshold: 0.0,
                    disk_threshold: 33.0,
                },
                &data,
                &disks,
            ),
            true
        );
    }

    #[test]
    fn mem_status_exceeds_test() {
        let mem1id = "mem1id";
        let mem2id = "mem2id";

        let mems = vec![
            HardwareMemInfo {
                id: -1,
                last_check: -1,
                total_space: 280,
                mem_id: mem1id.to_string(),
            },
            HardwareMemInfo {
                id: -1,
                last_check: -1,

                total_space: 120,
                mem_id: mem2id.to_string(),
            },
        ];

        let data = MemStatusData {
            frames: vec![
                MemFrameStatus {
                    id: -1,
                    last_check: -1,
                    mems_usage: vec![
                        SingleMemInfo {
                            id: -1,
                            frame_id: -1,
                            mem_id: mem1id.to_string(),
                            // usage: 200
                            available: 80,
                        },
                        SingleMemInfo {
                            id: -1,
                            frame_id: -1,
                            mem_id: mem2id.to_string(),
                            // usage: 10
                            available: 110,
                        },
                    ],
                },
                MemFrameStatus {
                    id: -1,
                    last_check: -1,
                    mems_usage: vec![
                        SingleMemInfo {
                            id: -1,
                            frame_id: -1,
                            mem_id: mem1id.to_string(),
                            // usage: 60
                            available: 220,
                        },
                        SingleMemInfo {
                            id: -1,
                            frame_id: -1,
                            mem_id: mem2id.to_string(),
                            // usage: 90
                            available: 30,
                        },
                    ],
                },
            ],
        };

        // mem1 usage: 200 + 60 / 2 = 260 / 2 = 130 = 46%
        // mem2 usage: 90 + 10 / 2 = 100 / 2 = 50 = 41%

        assert_eq!(
            mem_status_exceeds(
                &MonitorConfig {
                    id: -1,
                    device_id: "".to_string(),
                    updated_at: -1,
                    cpu_threshold: 0.0,
                    disk_threshold: 0.0,
                    mem_threshold: 46.1,
                },
                &data,
                &mems,
            ),
            false
        );
        assert_eq!(
            mem_status_exceeds(
                &MonitorConfig {
                    id: -1,
                    device_id: "".to_string(),
                    updated_at: -1,
                    cpu_threshold: 0.0,
                    disk_threshold: 0.0,
                    mem_threshold: 45.0,
                },
                &data,
                &mems,
            ),
            true
        );
        assert_eq!(
            mem_status_exceeds(
                &MonitorConfig {
                    id: -1,
                    device_id: "".to_string(),
                    updated_at: -1,
                    cpu_threshold: 0.0,
                    disk_threshold: 0.0,
                    mem_threshold: 33.0,
                },
                &data,
                &mems,
            ),
            true
        );
    }
}
