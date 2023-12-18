use crate::monitor::HardwareInfo;

use super::hardware_cpu_info::{fetch_latest_hardware_cpus_info, insert_hardware_cpu_info};
use super::hardware_disk_info::{fetch_latest_hardware_disks_info, insert_hardware_disk_info};
use super::hardware_mem_info::{fetch_latest_hardware_mems_info, insert_hardware_mem_info};

pub async fn insert_hardware_info(status: &HardwareInfo) -> Result<(), sqlx::Error> {
    for cpu_info in status.cpu_info.iter() {
        insert_hardware_cpu_info(cpu_info).await?;
    }

    for disk_info in status.disks_info.iter() {
        insert_hardware_disk_info(disk_info).await?;
    }

    for mem_info in status.mem_info.iter() {
        insert_hardware_mem_info(mem_info).await?;
    }

    Ok(())
}

pub async fn fetch_latest_hardware_info() -> Result<HardwareInfo, sqlx::Error> {
    let cpu_info = fetch_latest_hardware_cpus_info().await?;
    let disks_info = fetch_latest_hardware_disks_info().await?;
    let mem_info = fetch_latest_hardware_mems_info().await?;

    Ok(HardwareInfo {
        cpu_info,
        disks_info,
        mem_info,
    })
}
