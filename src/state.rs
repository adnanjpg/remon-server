use std::sync::Arc;
#[cfg(feature = "docker")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;

use sqlx::SqlitePool;
use tokio::sync::{RwLock, broadcast};

use crate::config::AuthConfig;
use crate::models::process::ProcessList;
use crate::models::stats::StatsEvent;
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
///
/// `collector_stats_base_interval_ms` is the *configured* base for the
/// stats collector. The actually-running interval lives in
/// `AppState.collector_stats_interval_ms` (an `AtomicU64`), and the
/// adaptive sampling task derives it from `base × multiplier` based on
/// subscriber count. PATCH /config updates the base; sampling re-applies
/// it on its next tick.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub server_name: String,
    pub collector_stats_base_interval_ms: u64,
    pub rollup_tick_interval_ms: u64,
    pub retention_tick_interval_ms: u64,
}

/// Shared application state.
///
/// Collector intervals are kept as `Arc<AtomicU64>` (not behind the same
/// `RwLock`) so adaptive-sampling logic can change them on the fly without
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
    pub processes_tx: broadcast::Sender<ProcessList>,

    /// Latest process snapshot, refreshed by the processes collector once
    /// per tick. REST `GET /processes` reads from here in O(microseconds);
    /// the broadcast channel above is reserved for streaming consumers
    /// (none today; future `/sse/processes` would use it).
    ///
    /// Why a separate cache instead of a `subscribe()`-on-broadcast trick:
    /// `broadcast::Receiver` only sees messages sent *after* subscription —
    /// so a fresh `subscribe(); try_recv()` in a request handler is always
    /// empty between collector ticks, forcing a fallback that re-reads
    /// sysinfo with no delta and reports 0% CPU on every process.
    pub processes_latest: Arc<RwLock<Option<ProcessList>>>,

    pub effective_config: Arc<RwLock<EffectiveConfig>>,

    pub collector_stats_interval_ms: Arc<AtomicU64>,
    pub collector_processes_interval_ms: Arc<AtomicU64>,
    #[cfg(feature = "docker")]
    pub collector_docker_interval_ms: Arc<AtomicU64>,

    /// Master kill-switch for the WS Docker exec endpoint. Loaded from
    /// `docker.exec_enabled` at boot; `Arc<AtomicBool>` so a future
    /// PATCH /config can flip it at runtime without a restart.
    #[cfg(feature = "docker")]
    pub docker_exec_enabled: Arc<AtomicBool>,

    /// Hardware inventory captured at boot. `cpu_model`, core counts, total
    /// memory, disk list and NIC list are static for the server's lifetime
    /// in the common case — `Arc` keeps clones cheap when handlers hand out
    /// references. Hot-plug events (USB disk, new NIC) are not reflected
    /// until restart; if that becomes a real need, add a refresh endpoint
    /// rather than re-running sysinfo on every request.
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
    pub fn new(
        db: SqlitePool,
        auth_config: AuthConfig,
        trusted_proxy: bool,
        effective_config: EffectiveConfig,
        collector_stats_interval_ms: u64,
        collector_processes_interval_ms: u64,
        #[cfg(feature = "docker")]
        collector_docker_interval_ms: u64,
        #[cfg(feature = "docker")]
        docker_exec_enabled: bool,
        hardware_info: HardwareInfo,
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
            processes_latest: Arc::new(RwLock::new(None)),
            effective_config: Arc::new(RwLock::new(effective_config)),
            collector_stats_interval_ms: Arc::new(AtomicU64::new(collector_stats_interval_ms)),
            collector_processes_interval_ms: Arc::new(AtomicU64::new(
                collector_processes_interval_ms,
            )),
            #[cfg(feature = "docker")]
            collector_docker_interval_ms: Arc::new(AtomicU64::new(collector_docker_interval_ms)),
            #[cfg(feature = "docker")]
            docker_exec_enabled: Arc::new(AtomicBool::new(docker_exec_enabled)),
            hardware_info: Arc::new(hardware_info),
            service_manager,
            probe_registry,
            notify,
            vapid_keys,
        }
    }
}
