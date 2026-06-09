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
    #[serde(default)]
    pub smart: SmartConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub port: u16,
    pub host: String,
    /// True when the server runs behind a reverse proxy that strips
    /// client-controlled `X-Forwarded-For` and appends the real client IP
    /// itself. When set:
    ///   - audit log writes (`devices.last_ip`) use the forwarded address
    ///   - per-IP rate limiting keys by the forwarded address
    ///
    /// Leave false for direct exposure; otherwise an attacker can spoof
    /// XFF and either pollute audit data or bypass rate limits.
    #[serde(default)]
    pub trusted_proxy: bool,
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
    /// SSRF policy master switch. When false (default) webhook URLs that
    /// resolve to loopback, RFC1918, link-local, ULA, or other non-global
    /// addresses are rejected at create-time and at send-time. Flip to
    /// true only on isolated dev / homelab boxes.
    #[serde(default)]
    pub allow_private_targets: bool,
    /// Hostname allow-list — webhook URLs whose host matches one of these
    /// entries (case-insensitive exact match) bypass the private-address
    /// check. Use this for narrowly-scoped exceptions in production
    /// (e.g. "mattermost.internal") instead of the master switch.
    #[serde(default)]
    pub allowed_private_hosts: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DockerConfig {
    pub socket_path: String,
    /// Master kill-switch for the WebSocket /docker/.../exec endpoint.
    /// When false the upgrade refuses with 503 — handy for production where
    /// exec is operationally too risky regardless of token possession.
    /// Defaults to false; opt-in explicitly in config.toml to enable.
    #[serde(default)]
    pub exec_enabled: bool,
}

/// SMART disk-health collection. Wraps the `smartctl` binary
/// (smartmontools) — the de-facto cross-platform way to read SMART;
/// there is no maintained pure-Rust alternative covering ATA + NVMe +
/// USB bridges. When the binary is absent the collector logs once and
/// exits; everything else keeps working.
#[derive(Debug, Deserialize, Clone)]
pub struct SmartConfig {
    /// Master switch. Default true — absence of smartctl degrades
    /// gracefully, so there is no cost on hosts without it.
    #[serde(default = "default_smart_enabled")]
    pub enabled: bool,
    /// Explicit path to smartctl. Empty (default) = resolve from PATH.
    #[serde(default)]
    pub smartctl_path: String,
    /// Poll interval in seconds. Default 1800 (30 min) — SMART moves
    /// slowly and each poll issues real commands to every disk. Floor 60.
    #[serde(default = "default_smart_interval_secs")]
    pub interval_secs: u64,
}

impl Default for SmartConfig {
    fn default() -> Self {
        Self {
            enabled: default_smart_enabled(),
            smartctl_path: String::new(),
            interval_secs: default_smart_interval_secs(),
        }
    }
}

fn default_smart_enabled() -> bool {
    true
}

fn default_smart_interval_secs() -> u64 {
    1800
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
            // Override with environment variables (prefix: REMON).
            // `__` separates both the prefix and the nested keys, so
            // REMON__SERVER__PORT=9000 overrides server.port.
            .add_source(
                Environment::with_prefix("REMON")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()?;

        config.try_deserialize()
    }
}
