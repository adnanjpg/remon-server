use serde::{Deserialize, Serialize};

use super::defaults;

/// Runtime configuration stored in database
/// Can be modified via API without restart
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    pub server_name: String,

    // Collection intervals (milliseconds)
    pub stats_interval_ms: u64,
    pub process_interval_ms: u64,
    pub docker_interval_ms: u64,

    // Data retention (days)
    pub metrics_retention_days: u32,
    pub logs_retention_days: u32,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            server_name: defaults::SERVER_NAME.into(),
            stats_interval_ms: defaults::STATS_INTERVAL_MS,
            process_interval_ms: defaults::PROCESS_INTERVAL_MS,
            docker_interval_ms: defaults::DOCKER_INTERVAL_MS,
            metrics_retention_days: defaults::METRICS_RETENTION_DAYS,
            logs_retention_days: defaults::LOGS_RETENTION_DAYS,
        }
    }
}
