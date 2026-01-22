use crate::monitor::models::{
    get_cpu_status::CpuFrameStatus,
    get_disk_status::DiskFrameStatus,
    get_mem_status::MemFrameStatus,
    get_network_status::NetworkFrameStatus,
};
use serde::Serialize;

#[derive(Serialize)]
pub struct GetCpuStatusResponse {
    pub frames: Vec<CpuFrameStatus>,
}

#[derive(Serialize)]
pub struct GetMemStatusResponse {
    pub frames: Vec<MemFrameStatus>,
}

#[derive(Serialize)]
pub struct GetDiskStatusResponse {
    pub frames: Vec<DiskFrameStatus>,
}

#[derive(Serialize)]
pub struct GetNetworkStatusResponse {
    pub frames: Vec<NetworkFrameStatus>,
}