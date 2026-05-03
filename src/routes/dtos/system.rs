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
    pub description: SystemDescriptionDto,
    pub hardware: HardwareInfoDto,
}
