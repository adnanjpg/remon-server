use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

use sqlx::SqlitePool;
use tokio::sync::{Mutex, RwLock, broadcast, watch};

use crate::auth::session_cache::SessionCache;
use crate::config::{AssistantConfig, AuthConfig};
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

    /// Read-only operator assistant config (provider/base_url/model/key).
    /// Cloned into a fresh [`crate::assistant::Assistant`] per request; the
    /// api_key stays server-side and is never handed to a client.
    pub assistant_config: AssistantConfig,

    /// When true, audit and rate-limit code reads the client IP from
    /// `X-Forwarded-For` instead of the TCP peer. Mirrors
    /// `[server] trusted_proxy` from config; see ServerConfig for the
    /// trust contract.
    pub trusted_proxy: bool,

    /// Active pairing window (only one at a time; see PAIRING_MAX_ATTEMPTS).
    pub pairing_state: RwLock<Option<PairingState>>,

    /// Positive cache over `sessions` consulted by the auth middleware
    /// before falling back to the DB. Revocation paths must evict —
    /// see [`SessionCache`] for the contract.
    pub session_cache: SessionCache,

    pub stats_tx: broadcast::Sender<StatsEvent>,
    /// Process snapshots are wrapped in `Arc` so a broadcast fan-out and
    /// the latest-snapshot cache share one heap allocation per refresh
    /// (process lists routinely run into the hundreds of entries — each
    /// `ProcessInfo` carries a name, exe path and `Vec<String>` cmd).
    pub processes_tx: broadcast::Sender<Arc<ProcessList>>,

    /// Most recent stats tick — primer source for new SSE subscribers.
    pub stats_latest: Arc<RwLock<Option<AllStats>>>,

    /// Monotonic counter bumped by the stats collector after each
    /// `stats_latest` write. The alert evaluator subscribes and re-evaluates
    /// on change (with a fallback timer for liveness), so alert resolution is
    /// driven by fresh in-memory data rather than its own DB poll.
    pub stats_signal: watch::Sender<u64>,

    /// Latest process snapshot. `GET /processes` refreshes on demand when
    /// the cache is stale; no background scan, since sysinfo's process
    /// enumeration is expensive.
    pub processes_latest: Arc<RwLock<Option<Arc<ProcessList>>>>,
    /// Serializes on-demand process refreshes so a burst of `/processes`
    /// requests cannot all pay the full sysinfo scan at once.
    pub processes_refresh_lock: Arc<Mutex<()>>,
    /// Rolling per-process history (pid → recent samples) fed by the
    /// processes collector when `[assistant] process_history` is on.
    pub process_history:
        Arc<RwLock<std::collections::HashMap<u32, crate::models::process::ProcessHistory>>>,

    pub effective_config: Arc<RwLock<EffectiveConfig>>,

    pub collector_stats_interval_ms: Arc<AtomicU64>,
    /// Cache TTL for on-demand process snapshots (mirrors `collector_processes_interval_ms` DB column).
    pub processes_cache_ttl_ms: Arc<AtomicU64>,
    /// Poll interval for the container-stats collector (mirrors the
    /// `collector_docker_interval_ms` DB column). Re-read each tick.
    pub collector_docker_interval_ms: Arc<AtomicU64>,
    /// Poll interval for the SMART collector (mirrors the
    /// `collector_smart_interval_ms` DB column). Re-read each tick.
    pub collector_smart_interval_ms: Arc<AtomicU64>,

    /// Master kill-switch for the WS Docker exec endpoint. Loaded from
    /// `docker.exec_enabled` at boot; `Arc<AtomicBool>` so a future
    /// PATCH /config can flip it at runtime without a restart.
    #[cfg(feature = "docker")]
    pub docker_exec_enabled: Arc<AtomicBool>,

    /// Set by the SMART collector once it has confirmed a working
    /// `smartctl` binary. `GET /system/smart` reports it so clients can
    /// distinguish "no smartmontools" from "no readings yet". Starts
    /// false; never constructor-injected.
    pub smart_available: AtomicBool,

    /// Hardware inventory captured at boot. Static for the server's
    /// lifetime — hot-plug requires a restart.
    pub hardware_info: Arc<HardwareInfo>,

    /// Platform service manager — systemd / OpenRC on Linux, SCM (via
    /// PowerShell shell-out) on Windows, `Unsupported` fallback elsewhere.
    /// Concrete backend is selected at boot from `platform::init::detect()`.
    pub service_manager: Arc<dyn ServiceManager>,

    /// Loaded probe set + per-probe last result + scheduler join handles.
    /// Populated by `probes::scheduler::load_and_spawn` at boot and again
    /// on `POST /probes/reload`. REST handlers read this for the
    /// list/detail endpoints; history queries go through the DB.
    pub probe_registry: ProbeRegistry,

    /// Pluggable notification fan-out. Channels are loaded from the
    /// `notification_channels` table and hot-reloaded on every CRUD
    /// operation via the REST API. Credentials stay in server config.
    pub notify: Arc<NotificationManager>,

    /// Where producers hand notifications for delivery. Sending is
    /// non-blocking, so an alert transition or a ledger write never waits on a
    /// relay; one owned task does the fan-out and flushes on shutdown. See
    /// `notify::worker`.
    pub notify_queue: crate::notify::NotifyQueue,

    /// VAPID keypair for Web Push. Generated on first boot and persisted
    /// in `vapid_keys`. The public half is served to clients via
    /// `GET /push/vapid-public-key`; the private half signs the JWT we
    /// send to push relays in phase 3.
    pub vapid_keys: Arc<crate::services::webpush::VapidKeyPair>,

    /// Flips to `true` once shutdown has started. The infinite SSE streams
    /// (live stats, container logs, service log follow) race their next
    /// item against this so they end promptly instead of blocking axum's
    /// graceful shutdown forever — see `routes::sse::until_shutdown`.
    pub shutdown: watch::Sender<bool>,

    /// Set when the server is asked to end itself through `/system/restart`
    /// or `/system/shutdown`. Watched by the shutdown signal alongside
    /// SIGTERM, and read once serving stops to pick the exit code — which is
    /// what tells the supervisor whether to bring the agent back.
    pub exit_intent: watch::Sender<Option<ExitIntent>>,
}

impl AppState {
    /// Construct AppState. Caller is responsible for having loaded the
    /// effective config + initial intervals from the database; this just
    /// wires them into the shared structure.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: SqlitePool,
        auth_config: AuthConfig,
        assistant_config: AssistantConfig,
        trusted_proxy: bool,
        effective_config: EffectiveConfig,
        collector_stats_interval_ms: u64,
        processes_cache_ttl_ms: u64,
        collector_docker_interval_ms: u64,
        collector_smart_interval_ms: u64,
        #[cfg(feature = "docker")] docker_exec_enabled: bool,
        hardware_info: Arc<HardwareInfo>,
        service_manager: Arc<dyn ServiceManager>,
        probe_registry: ProbeRegistry,
        notify: Arc<NotificationManager>,
        notify_queue: crate::notify::NotifyQueue,
        vapid_keys: Arc<crate::services::webpush::VapidKeyPair>,
    ) -> Self {
        let (stats_tx, _) = broadcast::channel(64);
        let (processes_tx, _) = broadcast::channel(16);
        let (stats_signal, _) = watch::channel(0u64);
        let (shutdown, _) = watch::channel(false);
        let (exit_intent, _) = watch::channel(None);

        Self {
            db,
            auth_config,
            assistant_config,
            trusted_proxy,
            pairing_state: RwLock::new(None),
            session_cache: SessionCache::new(),
            stats_tx,
            processes_tx,
            stats_signal,
            stats_latest: Arc::new(RwLock::new(None)),
            processes_latest: Arc::new(RwLock::new(None)),
            processes_refresh_lock: Arc::new(Mutex::new(())),
            process_history: Arc::new(RwLock::new(std::collections::HashMap::new())),
            effective_config: Arc::new(RwLock::new(effective_config)),
            collector_stats_interval_ms: Arc::new(AtomicU64::new(collector_stats_interval_ms)),
            processes_cache_ttl_ms: Arc::new(AtomicU64::new(processes_cache_ttl_ms)),
            collector_docker_interval_ms: Arc::new(AtomicU64::new(collector_docker_interval_ms)),
            collector_smart_interval_ms: Arc::new(AtomicU64::new(collector_smart_interval_ms)),
            #[cfg(feature = "docker")]
            docker_exec_enabled: Arc::new(AtomicBool::new(docker_exec_enabled)),
            smart_available: AtomicBool::new(false),
            hardware_info,
            service_manager,
            probe_registry,
            notify,
            notify_queue,
            vapid_keys,
            shutdown,
            exit_intent,
        }
    }
}

/// Why the server is ending, when it was asked to rather than signalled.
///
/// The distinction is entirely about what happens next: every supervisor is
/// configured to restart the agent however it went down, so "come back" is the
/// default and "stay down" is the one that needs arranging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitIntent {
    /// Exit so the supervisor starts a fresh process. Used to apply
    /// boot-time configuration without shell access to the host.
    Restart,
    /// Stop and stay stopped. Only reachable where something outside the
    /// process can be told not to restart it.
    Shutdown,
}
