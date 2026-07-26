//! `GET /system/info`, `GET /system/smart`, and `GET /summary`.

use axum::{Json, extract::State, http::StatusCode};
use log::error;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::error::AppResult;
use crate::models::system::{DiskInfo, HardwareInfo, NetworkInterfaceInfo};
use crate::routes::dtos::system::LifecycleResponse;
use crate::routes::dtos::system::{
    DiskInfoDto, HardwareInfoDto, NetworkInterfaceInfoDto, SmartDeviceDto, SmartResponse,
    SummaryResponse, SystemDescriptionDto, SystemInfoResponse,
};
use crate::routes::extractors::Claims;
use crate::services::system as system_svc;
use crate::state::AppState;
use crate::state::ExitIntent;
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

// ===== Lifecycle =====
//
// The deliberate counterparts to what `/processes/{pid}` and
// `/services/{name}` now refuse. Both say what they do in their name, both
// land in the ledger as lifecycle events rather than as an action against some
// unrelated unit, and both answer before they act — a handler that ended the
// process inline would never get its response out.

/// `POST /system/restart` — end this process so the supervisor starts a fresh
/// one. The way to apply boot-time configuration (listen port, log format,
/// CORS origins) without shell access to the host.
///
/// Answers 202 and then drains: in-flight requests finish, the SSE streams
/// close, and the clean-shutdown marker is written so the next boot does not
/// report a crash.
pub async fn restart_server(
    claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<(StatusCode, Json<LifecycleResponse>)> {
    crate::services::events::record_operator(
        &state,
        &claims.device_id,
        "server_lifecycle",
        "Server restart requested".to_string(),
        Some("server"),
        None,
        Some(serde_json::json!({ "action": "restart" })),
    );

    request_exit(&state, ExitIntent::Restart);

    Ok((
        StatusCode::ACCEPTED,
        Json(LifecycleResponse {
            message: "Restarting. The supervisor will start a fresh process.".to_string(),
        }),
    ))
}

/// `POST /system/shutdown` — stop monitoring this host and stay stopped.
///
/// Every supervisor is configured to restart the agent however it went down,
/// so exiting is not enough: the supervisor has to be told. That is only
/// possible when the server knows which unit it runs under, which is why this
/// refuses rather than pretending when it does not — a 202 followed by the
/// agent coming back anyway would be worse than an error.
pub async fn shutdown_server(
    claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<(StatusCode, Json<LifecycleResponse>)> {
    let unit = crate::platform::identity::service_name();

    crate::services::events::record_operator(
        &state,
        &claims.device_id,
        "server_lifecycle",
        match unit {
            Some(u) => format!("Server shutdown requested (unit '{u}')"),
            None => "Server shutdown requested".to_string(),
        },
        Some("server"),
        None,
        Some(serde_json::json!({ "action": "shutdown", "unit": unit })),
    );

    let Some(unit) = unit else {
        // Nothing supervising us, so simply ending the process is enough and
        // is the whole of what "shutdown" can mean here.
        request_exit(&state, ExitIntent::Shutdown);
        return Ok((
            StatusCode::ACCEPTED,
            Json(LifecycleResponse {
                message: "Shutting down. Nothing will restart this process.".to_string(),
            }),
        ));
    };

    // Hand the decision to the supervisor: it stops us with SIGTERM, which
    // drains exactly like the signal path, and records the stop as deliberate
    // so it does not undo it. The task issuing the call is killed partway
    // through, which is expected and harmless.
    let manager = Arc::clone(&state.service_manager);
    let state_for_failure = Arc::clone(&state);
    let unit_name = unit.to_string();
    tokio::spawn(async move {
        if let Err(e) = manager.stop(&unit_name).await {
            error!("shutdown requested but stopping unit '{unit_name}' failed: {e}");
            // The caller got a 202 and is entitled to know it did not happen.
            crate::services::events::record(
                &state_for_failure,
                crate::storage::repositories::NewHostEvent {
                    source: "system",
                    kind: "server_lifecycle",
                    severity: "warn",
                    message: format!(
                        "Shutdown was requested but stopping unit '{unit_name}' failed: {e}"
                    ),
                    ref_type: Some("server"),
                    ..Default::default()
                },
            );
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(LifecycleResponse {
            message: format!("Stopping unit '{unit}'. Starting it again needs access to the host."),
        }),
    ))
}

/// Record why we are ending and let the shutdown signal pick it up. Setting it
/// twice is harmless — the first value is the one that took effect.
///
/// `send_replace`, not `send`: the latter refuses when no receiver is
/// listening and leaves the value unchanged, which would silently drop the
/// reason for the exit on any build that has not subscribed yet.
fn request_exit(state: &Arc<AppState>, intent: ExitIntent) {
    state.exit_intent.send_replace(Some(intent));
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
