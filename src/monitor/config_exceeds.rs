use crate::monitor::models::get_cpu_status::CpuFrameStatusTrait;

use crate::monitor::models::get_hardware_info::{HardwareDiskInfo, HardwareMemInfo};
use crate::monitor::models::get_mem_status::MemStatusData;
use crate::monitor::persistence::fetch_monitor_configs;
use crate::notification_service::{self, NotificationMessage};
use log::{error, warn};
use std::collections::HashMap;
use std::vec;

use super::models::get_cpu_status::CpuStatusData;
use super::models::get_disk_status::{DiskStatusData, DiskStatusDataTrait};
use super::models::get_mem_status::MemStatusDataTrait;
use super::models::MonitorConfig;

pub(super) async fn check_thresholds(
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

        let any_exceeds = cpu.is_some() || mem.is_some() || disk.is_some();

        if any_exceeds {
            let mut exceeding_msgs: Vec<String> = vec![];

            match cpu {
                Some(cpu) => {
                    exceeding_msgs.push(format!("cpu with {}%", cpu));
                }
                None => {}
            }

            match mem {
                Some(mem) => {
                    exceeding_msgs.push(format!("mem with {}%", mem));
                }
                None => {}
            }

            match disk {
                Some(disk) => {
                    exceeding_msgs.push(format!("disk with {}%", disk));
                }
                None => {}
            }

            let result = exceeding_msgs.join(", ");

            let warn_msg = format!(
                "the config for device id {} thresholds exceeded for: {}",
                config.device_id, result
            );

            warn!("{}", warn_msg);

            // TODO(adnanjpg): dont spam the user or fcm
            // send_notification_to_exceeding_device(&config, cpu, mem, disk).await;
        }
    }
}

#[allow(dead_code)]
async fn send_notification_to_exceeding_device(
    config: &MonitorConfig,
    cpu: Option<f64>,
    mem: Option<f64>,
    disk: Option<f64>,
) -> bool {
    let title = "IMPORTANT: Your config limits are exceeded";

    let mut exceeding_msgs: Vec<String> = vec![];

    match cpu {
        Some(cpu) => {
            exceeding_msgs.push(format!("cpu with {}%", cpu));
        }
        None => {}
    }

    match mem {
        Some(mem) => {
            exceeding_msgs.push(format!("mem with {}%", mem));
        }
        None => {}
    }

    match disk {
        Some(disk) => {
            exceeding_msgs.push(format!("disk with {}%", disk));
        }
        None => {}
    }

    let result = exceeding_msgs.join(", ");
    // TODO(adnanjpg): include server ip
    let body = format!("the thresholds exceeded for: {}", result);

    let message = NotificationMessage {
        title: title.to_string(),
        body: body.to_string(),
    };
    let fcm_token = config.fcm_token.to_string();
    let not_res = notification_service::send_notification_to_single(&fcm_token, &message).await;

    if let Err(not_res) = not_res {
        error!(
            "Sending exceeding notification resulted with the following error: {}",
            not_res
        );

        return false;
    }

    return true;
}

fn cpu_status_exceeds(config: &MonitorConfig, status: &CpuStatusData) -> StatusExceedsReturn {
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
            return Some(mean);
        }

        continue;
    }

    None
}

fn mem_status_exceeds(
    config: &MonitorConfig,
    status: &MemStatusData,
    mems_info: &Vec<HardwareMemInfo>,
) -> StatusExceedsReturn {
    let mut totals_map = HashMap::new() as super::models::get_mem_status::MemTotalSpaceMap;
    mems_info.iter().for_each(|f| {
        totals_map.insert(f.mem_id.to_string(), f.total_space);
    });

    let means = status.mems_usage_means_percentages(&totals_map);

    // in some cases, more than one mem can exceed the threshold
    // so we want to return the exceed with the biggest value,
    // so we can let the user set their threat level right
    let mut biggest_val: StatusExceedsReturn = None;

    for mean in means {
        let vall = mean.1 as f64;
        if vall >= config.mem_threshold {
            if let Some(biggest_val) = biggest_val {
                if biggest_val > vall {
                    continue;
                }
            }

            biggest_val = Some(vall);
        }

        continue;
    }

    biggest_val
}

fn disk_status_exceeds(
    config: &MonitorConfig,
    status: &DiskStatusData,
    disks_info: &Vec<HardwareDiskInfo>,
) -> StatusExceedsReturn {
    let mut totals_map = HashMap::new() as super::models::get_disk_status::DiskTotalSpaceMap;
    disks_info.iter().for_each(|f| {
        totals_map.insert(f.disk_id.to_string(), f.total_space);
    });

    let means = status.disks_usage_means_percentages(&totals_map);

    // in some cases, more than one disk can exceed the threshold
    // so we want to return the exceed with the biggest value,
    // so we can let the user set their threat level right
    let mut biggest_val: StatusExceedsReturn = None;

    for mean in means {
        let vall = mean.1 as f64;
        if vall >= config.disk_threshold {
            if let Some(biggest_val) = biggest_val {
                if biggest_val > vall {
                    continue;
                }
            }

            biggest_val = Some(vall);
        }

        continue;
    }

    biggest_val
}

type StatusExceedsReturn = Option<
    // the mean usage percentage
    // if it not exceeds, will return None
    f64,
>;

fn statuses_exceeds(
    config: &MonitorConfig,
    cpu_status: &CpuStatusData,
    mem_status: &MemStatusData,
    mems_info: &Vec<HardwareMemInfo>,
    disk_status: &DiskStatusData,
    disks_info: &Vec<HardwareDiskInfo>,
) -> (
    StatusExceedsReturn,
    StatusExceedsReturn,
    StatusExceedsReturn,
) {
    (
        cpu_status_exceeds(config, cpu_status),
        mem_status_exceeds(config, mem_status, mems_info),
        disk_status_exceeds(config, disk_status, disks_info),
    )
}

#[cfg(test)]
mod tests {
    use crate::monitor::models::{
        get_cpu_status::{CpuCoreInfo, CpuFrameStatus},
        get_disk_status::{DiskFrameStatus, SingleDiskInfo},
        get_mem_status::{MemFrameStatus, SingleMemInfo},
    };

    use super::*;

    #[test]
    fn cpu_status_exceeds_test() {
        let data = CpuStatusData {
            frames: vec![
                // usage 20%
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
                // usage 50%
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
                    fcm_token: "".to_string(),
                    updated_at: -1,
                    disk_threshold: 0.0,
                    mem_threshold: 0.0,
                    cpu_threshold: 60.0,
                },
                &data
            ),
            None,
        );
        assert_eq!(
            cpu_status_exceeds(
                &MonitorConfig {
                    id: -1,
                    device_id: "".to_string(),
                    fcm_token: "".to_string(),
                    updated_at: -1,
                    disk_threshold: 0.0,
                    mem_threshold: 0.0,
                    cpu_threshold: 30.0
                },
                &data
            ),
            Some(50.0),
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
                    fcm_token: "".to_string(),
                    updated_at: -1,
                    cpu_threshold: 0.0,
                    mem_threshold: 0.0,
                    disk_threshold: 46.1,
                },
                &data,
                &disks,
            ),
            None,
        );
        assert_eq!(
            disk_status_exceeds(
                &MonitorConfig {
                    id: -1,
                    device_id: "".to_string(),
                    fcm_token: "".to_string(),
                    updated_at: -1,
                    cpu_threshold: 0.0,
                    mem_threshold: 0.0,
                    disk_threshold: 45.0,
                },
                &data,
                &disks,
            ),
            Some(46.0),
        );
        assert_eq!(
            disk_status_exceeds(
                &MonitorConfig {
                    id: -1,
                    device_id: "".to_string(),
                    fcm_token: "".to_string(),
                    updated_at: -1,
                    cpu_threshold: 0.0,
                    mem_threshold: 0.0,
                    disk_threshold: 33.0,
                },
                &data,
                &disks,
            ),
            Some(46.0),
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
                    fcm_token: "".to_string(),
                    updated_at: -1,
                    cpu_threshold: 0.0,
                    disk_threshold: 0.0,
                    mem_threshold: 46.1,
                },
                &data,
                &mems,
            ),
            None,
        );
        assert_eq!(
            mem_status_exceeds(
                &MonitorConfig {
                    id: -1,
                    device_id: "".to_string(),
                    fcm_token: "".to_string(),
                    updated_at: -1,
                    cpu_threshold: 0.0,
                    disk_threshold: 0.0,
                    mem_threshold: 45.0,
                },
                &data,
                &mems,
            ),
            Some(46.0),
        );
        assert_eq!(
            mem_status_exceeds(
                &MonitorConfig {
                    id: -1,
                    device_id: "".to_string(),
                    fcm_token: "".to_string(),
                    updated_at: -1,
                    cpu_threshold: 0.0,
                    disk_threshold: 0.0,
                    mem_threshold: 33.0,
                },
                &data,
                &mems,
            ),
            Some(46.0),
        );
    }
}
