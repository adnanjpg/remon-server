//! REST DTOs for the time-series history endpoints.
//!
//! Every list endpoint echoes back the `resolution` it actually served so
//! the client can tell whether the server auto-downsampled or honoured an
//! explicit override. `points` are sorted by `timestamp` ascending.

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct MetricsRangeQuery {
    pub start: Option<i64>,
    pub end: Option<i64>,
    /// Optional override; otherwise auto-selected from the span.
    /// One of: "raw", "1m", "5m", "1h".
    pub resolution: Option<String>,
    /// Hard cap on points returned. Defaults to 1000, max 5000.
    pub limit: Option<u32>,
}

// ===== CPU =====

#[derive(Debug, Serialize)]
pub struct CpuPoint {
    pub timestamp: i64,
    pub usage_percent: f64,
    pub load_1m: f64,
    pub load_5m: f64,
    pub load_15m: f64,
    /// Linux-only: hypervisor steal time as a percent of total CPU.
    /// Sustained >2-3% on a VPS is a credible signal of host throttling.
    /// `null` on Windows / macOS.
    pub steal_percent: Option<f64>,
    /// Linux-only: time the CPU was idle waiting for outstanding disk I/O.
    pub iowait_percent: Option<f64>,
    /// Linux-only: time the kernel ran a guest VM. Non-zero only when
    /// this host *is* a hypervisor.
    pub guest_percent: Option<f64>,
    /// Linux-only: kernel-wide context switches per second.
    pub context_switches_per_sec: Option<i64>,
    /// Linux-only: process forks per second (clone() syscall rate).
    pub process_forks_per_sec: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct CpuHistoryResponse {
    pub resolution: String,
    pub points: Vec<CpuPoint>,
}

// ===== CPU cores (raw only) =====

#[derive(Debug, Serialize)]
pub struct CpuCorePoint {
    pub timestamp: i64,
    pub core_index: i64,
    pub usage_percent: f64,
    pub freq_mhz: i64,
}

#[derive(Debug, Serialize)]
pub struct CpuCoresHistoryResponse {
    pub resolution: String, // always "raw"
    pub points: Vec<CpuCorePoint>,
}

// ===== Memory =====

#[derive(Debug, Serialize)]
pub struct MemoryPoint {
    pub timestamp: i64,
    pub used_bytes: i64,
    pub available_bytes: i64,
    pub cached_bytes: i64,
    pub swap_used_bytes: i64,
    /// Linux-only: minor page faults per second (cold-page hits, no I/O).
    pub page_faults_minor_per_sec: Option<i64>,
    /// Linux-only: major page faults per second (required disk I/O — a
    /// sustained non-zero rate means the working set doesn't fit in RAM).
    pub page_faults_major_per_sec: Option<i64>,
    /// Linux-only: pages swapped IN from disk per second.
    /// Sustained > 0 = active thrashing.
    pub swap_in_pages_per_sec: Option<i64>,
    /// Linux-only: pages swapped OUT to disk per second.
    pub swap_out_pages_per_sec: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct MemoryHistoryResponse {
    pub resolution: String,
    pub points: Vec<MemoryPoint>,
}

// ===== Disk =====

#[derive(Debug, Serialize)]
pub struct DiskPoint {
    pub timestamp: i64,
    pub mount_point: String,
    pub used_bytes: i64,
    pub available_bytes: i64,
    pub read_bytes_per_sec: i64,
    pub write_bytes_per_sec: i64,
    /// Linux-only (statvfs): 0..=100. A volume can run out of inodes long
    /// before bytes when there are millions of small files.
    pub inode_used_percent: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct DiskHistoryResponse {
    pub resolution: String,
    pub points: Vec<DiskPoint>,
}

// ===== Network =====

#[derive(Debug, Serialize)]
pub struct NetworkPoint {
    pub timestamp: i64,
    pub interface_name: String,
    pub rx_bytes_per_sec: i64,
    pub tx_bytes_per_sec: i64,
    pub rx_packets_per_sec: i64,
    pub tx_packets_per_sec: i64,
    /// Frames the NIC dropped or rejected on receive / transmit, per second.
    /// Sustained non-zero usually = bad cable, MTU mismatch, driver bug.
    pub errors_in_per_sec: i64,
    pub errors_out_per_sec: i64,
}

#[derive(Debug, Serialize)]
pub struct NetworkHistoryResponse {
    pub resolution: String,
    pub points: Vec<NetworkPoint>,
}

// ===== Pressure (PSI) =====

#[derive(Debug, Serialize)]
pub struct PressurePoint {
    pub timestamp: i64,
    pub some_avg10: f64,
    pub some_avg60: f64,
    pub some_avg300: f64,
    pub full_avg10: f64,
    pub full_avg60: f64,
    pub full_avg300: f64,
}

#[derive(Debug, Serialize)]
pub struct PressureHistoryResponse {
    pub resolution: String,
    /// Echoes the path-param resource: `"cpu" | "memory" | "io"`.
    pub resource: String,
    pub points: Vec<PressurePoint>,
}

// ===== Components (hardware sensors) =====

#[derive(Debug, Serialize)]
pub struct ComponentPoint {
    pub timestamp: i64,
    pub label: String,
    pub temperature_c: Option<f64>,
    pub max_c: Option<f64>,
    pub critical_c: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct ComponentsHistoryResponse {
    pub resolution: String,
    pub points: Vec<ComponentPoint>,
}
