//! `GET /system/info`, `GET /system/smart`, and `GET /summary`.

use axum::{Json, extract::State};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::error::AppResult;
use crate::models::system::{DiskInfo, HardwareInfo, NetworkInterfaceInfo};
use crate::routes::dtos::system::{
    DiskInfoDto, HardwareInfoDto, NetworkInterfaceInfoDto, SmartDeviceDto, SmartResponse,
    SummaryResponse, SystemDescriptionDto, SystemInfoResponse,
};
use crate::routes::extractors::Claims;
use crate::services::system as system_svc;
use crate::state::AppState;
use crate::storage::repositories::{AlertRepository, SmartRepository};

pub async fn get_system_info(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<SystemInfoResponse>> {
    let desc = system_svc::get_description();
    let hardware = state.hardware_info.as_ref();
    let server_name = state.effective_config.read().await.server_name.clone();

    Ok(Json(SystemInfoResponse {
        server_name,
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

/// SMART disk health — latest reading per device, plus whether the
/// collector found a usable `smartctl` at all.
pub async fn get_smart(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<SmartResponse>> {
    let latest = SmartRepository::new(state.db.clone()).read_latest().await?;
    Ok(Json(SmartResponse {
        available: state.smart_available.load(Ordering::Relaxed),
        devices: latest
            .into_iter()
            .map(|l| SmartDeviceDto {
                device: l.row.device,
                model: l.row.model,
                serial: l.row.serial,
                health_passed: l.row.health_passed,
                temperature_c: l.row.temperature_c,
                power_on_hours: l.row.power_on_hours,
                power_cycles: l.row.power_cycles,
                reallocated_sectors: l.row.reallocated_sectors,
                pending_sectors: l.row.pending_sectors,
                uncorrectable_sectors: l.row.uncorrectable_sectors,
                udma_crc_errors: l.row.udma_crc_errors,
                percentage_used: l.row.percentage_used,
                available_spare_percent: l.row.available_spare_percent,
                media_errors: l.row.media_errors,
                timestamp: l.timestamp,
            })
            .collect(),
    }))
}

/// One-call host overview for multi-server clients. Reads the latest
/// collector tick from the in-memory cache (no DB round trip for gauges)
/// plus a single COUNT over `alert_state`.
pub async fn get_summary(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<SummaryResponse>> {
    let desc = system_svc::get_description();
    let server_name = state.effective_config.read().await.server_name.clone();
    let (alerts_pending, alerts_firing) = AlertRepository::new(state.db.clone())
        .count_active_state()
        .await?;

    let stats = state.stats_latest.read().await.clone();
    let (stats_timestamp, cpu_usage_percent, memory_used, memory_total, disk_max) = match &stats {
        Some(s) => {
            // Fullest mount wins the card slot; removable/pseudo mounts are
            // already filtered by the collector.
            let disk_max = s
                .disks
                .iter()
                .filter(|d| d.total_bytes > 0)
                .map(|d| {
                    (
                        d.used_bytes as f64 / d.total_bytes as f64 * 100.0,
                        d.mount_point.clone(),
                    )
                })
                .max_by(|a, b| a.0.total_cmp(&b.0));
            (
                Some(s.cpu.timestamp),
                Some(s.cpu.usage_percent),
                Some(s.memory.used_bytes),
                Some(s.memory.total_bytes),
                disk_max,
            )
        }
        None => (None, None, None, None, None),
    };
    let (disk_max_used_percent, disk_max_mount) = match disk_max {
        Some((pct, mount)) => (Some(pct), Some(mount)),
        None => (None, None),
    };

    Ok(Json(SummaryResponse {
        server_name,
        hostname: desc.hostname,
        os: desc.os,
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: desc.uptime_secs,
        stats_timestamp,
        cpu_usage_percent,
        memory_used_bytes: memory_used,
        memory_total_bytes: memory_total,
        disk_max_used_percent,
        disk_max_mount,
        alerts_pending,
        alerts_firing,
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
