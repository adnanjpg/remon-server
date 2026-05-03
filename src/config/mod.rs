use config::{Config as ConfigBuilder, ConfigError, Environment, File};
use serde::Deserialize;
use std::env;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub auth: AuthConfig,
    pub monitoring: MonitoringConfig,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    pub docker: DockerConfig,
    #[serde(default)]
    pub cors: CorsConfig,
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
    /// Minimum log level to persist to database: "error", "warn", "info", "debug", "trace"
    pub log_insertion_level: String,
    /// Application name used in logs
    pub app_name: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LoggingConfig {
    pub level: String,
    pub format: String,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct NotificationsConfig {
    #[serde(default)]
    pub fcm: FcmCredentials,
    #[serde(default)]
    pub telegram: TelegramCredentials,
    #[serde(default)]
    pub ntfy: NtfyCredentials,
    #[serde(default)]
    pub webhook: WebhookCredentials,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct FcmCredentials {
    /// Path to the Firebase service account JSON file.
    #[serde(default)]
    pub service_account_path: String,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct TelegramCredentials {
    /// Telegram Bot API token (from @BotFather).
    #[serde(default)]
    pub bot_token: String,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct NtfyCredentials {
    /// Optional Bearer token for authenticated ntfy servers.
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct WebhookCredentials {
    /// Optional secret sent as `Authorization: Bearer <secret>`.
    #[serde(default)]
    pub secret: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DockerConfig {
    pub socket_path: String,
    /// Master kill-switch for the WebSocket /docker/.../exec endpoint.
    /// When false the upgrade refuses with 503 — handy for production where
    /// exec is operationally too risky regardless of token possession.
    #[serde(default = "default_true")]
    pub exec_enabled: bool,
}

fn default_true() -> bool {
    true
}

/// CORS policy.
///
/// `allow_any_origin = true` is fine for local development (browser running
/// on localhost:5173 hitting the API on localhost:8080). For production,
/// flip it to false and put the real frontend origin(s) in
/// `allowed_origins` — wildcard with credentialed requests would be a
/// browser-rejected misconfiguration anyway.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct CorsConfig {
    #[serde(default)]
    pub allow_any_origin: bool,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
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
