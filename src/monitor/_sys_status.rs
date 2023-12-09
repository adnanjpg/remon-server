use crate::monitor::persistence::{fetch_monitor_configs, insert_monitor_status};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use sysinfo::{CpuExt, CpuRefreshKind, DiskExt, RefreshKind, System, SystemExt};
use tokio::time::sleep;

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerDescription {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct MonitorConfig {
    pub device_id: String,
    pub cpu_threshold: f64,
    pub mem_threshold: f64,
    pub storage_threshold: f64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::Type)]
pub struct DiskStatus {
    pub name: String,
    pub total: u64,
    pub available: u64,
}

pub struct CpuStatus {
    pub vendor_id: String,
    pub brand: String,

}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct MonitorStatus {
    pub cpu_usage: f64,
    pub mem_usage: f64,
    pub storage_usage: Vec<DiskStatus>,
    pub last_check: i64,
}

// TODO(isaidsari): make it configurable
const CHECK_INTERVAL: u64 = 30;

pub fn init_sys_status_check() {
    tokio::spawn(async move {

        let mut system = sysinfo::System::new();

        loop {
            let start = std::time::Instant::now();

            // refresh all system info WARN: this takes too much time
            // let mut system = sysinfo::System::new_all();
            // system.refresh_all();
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

            system.cpus().iter().for_each(|cpu| {
                error!("cpu: {:?}", cpu);
            });

            let status = MonitorStatus {
                cpu_usage: system.global_cpu_info().cpu_usage() as f64 / 100.0,
                mem_usage: system.used_memory() as f64 / system.total_memory() as f64,
                storage_usage,
                last_check: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64,
            };

            let end = std::time::Instant::now();

            debug!("time to refresh: {:?}", end - start);

            info!("status: {:?}", status);

            if let Err(e) = insert_monitor_status(&status).await {
                error!("failed to insert monitor status: {}", e);
            }

            check_thresholds(&status).await;

            sleep(Duration::from_secs(CHECK_INTERVAL)).await;
        }
    });
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
    (
        status.cpu_usage > config.cpu_threshold,
        status.mem_usage > config.mem_threshold,
        true,
    )
}