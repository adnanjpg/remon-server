use sqlx::SqlitePool;
use tokio::sync::{RwLock, broadcast};

use crate::config::AuthConfig;

// Modern broadcast channels
use crate::models::stats::StatsEvent;
use crate::models::process::ProcessList;

/// Active pairing code state
#[derive(Debug, Clone)]
pub struct PairingState {
    pub code: String,
    pub expires_at: i64,
}

/// Shared application state
/// Contains database connection and broadcast channels for real-time data distribution
pub struct AppState {
    /// Database connection pool
    pub db: SqlitePool,

    /// Auth configuration
    pub auth_config: AuthConfig,

    /// Active pairing state (if any)
    pub pairing_state: RwLock<Option<PairingState>>,

    /// Modern unified broadcast channels
    pub stats_tx: broadcast::Sender<StatsEvent>,
    pub processes_tx: broadcast::Sender<ProcessList>,
}

impl AppState {
    /// Create new AppState with database pool
    ///
    /// Channel buffer sizes:
    /// - Stats: 64 (updates ~2/sec)
    /// - Processes: 16 (updates ~5/sec)
    pub fn new(db: SqlitePool, auth_config: AuthConfig) -> Self {
        let (stats_tx, _) = broadcast::channel(64);
        let (processes_tx, _) = broadcast::channel(16);

        Self {
            db,
            auth_config,
            pairing_state: RwLock::new(None),
            stats_tx,
            processes_tx,
        }
    }
}
