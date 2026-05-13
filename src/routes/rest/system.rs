//! `GET /system/info`.

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
    let desc = system_svc::get_description();
    let hardware = state
        .hardware_info
        .get_or_init(system_svc::init_hardware_info)
        .await;

    Ok(Json(SystemInfoResponse {
        description: SystemDescriptionDto {
            hostname: desc.hostname,
            os: desc.os,
            os_version: desc.os_version,
            kernel: desc.kernel,
            uptime_secs: desc.uptime_secs,
            version: env!("CARGO_PKG_VERSION").to_string(),
            build_mode: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .to_string(),
            built_at: env!("BUILD_TIME").parse().unwrap_or(0),
        },
        hardware: hardware_info_to_dto(hardware),
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
