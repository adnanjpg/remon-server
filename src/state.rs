use std::sync::Arc;
#[cfg(feature = "docker")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;

use sqlx::SqlitePool;
use tokio::sync::{Mutex, RwLock, broadcast};

use crate::config::AuthConfig;
use crate::models::process::ProcessList;
use crate::models::stats::{AllStats, StatsEvent};
use crate::models::system::HardwareInfo;
use crate::notify::NotificationManager;
use crate::platform::services::ServiceManager;
use crate::probes::registry::ProbeRegistry;

/// Active pairing code state.
///
/// `attempts_remaining` is decremented on each wrong-code attempt against
/// `complete_pairing` and the state is wiped when it reaches zero — so the
/// brute-force window is bounded even before TTL expiry.
#[derive(Debug, Clone)]
pub struct PairingState {
    pub code: String,
    pub expires_at: i64,
    pub attempts_remaining: u8,
}

/// Maximum wrong-code attempts allowed against a single pairing window.
pub const PAIRING_MAX_ATTEMPTS: u8 = 3;

/// Effective server-side runtime configuration: TOML defaults overridden by
/// values read from the `server_config` table at boot. Held under `RwLock`
/// so PATCH /config can swap it without a restart; background tasks re-read
/// on each tick.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub server_name: String,
    pub rollup_tick_interval_ms: u64,
    pub retention_tick_interval_ms: u64,
}

/// Shared application state.
///
/// Collector intervals are kept as `Arc<AtomicU64>` (not behind the same
/// `RwLock`) so a PATCH /config can change them on the fly without
/// blocking other readers. Each collector loop reloads the value before
/// every tick.
pub struct AppState {
    pub db: SqlitePool,
    pub auth_config: AuthConfig,

    /// When true, audit and rate-limit code reads the client IP from
    /// `X-Forwarded-For` instead of the TCP peer. Mirrors
    /// `[server] trusted_proxy` from config; see ServerConfig for the
    /// trust contract.
    pub trusted_proxy: bool,

    /// Active pairing window (only one at a time; see PAIRING_MAX_ATTEMPTS).
    pub pairing_state: RwLock<Option<PairingState>>,

    pub stats_tx: broadcast::Sender<StatsEvent>,
    /// Process snapshots are wrapped in `Arc` so a broadcast fan-out and
    /// the latest-snapshot cache share one heap allocation per refresh
    /// (process lists routinely run into the hundreds of entries — each
    /// `ProcessInfo` carries a name, exe path and `Vec<String>` cmd).
    pub processes_tx: broadcast::Sender<Arc<ProcessList>>,

    /// Most recent stats tick — primer source for new SSE subscribers.
    pub stats_latest: Arc<RwLock<Option<AllStats>>>,

    /// Latest process snapshot. `GET /processes` refreshes on demand when
    /// the cache is stale; no background scan, since sysinfo's process
    /// enumeration is expensive.
    pub processes_latest: Arc<RwLock<Option<Arc<ProcessList>>>>,
    /// Serializes on-demand process refreshes so a burst of `/processes`
    /// requests cannot all pay the full sysinfo scan at once.
    pub processes_refresh_lock: Arc<Mutex<()>>,

    pub effective_config: Arc<RwLock<EffectiveConfig>>,

    pub collector_stats_interval_ms: Arc<AtomicU64>,
    /// Cache TTL for on-demand process snapshots (mirrors `collector_processes_interval_ms` DB column).
    pub processes_cache_ttl_ms: Arc<AtomicU64>,
    #[cfg(feature = "docker")]
    pub collector_docker_interval_ms: Arc<AtomicU64>,

    /// Master kill-switch for the WS Docker exec endpoint. Loaded from
    /// `docker.exec_enabled` at boot; `Arc<AtomicBool>` so a future
    /// PATCH /config can flip it at runtime without a restart.
    #[cfg(feature = "docker")]
    pub docker_exec_enabled: Arc<AtomicBool>,

    /// Hardware inventory captured at boot. Static for the server's
    /// lifetime — hot-plug requires a restart.
    pub hardware_info: Arc<HardwareInfo>,

    /// Platform service manager — systemd / OpenRC on Linux, SCM (via
    /// PowerShell shell-out) on Windows, `Unsupported` fallback elsewhere.
    /// Concrete backend is selected at boot from `platform::init::detect()`.
    pub service_manager: Arc<dyn ServiceManager>,

    /// Loaded probe set + per-probe last result + scheduler join handles.
    /// Populated by `probes::scheduler::load_and_spawn` at boot and again
    /// on `POST /admin/probes/reload`. REST handlers read this for the
    /// list/detail endpoints; history queries go through the DB.
    pub probe_registry: ProbeRegistry,

    /// Pluggable notification fan-out. Channels are loaded from the
    /// `notification_channels` table and hot-reloaded on every CRUD
    /// operation via the REST API. Credentials stay in server config.
    pub notify: Arc<NotificationManager>,

    /// VAPID keypair for Web Push. Generated on first boot and persisted
    /// in `vapid_keys`. The public half is served to clients via
    /// `GET /push/vapid-public-key`; the private half signs the JWT we
    /// send to push relays in phase 3.
    pub vapid_keys: Arc<crate::services::webpush::VapidKeyPair>,
}

impl AppState {
    /// Construct AppState. Caller is responsible for having loaded the
    /// effective config + initial intervals from the database; this just
    /// wires them into the shared structure.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: SqlitePool,
        auth_config: AuthConfig,
        trusted_proxy: bool,
        effective_config: EffectiveConfig,
        collector_stats_interval_ms: u64,
        processes_cache_ttl_ms: u64,
        #[cfg(feature = "docker")] collector_docker_interval_ms: u64,
        #[cfg(feature = "docker")] docker_exec_enabled: bool,
        hardware_info: Arc<HardwareInfo>,
        service_manager: Arc<dyn ServiceManager>,
        probe_registry: ProbeRegistry,
        notify: Arc<NotificationManager>,
        vapid_keys: Arc<crate::services::webpush::VapidKeyPair>,
    ) -> Self {
        let (stats_tx, _) = broadcast::channel(64);
        let (processes_tx, _) = broadcast::channel(16);

        Self {
            db,
            auth_config,
            trusted_proxy,
            pairing_state: RwLock::new(None),
            stats_tx,
            processes_tx,
            stats_latest: Arc::new(RwLock::new(None)),
            processes_latest: Arc::new(RwLock::new(None)),
            processes_refresh_lock: Arc::new(Mutex::new(())),
            effective_config: Arc::new(RwLock::new(effective_config)),
            collector_stats_interval_ms: Arc::new(AtomicU64::new(collector_stats_interval_ms)),
            processes_cache_ttl_ms: Arc::new(AtomicU64::new(processes_cache_ttl_ms)),
            #[cfg(feature = "docker")]
            collector_docker_interval_ms: Arc::new(AtomicU64::new(collector_docker_interval_ms)),
            #[cfg(feature = "docker")]
            docker_exec_enabled: Arc::new(AtomicBool::new(docker_exec_enabled)),
            hardware_info,
            service_manager,
            probe_registry,
            notify,
            vapid_keys,
        }
    }
}
