use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct GetDiskStatusRequest {
    pub start_time: i64,
    pub end_time: i64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct SingleDiskInfo {
    pub id: i64,
    pub frame_id: i64,
    // the id of the disk, consists from key info like name, fs, etc.
    pub disk_id: String,
    pub available: i64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct DiskFrameStatus {
    pub id: i64,
    pub last_check: i64,
    // usage for each disk
    pub disks_usage: Vec<SingleDiskInfo>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct DiskStatusData {
    // usage for each frame, the size
    // of the frame is defined in the config
    // where the user picks the frequency
    // of the monitoring
    pub frames: Vec<DiskFrameStatus>,
}
