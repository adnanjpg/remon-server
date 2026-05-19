use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// CPU statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuStats {
    pub usage_percent: f64,
    pub per_core: Vec<CoreStats>,
    pub load_avg: LoadAverage,
    pub timestamp: i64,
    /// `/proc/stat` 8th column (steal): time the hypervisor stole from this
    /// VM for other guests. >2-3% sustained on a VPS is a credible signal
    /// of host oversubscription / throttling. Linux only; `None` elsewhere.
    pub steal_percent: Option<f64>,
    /// `/proc/stat` 5th column (iowait): CPU time spent idle while waiting
    /// for outstanding disk I/O. Linux only.
    pub iowait_percent: Option<f64>,
    /// `/proc/stat` 9th column (guest): CPU time this kernel spent running
    /// a guest VM. Non-zero only when this host *is* a hypervisor. Linux only.
    pub guest_percent: Option<f64>,
    /// User-space CPU time (user + nice) as percent of total. Linux only.
    pub user_percent: Option<f64>,
    /// Kernel-space CPU time (system + irq + softirq) as percent of total. Linux only.
    pub system_percent: Option<f64>,
    /// Kernel-wide context switches per second (`/proc/stat ctxt` delta).
    /// Tens of thousands is normal; a sudden spike with no workload change
    /// is the classic signature of thread thrash. Linux only.
    pub context_switches_per_sec: Option<u64>,
    /// Process forks per second (`/proc/stat processes` delta — counts
    /// `clone()` syscall invocations despite the line name). Useful for
    /// catching restart loops / fork-bombs. Linux only.
    pub process_forks_per_sec: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreStats {
    pub core_index: u32,
    pub usage_percent: f64,
    pub freq_mhz: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadAverage {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

/// Memory statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStats {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub cached_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub timestamp: i64,
    /// Minor page faults per second (`pgfault - pgmajfault`). High rate is
    /// usually fine — programs hitting cold pages of their own working set.
    /// Linux only (/proc/vmstat).
    pub page_faults_minor_per_sec: Option<u64>,
    /// Major page faults per second — those that required disk I/O. A
    /// sustained non-zero rate means the working set doesn't fit in RAM.
    pub page_faults_major_per_sec: Option<u64>,
    /// Pages swapped IN from disk to RAM per second (`pswpin` delta).
    /// Sustained > 0 means active thrashing.
    pub swap_in_pages_per_sec: Option<u64>,
    /// Pages swapped OUT from RAM to disk per second (`pswpout` delta).
    pub swap_out_pages_per_sec: Option<u64>,
}

/// Disk statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskStats {
    pub mount_point: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub read_bytes_per_sec: u64,
    pub write_bytes_per_sec: u64,
    pub timestamp: i64,
    /// Filesystem inode utilization (0.0..=100.0). Linux only (statvfs).
    pub inode_used_percent: Option<f64>,
    /// Read I/O operations per second. Linux only (/proc/diskstats).
    pub read_iops: Option<u64>,
    /// Write I/O operations per second. Linux only (/proc/diskstats).
    pub write_iops: Option<u64>,
    /// Percentage of time the device had at least one I/O in flight (0..=100).
    /// Analogous to `%util` in iostat. Linux only (/proc/diskstats).
    pub io_util_percent: Option<f64>,
}

/// Network statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkStats {
    pub interface: String,
    pub rx_bytes_per_sec: u64,
    pub tx_bytes_per_sec: u64,
    pub rx_packets_per_sec: u64,
    pub tx_packets_per_sec: u64,
    /// Error counters — frames the NIC dropped or rejected on receive /
    /// transmit since the last refresh, divided by the elapsed interval.
    /// A non-zero rate that doesn't go away usually means a bad cable,
    /// MTU mismatch, or driver problem.
    pub errors_in_per_sec: u64,
    pub errors_out_per_sec: u64,
    /// Cumulative bytes received/transmitted since boot (from sysinfo
    /// `total_received` / `total_transmitted`). Live-only — not stored in
    /// the metrics DB; useful for the live overview panel.
    pub rx_bytes_total: u64,
    pub tx_bytes_total: u64,
    pub timestamp: i64,
}

/// PSI (Pressure Stall Information) snapshot for a single resource.
///
/// Linux 4.20+ exposes `/proc/pressure/{cpu,memory,io}`, each containing
/// "fraction of time at least one task was stalled waiting for the
/// resource" averages over 10s / 60s / 300s windows. `some` covers any
/// task being stalled; `full` (memory/io only — the kernel does not
/// emit `full` on `cpu`) requires every runnable task to be stalled.
///
/// PSI is a more accurate saturation indicator than `load_avg` for modern
/// container workloads — it does not double-count work-conserving I/O.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PressureStats {
    pub some_avg10: f64,
    pub some_avg60: f64,
    pub some_avg300: f64,
    pub full_avg10: f64,
    pub full_avg60: f64,
    pub full_avg300: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PressureSnapshot {
    pub cpu: Option<PressureStats>,
    pub memory: Option<PressureStats>,
    pub io: Option<PressureStats>,
    pub timestamp: i64,
}

/// Hardware sensor reading. `label` is whatever the OS gave us — it's
/// human-presentation, not a stable key. Cross-platform via sysinfo's
/// `Components` API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentInfo {
    pub label: String,
    /// Current temperature in °C. `None` when the OS doesn't expose a
    /// reading for this component (it can show up in the list anyway).
    pub temperature_c: Option<f64>,
    /// Highest reading sysinfo has seen since startup. Useful for headroom
    /// charts; not all platforms supply it.
    pub max_c: Option<f64>,
    /// Vendor-declared critical threshold above which thermal throttling
    /// or shutdown kicks in. Where reported, alerts should fire well
    /// below this.
    pub critical_c: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentsSnapshot {
    pub components: Vec<ComponentInfo>,
    pub timestamp: i64,
}

/// All stats combined.
///
/// Fields are wrapped in `Arc` so the primer-cache snapshot and the
/// broadcast events share the same heap-allocated payloads — a new
/// subscriber-tick fans out as N refcount bumps instead of N deep clones
/// of every Vec/String inside.
///
/// Not `Deserialize`: `Arc<T>: Deserialize` requires serde's `rc`
/// feature and no caller of this struct decodes it from JSON anyway.
#[derive(Debug, Clone, Serialize)]
pub struct AllStats {
    pub cpu: Arc<CpuStats>,
    pub memory: Arc<MemoryStats>,
    pub disks: Arc<Vec<DiskStats>>,
    pub network: Arc<Vec<NetworkStats>>,
    pub pressure: Option<Arc<PressureSnapshot>>,
    pub components: Option<Arc<ComponentsSnapshot>>,
}

/// Stats event for broadcast channel.
///
/// Variants hold `Arc<T>` so `tokio::sync::broadcast`'s per-receiver clone
/// is just a refcount bump — receivers that filter out the event (e.g.
/// `/sse/stats/cpu` discarding `Memory`) pay nothing for the payload.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum StatsEvent {
    Cpu(Arc<CpuStats>),
    Memory(Arc<MemoryStats>),
    Disk(Arc<Vec<DiskStats>>),
    Network(Arc<Vec<NetworkStats>>),
    All(Arc<AllStats>),
    Pressure(Arc<PressureSnapshot>),
    Components(Arc<ComponentsSnapshot>),
}
