//! System info endpoint.
//!
//! `GET /system/info` returns the host header data the UI needs to draw a
//! "this machine" panel: hostname / OS / kernel / uptime / CPU model / core
//! count / total RAM / disk inventory / NIC inventory.
//!
//! Hardware inventory is read once at boot from `AppState.hardware_info`
//! (an `Arc<HardwareInfo>`) so per-request cost stays in the microsecond
//! range. The description block (`hostname`, `os`, `os_version`, `kernel`,
//! `uptime_secs`) is computed fresh every call — the underlying sysinfo
//! reads are cheap and `uptime_secs` is genuinely volatile.
//!
//! Hot-plug events (USB disk, new NIC) are not reflected until restart.
//! That trade-off is intentional: hardware refresh on every poll would be
//! wasteful, and a dedicated refresh endpoint can be added later without
//! breaking this contract.

use axum::{Json, extract::State};
use std::sync::Arc;

use crate::error::AppResult;
use crate::models::system::{DiskInfo, HardwareInfo, NetworkInterfaceInfo};
use crate::routes::dtos::system::{
    DiskInfoDto, HardwareInfoDto, NetworkInterfaceInfoDto, SystemDescriptionDto, SystemInfoResponse,
};
use crate::routes::extractors::Claims;
use crate::services::system as system_svc;
use crate::state::AppState;

pub async fn get_system_info(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<SystemInfoResponse>> {
    // Description: cheap to recompute every call; uptime_secs is volatile.
    let desc = system_svc::get_description();

    Ok(Json(SystemInfoResponse {
        description: SystemDescriptionDto {
            hostname: desc.hostname,
            os: desc.os,
            os_version: desc.os_version,
            kernel: desc.kernel,
            uptime_secs: desc.uptime_secs,
        },
        hardware: hardware_info_to_dto(&state.hardware_info),
    }))
}

fn hardware_info_to_dto(hw: &HardwareInfo) -> HardwareInfoDto {
    HardwareInfoDto {
        cpu_model: hw.cpu_model.clone(),
        cpu_cores: hw.cpu_cores,
        cpu_threads: hw.cpu_threads,
        total_memory_bytes: hw.total_memory_bytes,
        disks: hw.disks.iter().map(disk_info_to_dto).collect(),
        network_interfaces: hw
            .network_interfaces
            .iter()
            .map(network_interface_info_to_dto)
            .collect(),
    }
}

fn disk_info_to_dto(d: &DiskInfo) -> DiskInfoDto {
    DiskInfoDto {
        device_name: d.device_name.clone(),
        mount_point: d.mount_point.clone(),
        fs_type: d.fs_type.clone(),
        total_bytes: d.total_bytes,
        is_removable: d.is_removable,
    }
}

fn network_interface_info_to_dto(n: &NetworkInterfaceInfo) -> NetworkInterfaceInfoDto {
    NetworkInterfaceInfoDto {
        name: n.name.clone(),
        mac_address: n.mac_address.clone(),
        ip_addresses: n.ip_addresses.clone(),
        is_virtual: n.is_virtual,
    }
}
