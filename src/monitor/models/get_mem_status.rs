use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct GetCpuStatusRequest {
    pub start_time: i64,
    pub end_time: i64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GetMemStatusRequest {
    pub start_time: i64,
    pub end_time: i64,
}

// we actually only fetch a single mem data, but we're cloning this into 2 structs for convenience, so it would be read the same as the cpu and disk
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct SingleMemInfo {
    pub id: i64,
    pub frame_id: i64,
    // the id of the mem, currently we only have a single mem so this is going to be a constant
    pub mem_id: String,
    pub available: i64,
}
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct MemFrameStatus {
    pub id: i64,
    pub last_check: i64,
    // usage for each mem
    pub mems_usage: Vec<SingleMemInfo>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct MemStatusData {
    pub frames: Vec<MemFrameStatus>,
}
