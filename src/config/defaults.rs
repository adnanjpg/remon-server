// Default configuration values

pub const SERVER_NAME: &str = "My Server";

// Collection intervals (milliseconds)
pub const STATS_INTERVAL_MS: u64 = 2000;
pub const PROCESS_INTERVAL_MS: u64 = 5000;
pub const DOCKER_INTERVAL_MS: u64 = 3000;

// Retention policies (days)
pub const METRICS_RETENTION_DAYS: u32 = 7;
pub const LOGS_RETENTION_DAYS: u32 = 30;

// Token expiration (seconds)
pub const ACCESS_TOKEN_EXPIRY_SECS: i64 = 3600;        // 1 hour
pub const REFRESH_TOKEN_EXPIRY_SECS: i64 = 2592000;    // 30 days
pub const PAIRING_CODE_EXPIRY_SECS: i64 = 300;         // 5 minutes
