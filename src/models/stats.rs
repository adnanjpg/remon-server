use serde::{Deserialize, Serialize};

/// CPU statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuStats {
    pub usage_percent: f64,
    pub per_core: Vec<CoreStats>,
    pub load_avg: LoadAverage,
    pub timestamp: i64,
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
}

/// Network statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkStats {
    pub interface: String,
    pub rx_bytes_per_sec: u64,
    pub tx_bytes_per_sec: u64,
    pub rx_packets_per_sec: u64,
    pub tx_packets_per_sec: u64,
    pub timestamp: i64,
}

/// All stats combined
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllStats {
    pub cpu: CpuStats,
    pub memory: MemoryStats,
    pub disks: Vec<DiskStats>,
    pub network: Vec<NetworkStats>,
}

/// Stats event for broadcast channel
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum StatsEvent {
    Cpu(CpuStats),
    Memory(MemoryStats),
    Disk(Vec<DiskStats>),
    Network(Vec<NetworkStats>),
    All(AllStats),
}
