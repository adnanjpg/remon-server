//! REST DTOs for the system info endpoint.
//!
//! `SystemInfoResponse` aggregates the rarely-changing hardware inventory
//! (cached at boot) with the small-but-volatile description block (rebuilt
//! per request — `uptime_secs` is the only dynamic field there). The split
//! lets the UI render a host header panel from one call without paying a
//! sysinfo refresh every time it polls for uptime.

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct SystemDescriptionDto {
    pub hostname: String,
    pub os: String,
    pub os_version: String,
    pub kernel: String,
    pub uptime_secs: u64,
    /// remon-server build version (`CARGO_PKG_VERSION`). Lets clients
    /// surface server/web compatibility at a glance.
    pub version: String,
    /// Build mode — `"release"` or `"debug"`. Computed from
    /// `cfg!(debug_assertions)` at compile time.
    pub build_mode: String,
    /// Unix seconds when this binary was built (from `build.rs`).
    pub built_at: i64,
}

#[derive(Debug, Serialize)]
pub struct DiskInfoDto {
    pub device_name: String,
    pub mount_point: String,
    pub fs_type: String,
    pub total_bytes: u64,
    pub is_removable: bool,
}

#[derive(Debug, Serialize)]
pub struct NetworkInterfaceInfoDto {
    pub name: String,
    pub mac_address: Option<String>,
    /// Always empty today — sysinfo doesn't expose interface IPs. Kept in
    /// the shape so a later enrichment (e.g. via `local-ip-address` per
    /// interface) can land without breaking clients.
    pub ip_addresses: Vec<String>,
    /// Heuristic: name starts with `veth`, `docker`, or `br-`.
    pub is_virtual: bool,
}

#[derive(Debug, Serialize)]
pub struct HardwareInfoDto {
    pub cpu_model: String,
    pub cpu_cores: u32,
    pub cpu_threads: u32,
    pub total_memory_bytes: u64,
    pub disks: Vec<DiskInfoDto>,
    pub network_interfaces: Vec<NetworkInterfaceInfoDto>,
}

#[derive(Debug, Serialize)]
pub struct SystemInfoResponse {
    /// Canonical name from runtime config (`server_config.server_name`) —
    /// the label clients should prefer over any locally-stored alias.
    pub server_name: String,
    pub description: SystemDescriptionDto,
    pub hardware: HardwareInfoDto,
}

/// One disk in the `GET /system/smart` response. ATA-only fields are
/// `null` on NVMe devices and vice versa; every field except `device`
/// is best-effort (vendor JSON varies).
#[derive(Debug, Serialize)]
pub struct SmartDeviceDto {
    pub device: String,
    pub model: Option<String>,
    pub serial: Option<String>,
    /// smartctl's overall verdict. `false` is the headline alarm.
    pub health_passed: Option<bool>,
    pub temperature_c: Option<f64>,
    pub power_on_hours: Option<i64>,
    pub power_cycles: Option<i64>,
    /// ATA attribute 5 — sectors remapped to spares. Non-zero and
    /// growing is the classic pre-failure signal.
    pub reallocated_sectors: Option<i64>,
    /// ATA attribute 197 — sectors awaiting remap.
    pub pending_sectors: Option<i64>,
    /// ATA attribute 198 — sectors unreadable even offline.
    pub uncorrectable_sectors: Option<i64>,
    /// ATA attribute 199 — interface CRC errors; usually cabling.
    pub udma_crc_errors: Option<i64>,
    /// NVMe wear indicator, 0-100+ (can exceed 100).
    pub percentage_used: Option<i64>,
    pub available_spare_percent: Option<i64>,
    pub media_errors: Option<i64>,
    /// Unix seconds of the reading. Stale relative to the poll interval
    /// means the device stopped responding (or was unplugged).
    pub timestamp: i64,
}

#[derive(Debug, Serialize)]
pub struct SmartResponse {
    /// False until the collector has confirmed a working `smartctl`.
    /// Lets clients render "install smartmontools" instead of an empty
    /// disk list.
    pub available: bool,
    pub devices: Vec<SmartDeviceDto>,
}

/// `GET /summary` — one-call host overview for multi-server clients.
///
/// Deliberately flat and small: a fleet view polls this once per daemon to
/// render a server card (name, health, headline gauges, alert badge)
/// without fanning out to `/system/info` + `/metrics/*` + `/alerts/state`.
/// Live-stat fields are `None` until the first collector tick lands.
#[derive(Debug, Serialize)]
pub struct SummaryResponse {
    /// Operator-set display name (`server_name` from runtime config).
    pub server_name: String,
    pub hostname: String,
    pub os: String,
    /// remon-server build version, for client compatibility checks.
    pub version: String,
    pub uptime_secs: u64,
    /// Timestamp of the stats tick the gauge fields below were read from.
    pub stats_timestamp: Option<i64>,
    pub cpu_usage_percent: Option<f64>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    /// Fullest mount's used percentage — the single most useful disk
    /// number for an at-a-glance card. Mount identified by `disk_max_mount`.
    pub disk_max_used_percent: Option<f64>,
    pub disk_max_mount: Option<String>,
    pub alerts_pending: u32,
    pub alerts_firing: u32,
}
