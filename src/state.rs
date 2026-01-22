use sqlx::SqlitePool;
use tokio::sync::broadcast;

// Re-export monitor types for broadcast
pub use crate::monitor::models::get_cpu_status::CpuStatusData;
pub use crate::monitor::models::get_disk_status::DiskStatusData;
pub use crate::monitor::models::get_mem_status::MemStatusData;
pub use crate::monitor::models::get_network_status::NetworkStatusData;

/// Shared application state
/// Contains database connection and broadcast channels for real-time data distribution

pub struct AppState {
    /// Database connection pool
    pub db: SqlitePool,

    /// Broadcast channels for real-time system metrics
    /// Collectors publish to these, SSE endpoints subscribe
    pub cpu_stats_tx: broadcast::Sender<CpuStatusData>,
    pub mem_stats_tx: broadcast::Sender<MemStatusData>,
    pub disk_stats_tx: broadcast::Sender<DiskStatusData>,
    pub network_stats_tx: broadcast::Sender<NetworkStatusData>,
}

impl AppState {
    /// Create new AppState with database pool
    ///
    /// Channel buffer sizes:
    /// - CPU/Memory/Disk: 64 (system metrics update ~1/sec)
    /// - Network: 64 (network stats update ~1/sec)
    pub fn new(db: SqlitePool) -> Self {
        let (cpu_stats_tx, _) = broadcast::channel(64);
        let (mem_stats_tx, _) = broadcast::channel(64);
        let (disk_stats_tx, _) = broadcast::channel(64);
        let (network_stats_tx, _) = broadcast::channel(64);

        Self {
            db,
            cpu_stats_tx,
            mem_stats_tx,
            disk_stats_tx,
            network_stats_tx,
        }
    }
}
