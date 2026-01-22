use config::{Config as ConfigBuilder, ConfigError, Environment, File};
use serde::Deserialize;
use std::env;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub auth: AuthConfig,
    pub monitoring: MonitoringConfig,
    pub grpc: GrpcConfig,
    pub logging: LoggingConfig,
    pub fcm: FcmConfig,
    pub docker: DockerConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub port: u16,
    pub host: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DatabaseConfig {
    pub path: String,
    pub folder_path: String,
    pub max_connections: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuthConfig {
    pub jwt_secret: String,
    /// Access token TTL in seconds (default: 3600 = 1 hour)
    #[serde(default = "default_access_token_ttl")]
    pub access_token_ttl_secs: u64,
    /// Refresh token TTL in seconds (default: 2592000 = 30 days)
    #[serde(default = "default_refresh_token_ttl")]
    pub refresh_token_ttl_secs: u64,
    /// Pairing code TTL in seconds (default: 300 = 5 minutes)
    #[serde(default = "default_pairing_code_ttl")]
    pub pairing_code_ttl_secs: u64,
}

fn default_access_token_ttl() -> u64 {
    3600 // 1 hour
}

fn default_refresh_token_ttl() -> u64 {
    2592000 // 30 days
}

fn default_pairing_code_ttl() -> u64 {
    300 // 5 minutes
}

#[derive(Debug, Deserialize, Clone)]
pub struct MonitoringConfig {
    pub update_interval_ms: u64,
    pub enable_notifications: bool,
    /// Minimum interval between sending threshold exceeded notifications (in seconds)
    pub notification_interval_seconds: u64,
    /// Minimum log level to persist to database: "error", "warn", "info", "debug", "trace"
    pub log_insertion_level: String,
    /// Application name used in logs
    pub app_name: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GrpcConfig {
    pub address: String,
    pub enable_reflection: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LoggingConfig {
    pub level: String,
    pub format: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FcmConfig {
    pub credentials_path: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DockerConfig {
    pub enabled: bool,
    pub socket_path: String,
    pub check_interval_ms: u64,
}

impl Config {
    pub fn new() -> Result<Self, ConfigError> {
        // Determine the run environment (default to "development")
        let run_env = env::var("RUN_ENV").unwrap_or_else(|_| "development".into());

        let config = ConfigBuilder::builder()
            // Start with default configuration
            .add_source(File::with_name("config/default"))
            // Layer on environment-specific configuration
            .add_source(File::with_name(&format!("config/{}", run_env)).required(false))
            // Override with environment variables (prefix: REMON_)
            // Example: REMON_SERVER__PORT=9000 overrides server.port
            .add_source(
                Environment::with_prefix("REMON")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()?;

        config.try_deserialize()
    }
}
