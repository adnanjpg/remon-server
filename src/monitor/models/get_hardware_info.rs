use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct HardwareCpuInfo {
    pub id: i64,
    // the id of the cpu chip, consists from key info like vendor_id, brand, etc.
    pub cpu_id: String,
    pub core_count: i32,
    pub vendor_id: String,
    pub brand: String,
    pub last_check: i64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct HardwareDiskInfo {
    pub id: i64,
    // the id of the disk, consists from key info like name, fs, etc.
    pub disk_id: String,
    pub name: String,
    pub fs_type: String,
    pub kind: String,
    pub is_removable: bool,
    pub mount_point: String,
    pub total_space: i64,
    pub last_check: i64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct HardwareMemInfo {
    pub id: i64,
    pub mem_id: String,
    pub total_space: i64,
    pub last_check: i64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct HardwareInfo {
    pub cpu_info: Vec<HardwareCpuInfo>,
    pub disks_info: Vec<HardwareDiskInfo>,
    pub mem_info: Vec<HardwareMemInfo>,
}
