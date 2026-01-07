use serde::{Deserialize, Serialize};

/// Complete system information snapshot (htop-like)
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SystemInfo {
    /// System boot time as UNIX timestamp (seconds)
    pub boot_time: i64,
    /// System uptime in seconds
    pub uptime_seconds: i64,
    /// Load average (1, 5, 15 minutes)
    pub load_average: LoadAverage,
    /// Detailed memory information
    pub memory: MemoryInfo,
    /// Swap usage information
    pub swap: SwapInfo,
    /// Network interfaces with I/O stats
    pub networks: Vec<NetworkInfo>,
    /// Process statistics by state
    pub process_stats: ProcessStats,
}

/// System load average
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LoadAverage {
    /// 1 minute load average
    pub one: f64,
    /// 5 minute load average
    pub five: f64,
    /// 15 minute load average
    pub fifteen: f64,
}

/// Detailed memory information
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MemoryInfo {
    /// Total physical memory in bytes
    pub total: i64,
    /// Used memory in bytes
    pub used: i64,
    /// Free memory in bytes
    pub free: i64,
    /// Available memory in bytes (can be more than free due to caching)
    pub available: i64,
}

/// Swap usage information
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SwapInfo {
    /// Total swap space in bytes
    pub total: i64,
    /// Used swap space in bytes
    pub used: i64,
    /// Free swap space in bytes
    pub free: i64,
}

/// Network interface information
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NetworkInfo {
    /// Interface name (e.g., "eth0", "wlan0")
    pub name: String,
    /// Total bytes received (cumulative)
    pub rx_bytes: i64,
    /// Total bytes transmitted (cumulative)
    pub tx_bytes: i64,
    /// Total packets received (cumulative)
    pub rx_packets: i64,
    /// Total packets transmitted (cumulative)
    pub tx_packets: i64,
}

/// Process statistics by state
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProcessStats {
    /// Total number of processes
    pub total: u32,
    /// Running processes
    pub running: u32,
    /// Sleeping processes
    pub sleeping: u32,
    /// Stopped processes
    pub stopped: u32,
    /// Zombie processes
    pub zombie: u32,
}
