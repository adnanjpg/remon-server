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
    /// Linux-only: user+nice CPU time as percent of total.
    pub user_percent: Option<f64>,
    /// Linux-only: kernel CPU time (system+irq+softirq) as percent of total.
    pub system_percent: Option<f64>,
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
    pub total_bytes: i64,
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
    pub total_bytes: i64,
    pub used_bytes: i64,
    pub available_bytes: i64,
    pub read_bytes_per_sec: i64,
    pub write_bytes_per_sec: i64,
    /// Linux-only (statvfs): 0..=100. A volume can run out of inodes long
    /// before bytes when there are millions of small files.
    pub inode_used_percent: Option<f64>,
    /// Linux-only (/proc/diskstats): read I/O operations per second.
    pub read_iops: Option<i64>,
    /// Linux-only (/proc/diskstats): write I/O operations per second.
    pub write_iops: Option<i64>,
    /// Linux-only (/proc/diskstats): percent of wall-clock time the device had I/O in flight.
    pub io_util_percent: Option<f64>,
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

#[derive(Debug, Serialize)]
pub struct NetworkUsageInterface {
    pub name: String,
    pub rx_bytes: i64,
    pub tx_bytes: i64,
    /// An encapsulating tunnel: the same payload crosses a physical NIC on its
    /// way out, so counting both reports roughly double the traffic the host
    /// actually moved. Listed, but left out of the totals.
    pub is_tunnel: bool,
}

/// Bytes moved over a window, integrated from the stored rates.
#[derive(Debug, Serialize)]
pub struct NetworkUsageResponse {
    pub start: i64,
    pub end: i64,
    /// Bucket size the sum was taken at; a longer window integrates coarser rows.
    pub resolution: String,
    /// Non-tunnel interfaces only — see `NetworkUsageInterface::is_tunnel`.
    pub total_rx_bytes: i64,
    pub total_tx_bytes: i64,
    /// Share of the window that actually holds samples, 0.0..=1.0. Below 1.0 the
    /// daemon was down or the rows aged out, and the totals are a floor rather
    /// than the whole truth — a distinction anyone reading this against a
    /// bandwidth quota needs to see.
    pub coverage: f64,
    pub interfaces: Vec<NetworkUsageInterface>,
}

// ===== Docker containers =====

#[derive(Debug, Serialize)]
pub struct DockerPoint {
    pub timestamp: i64,
    pub cpu_percent: f64,
    pub memory_used_bytes: i64,
    /// 0 when the container has no memory limit.
    pub memory_limit_bytes: i64,
    /// Byte counters below are cumulative since the container started.
    pub network_rx_bytes: i64,
    pub network_tx_bytes: i64,
    pub block_read_bytes: i64,
    pub block_write_bytes: i64,
    pub pids: i64,
}

#[derive(Debug, Serialize)]
pub struct DockerHistoryResponse {
    pub resolution: String,
    pub points: Vec<DockerPoint>,
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

// ===== Batch =====

/// Query for `GET /metrics/batch`. Collapses N per-resource requests into
/// one — saves cellular radio wake-ups and JWT/envelope overhead.
#[derive(Debug, Deserialize)]
pub struct BatchMetricsQuery {
    /// Comma-separated, e.g. `cpu,memory,disk`. Whitelist enforced.
    pub resources: String,
    /// Relative window: `30m`, `1h`, `24h`, `7d`, or raw seconds.
    /// Mutually exclusive with `start`/`end`.
    pub span: Option<String>,
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub resolution: Option<String>,
    pub limit: Option<u32>,
}

/// Tagged on `resource` so new variants (probe, pressure) stay additive.
#[derive(Debug, Serialize)]
#[serde(tag = "resource", rename_all = "snake_case")]
pub enum BatchSeries {
    Cpu { points: Vec<CpuPoint> },
    CpuCores { points: Vec<CpuCorePoint> },
    Memory { points: Vec<MemoryPoint> },
    Disk { points: Vec<DiskPoint> },
    Network { points: Vec<NetworkPoint> },
    Components { points: Vec<ComponentPoint> },
}

#[derive(Debug, Serialize)]
pub struct BatchMetricsResponse {
    pub start: i64,
    pub end: i64,
    /// Resolution applied to every series — one per request, like the
    /// single-resource endpoints.
    pub resolution: String,
    pub series: Vec<BatchSeries>,
}
