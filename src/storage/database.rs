use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};
use log::info;

use crate::storage::repositories::*;

/// Database wrapper with repository access
#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Connect to SQLite database
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await?;

        info!("Connected to database");

        Ok(Self { pool })
    }

    /// Run migrations
    pub async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::query(MIGRATIONS)
            .execute(&self.pool)
            .await?;

        info!("Database migrations applied");
        Ok(())
    }

    // Repository accessors
    pub fn config(&self) -> ConfigRepository {
        ConfigRepository::new(self.pool.clone())
    }

    pub fn devices(&self) -> DeviceRepository {
        DeviceRepository::new(self.pool.clone())
    }

    pub fn metrics(&self) -> MetricsRepository {
        MetricsRepository::new(self.pool.clone())
    }

    pub fn alerts(&self) -> AlertRepository {
        AlertRepository::new(self.pool.clone())
    }

    pub fn notifiers(&self) -> NotifierRepository {
        NotifierRepository::new(self.pool.clone())
    }

    pub fn logs(&self) -> LogRepository {
        LogRepository::new(self.pool.clone())
    }

    pub fn docker(&self) -> DockerRepository {
        DockerRepository::new(self.pool.clone())
    }

    /// Get raw pool for health checks or legacy code
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

const MIGRATIONS: &str = r#"
-- ============================================
-- CONFIGURATION
-- ============================================

CREATE TABLE IF NOT EXISTS server_config (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    server_name TEXT NOT NULL DEFAULT 'My Server',
    stats_interval_ms INTEGER NOT NULL DEFAULT 2000,
    process_interval_ms INTEGER NOT NULL DEFAULT 5000,
    docker_interval_ms INTEGER NOT NULL DEFAULT 3000,
    metrics_retention_days INTEGER NOT NULL DEFAULT 7,
    logs_retention_days INTEGER NOT NULL DEFAULT 30,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Insert default config if not exists
INSERT OR IGNORE INTO server_config (id) VALUES (1);

-- ============================================
-- DEVICES & AUTH
-- ============================================

CREATE TABLE IF NOT EXISTS devices (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL,
    totp_secret TEXT,
    last_ip TEXT,
    last_seen INTEGER NOT NULL DEFAULT (unixepoch()),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    is_active INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    device_id TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    FOREIGN KEY (device_id) REFERENCES devices(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_sessions_device ON sessions(device_id);
CREATE INDEX IF NOT EXISTS idx_sessions_expires ON sessions(expires_at);

-- ============================================
-- ALERT RULES & NOTIFIERS
-- ============================================

CREATE TABLE IF NOT EXISTS alert_rules (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    metric_type TEXT NOT NULL,
    condition TEXT NOT NULL,
    threshold REAL NOT NULL,
    duration_secs INTEGER NOT NULL DEFAULT 0,
    target_id TEXT,
    cooldown_mins INTEGER NOT NULL DEFAULT 15,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS notifiers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    notifier_type TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    config_json TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS alert_notifiers (
    alert_rule_id INTEGER NOT NULL,
    notifier_id INTEGER NOT NULL,
    PRIMARY KEY (alert_rule_id, notifier_id),
    FOREIGN KEY (alert_rule_id) REFERENCES alert_rules(id) ON DELETE CASCADE,
    FOREIGN KEY (notifier_id) REFERENCES notifiers(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS alert_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_id INTEGER NOT NULL,
    triggered_at INTEGER NOT NULL,
    resolved_at INTEGER,
    metric_value REAL NOT NULL,
    notified INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (rule_id) REFERENCES alert_rules(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_alert_history_ts ON alert_history(triggered_at DESC);
CREATE INDEX IF NOT EXISTS idx_alert_history_rule ON alert_history(rule_id, triggered_at DESC);

-- ============================================
-- HARDWARE INFO
-- ============================================

CREATE TABLE IF NOT EXISTS hardware_info (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    hostname TEXT NOT NULL,
    os TEXT NOT NULL,
    os_version TEXT NOT NULL,
    kernel TEXT NOT NULL,
    cpu_model TEXT NOT NULL,
    cpu_cores INTEGER NOT NULL,
    cpu_threads INTEGER NOT NULL,
    total_memory_bytes INTEGER NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS hardware_disks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    device_name TEXT NOT NULL UNIQUE,
    mount_point TEXT NOT NULL,
    fs_type TEXT NOT NULL,
    total_bytes INTEGER NOT NULL,
    is_removable INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS hardware_network_interfaces (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    mac_address TEXT,
    is_virtual INTEGER NOT NULL DEFAULT 0
);

-- ============================================
-- TIME-SERIES METRICS
-- ============================================

CREATE TABLE IF NOT EXISTS metrics_cpu (
    timestamp INTEGER NOT NULL,
    usage_percent REAL NOT NULL,
    load_1m REAL NOT NULL,
    load_5m REAL NOT NULL,
    load_15m REAL NOT NULL,
    PRIMARY KEY (timestamp)
);

CREATE TABLE IF NOT EXISTS metrics_cpu_cores (
    timestamp INTEGER NOT NULL,
    core_index INTEGER NOT NULL,
    usage_percent REAL NOT NULL,
    freq_mhz INTEGER NOT NULL,
    PRIMARY KEY (timestamp, core_index)
);

CREATE TABLE IF NOT EXISTS metrics_memory (
    timestamp INTEGER NOT NULL,
    used_bytes INTEGER NOT NULL,
    available_bytes INTEGER NOT NULL,
    cached_bytes INTEGER NOT NULL,
    swap_used_bytes INTEGER NOT NULL,
    PRIMARY KEY (timestamp)
);

CREATE TABLE IF NOT EXISTS metrics_disk (
    timestamp INTEGER NOT NULL,
    mount_point TEXT NOT NULL,
    used_bytes INTEGER NOT NULL,
    available_bytes INTEGER NOT NULL,
    read_bytes_per_sec INTEGER NOT NULL DEFAULT 0,
    write_bytes_per_sec INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (timestamp, mount_point)
);

CREATE TABLE IF NOT EXISTS metrics_network (
    timestamp INTEGER NOT NULL,
    interface_name TEXT NOT NULL,
    rx_bytes_per_sec INTEGER NOT NULL,
    tx_bytes_per_sec INTEGER NOT NULL,
    rx_packets_per_sec INTEGER NOT NULL,
    tx_packets_per_sec INTEGER NOT NULL,
    PRIMARY KEY (timestamp, interface_name)
);

CREATE TABLE IF NOT EXISTS metrics_docker (
    timestamp INTEGER NOT NULL,
    container_id TEXT NOT NULL,
    cpu_percent REAL NOT NULL,
    memory_used_bytes INTEGER NOT NULL,
    memory_limit_bytes INTEGER NOT NULL,
    network_rx_bytes INTEGER NOT NULL,
    network_tx_bytes INTEGER NOT NULL,
    block_read_bytes INTEGER NOT NULL DEFAULT 0,
    block_write_bytes INTEGER NOT NULL DEFAULT 0,
    pids INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (timestamp, container_id)
);

-- Indexes for time-based queries
CREATE INDEX IF NOT EXISTS idx_metrics_cpu_ts ON metrics_cpu(timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_metrics_memory_ts ON metrics_memory(timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_metrics_disk_ts ON metrics_disk(timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_metrics_network_ts ON metrics_network(timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_metrics_docker_ts ON metrics_docker(timestamp DESC);

-- ============================================
-- DOCKER INVENTORY
-- ============================================

CREATE TABLE IF NOT EXISTS docker_containers (
    container_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    image TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    first_seen INTEGER NOT NULL DEFAULT (unixepoch()),
    last_seen INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS docker_images (
    image_id TEXT PRIMARY KEY,
    tags TEXT,
    size_bytes INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    first_seen INTEGER NOT NULL DEFAULT (unixepoch())
);

-- ============================================
-- LOGS
-- ============================================

CREATE TABLE IF NOT EXISTS logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,
    level INTEGER NOT NULL,
    source TEXT NOT NULL,
    target TEXT NOT NULL,
    message TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_logs_ts ON logs(timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_logs_source ON logs(source, timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_logs_level ON logs(level, timestamp DESC);
"#;
