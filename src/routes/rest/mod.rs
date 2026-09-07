pub mod actions;
pub mod admin;
pub mod alerts;
pub mod assistant;
pub mod auth;
pub mod cron;
#[cfg(feature = "docker")]
pub mod docker;
pub mod events;
pub mod heartbeats;
pub mod incidents;
pub mod logs;
pub mod me;
pub mod metrics;
pub mod misc;
pub mod notifications;
pub mod pairing;
pub mod ping;
pub mod probes;
pub mod process;
pub mod push;
pub mod services;
pub mod system;

use crate::state::AppState;

use axum::{
    Router, middleware,
    routing::{delete, get, post, put},
};
use std::sync::Arc;
use std::time::Duration;
use tower_governor::{
    GovernorLayer, governor::GovernorConfigBuilder, key_extractor::SmartIpKeyExtractor,
};
use tower_http::limit::RequestBodyLimitLayer;

/// Bodies on anonymous auth endpoints are tiny: device_id + device_token +
/// optional device_name + optional fcm_token. 8 KiB leaves plenty of room
/// for the longest reasonable shape and rejects DoS-by-large-body before
/// any handler code runs.
const ANON_AUTH_BODY_LIMIT: usize = 8 * 1024;

/// The heartbeat ping router. Anonymous — the slug is the credential —
/// so it gets its own per-IP governor, sized for the legitimate case of
/// many cron jobs behind one NAT (burst 60, then ~1 req/s sustained)
/// rather than the auth limiter's brute-force posture. Online brute force
/// against a 128-bit slug space is a non-issue at any of these rates.
fn ping_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/ping/{slug}",
            get(ping::ping_success).post(ping::ping_success),
        )
        .route(
            "/ping/{slug}/fail",
            get(ping::ping_fail).post(ping::ping_fail),
        )
        // POST-only: a pasted URL must not let a link prefetcher flip
        // monitoring state.
        .route("/ping/{slug}/pause", post(ping::ping_pause))
        .route("/ping/{slug}/resume", post(ping::ping_resume))
        .route(
            "/ping/{slug}/{exit_code}",
            get(ping::ping_exit_code).post(ping::ping_exit_code),
        )
}

/// The operator-assistant route, kept separate from `create_routes` so
/// `build_app` can give it a longer request timeout: a multi-step tool-use
/// loop legitimately runs past the 30s cap that fits ordinary REST calls.
/// Still auth-gated — it's merged inside the protected router in `build_app`.
pub fn create_assistant_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/assistant", post(assistant::ask))
        .route("/assistant/stream", post(assistant::ask_stream))
}

/// Build the REST router. Public routes are merged with protected routes; the
/// latter run through `auth_middleware` (which needs `AppState` for the
/// session/jti revocation check, hence the explicit `state` argument).
pub fn create_routes(state: Arc<AppState>) -> Router<Arc<AppState>> {
    // Per-IP rate limit on unauthenticated auth endpoints. Replenishes one
    // token every 12 seconds with a burst of 5 — i.e. a single IP can fire
    // five quick attempts then settles to ~5 req/min. Combined with the
    // 8-digit pairing code + 3-attempts cap this puts online
    // brute-force well out of reach.
    //
    // Key extraction depends on deployment: behind a reverse proxy the TCP
    // peer is always 127.0.0.1, so the default `PeerIpKeyExtractor` would
    // collapse the entire internet into one rate-limit bucket. When
    // `trusted_proxy` is set the smart extractor honours `X-Forwarded-For`
    // / `X-Real-IP` instead.
    // Background reaper to evict idle IP entries so memory stays bounded
    // under abuse. Spawned once per limiter; duplicated inside each branch
    // because the limiter's concrete type depends on the key extractor and
    // a generic helper would mean importing `governor`'s internals.
    let (rate_limited_auth, rate_limited_ping) = if state.trusted_proxy {
        let conf = Arc::new(
            GovernorConfigBuilder::default()
                .per_second(12)
                .burst_size(5)
                .key_extractor(SmartIpKeyExtractor)
                .finish()
                .expect("valid governor config"),
        );
        let limiter = conf.limiter().clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                limiter.retain_recent();
            }
        });
        let auth_router = Router::new()
            .route("/auth/pair/initiate", post(pairing::initiate_pairing))
            .route("/auth/pair/complete", post(pairing::complete_pairing))
            .route("/auth/login", post(auth::login))
            .route("/auth/refresh", post(auth::refresh))
            .layer(GovernorLayer::new(conf))
            .layer(RequestBodyLimitLayer::new(ANON_AUTH_BODY_LIMIT));

        let ping_conf = Arc::new(
            GovernorConfigBuilder::default()
                .per_second(1)
                .burst_size(60)
                .key_extractor(SmartIpKeyExtractor)
                .finish()
                .expect("valid governor config"),
        );
        let ping_limiter = ping_conf.limiter().clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                ping_limiter.retain_recent();
            }
        });
        // No per-route body limit: fail bodies are read capped (4 KiB)
        // inside the handlers so an oversized trace truncates instead of
        // 413-ing away the fail signal; the app-wide 64 KiB limit stays.
        let ping_router = ping_routes().layer(GovernorLayer::new(ping_conf));

        (auth_router, ping_router)
    } else {
        let conf = Arc::new(
            GovernorConfigBuilder::default()
                .per_second(12)
                .burst_size(5)
                .finish()
                .expect("valid governor config"),
        );
        let limiter = conf.limiter().clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                limiter.retain_recent();
            }
        });
        let auth_router = Router::new()
            .route("/auth/pair/initiate", post(pairing::initiate_pairing))
            .route("/auth/pair/complete", post(pairing::complete_pairing))
            .route("/auth/login", post(auth::login))
            .route("/auth/refresh", post(auth::refresh))
            .layer(GovernorLayer::new(conf))
            .layer(RequestBodyLimitLayer::new(ANON_AUTH_BODY_LIMIT));

        let ping_conf = Arc::new(
            GovernorConfigBuilder::default()
                .per_second(1)
                .burst_size(60)
                .finish()
                .expect("valid governor config"),
        );
        let ping_limiter = ping_conf.limiter().clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                ping_limiter.retain_recent();
            }
        });
        let ping_router = ping_routes().layer(GovernorLayer::new(ping_conf));

        (auth_router, ping_router)
    };

    let public_routes = Router::new()
        .route("/health", get(misc::healthcheck))
        .route("/ready", get(misc::ready))
        .merge(rate_limited_auth)
        .merge(rate_limited_ping);

    let protected_routes = Router::new()
        .route("/auth/logout", post(auth::logout))
        // Self (calling device) endpoints
        .route("/me/fcm-token", axum::routing::patch(me::update_fcm_token))
        // Session/device management — list/rename/revoke any paired device.
        .route("/me/sessions", get(me::list_sessions))
        .route(
            "/me/sessions/{id}",
            axum::routing::patch(me::rename_session).delete(me::revoke_session),
        )
        // Web Push — public VAPID key + per-device subscription register.
        .route("/push/vapid-public-key", get(push::vapid_public_key))
        .route(
            "/me/push-subscription",
            post(push::subscribe_push).delete(push::unsubscribe_push),
        )
        // Local host description + hardware inventory (uptime is fresh; the
        // rest is cached at boot — see services/system.rs).
        .route("/system/info", get(system::get_system_info))
        // SMART disk health — latest reading per device (smartctl-backed).
        .route("/system/smart", get(system::get_smart))
        // Lifecycle. The deliberate counterparts to what /processes/{pid} and
        // /services/{name} refuse when they name this server.
        .route("/system/restart", post(system::restart_server))
        .route("/system/shutdown", post(system::shutdown_server))
        // One-call host overview — fleet/multi-server clients poll this
        // once per daemon instead of fanning out to info/metrics/alerts.
        .route("/summary", get(system::get_summary))
        // Runtime configuration
        .route("/config", get(admin::get_config).patch(admin::patch_config))
        .route(
            "/config/retention",
            get(admin::get_retention).patch(admin::patch_retention),
        )
        .route("/config/resolutions", get(admin::get_resolutions))
        .route(
            "/config/resolutions/{name}",
            axum::routing::patch(admin::patch_resolution),
        )
        // Alert engine: rule CRUD + active-state + event log
        .route(
            "/alerts",
            get(alerts::list_alerts).post(alerts::create_alert),
        )
        .route("/alerts/state", get(alerts::list_active_state))
        .route("/alerts/events", get(alerts::list_recent_events))
        .route("/alerts/schema", get(alerts::get_alerts_schema))
        .route(
            "/alerts/{id}",
            get(alerts::get_alert)
                .put(alerts::update_alert)
                .delete(alerts::delete_alert),
        )
        .route("/alerts/{id}/events", get(alerts::list_events_for_rule))
        .route(
            "/alerts/{id}/silence",
            post(alerts::silence_alert).delete(alerts::unsilence_alert),
        )
        // Alert actions — the other end of the fanout. A notification tells
        // someone; a binding does something. Bindings hang off a rule, so
        // they are created there and managed under /actions.
        .route(
            "/alerts/{id}/actions",
            get(actions::list_bindings_for_rule).post(actions::create_binding),
        )
        .route("/actions", get(actions::list_catalog))
        .route("/actions/reload", post(actions::reload_actions))
        .route("/actions/bindings", get(actions::list_bindings))
        .route("/actions/runs", get(actions::list_runs))
        .route(
            "/actions/bindings/{id}",
            get(actions::get_binding)
                .put(actions::update_binding)
                .delete(actions::delete_binding),
        )
        .route("/actions/bindings/{id}/run", post(actions::run_binding))
        .route("/actions/runs/{id}", get(actions::get_run))
        .route("/actions/runs/{id}/confirm", post(actions::confirm_run))
        .route("/actions/runs/{id}/dismiss", post(actions::dismiss_run))
        // Incident flight recorder — manual/external capture trigger.
        // Alert transitions capture on their own (services::incidents).
        .route("/incidents/capture", post(incidents::capture))
        // Reading them back. The timeline points at these ids, and by the time
        // anyone follows one the rollup has already thinned the series.
        .route("/incidents", get(incidents::list))
        .route("/incidents/{id}", get(incidents::get))
        // Unified event timeline — host_events ∪ alert_events ∪ incidents,
        // one normalized stream for chart annotations and feeds.
        .route("/events", get(events::list_events))
        // Time-series history
        .route("/metrics/cpu", get(metrics::cpu_history))
        .route("/metrics/cpu/cores", get(metrics::cpu_cores_history))
        .route("/metrics/memory", get(metrics::memory_history))
        .route("/metrics/disk", get(metrics::disk_history))
        .route("/metrics/network", get(metrics::network_history))
        .route("/metrics/network/usage", get(metrics::network_usage))
        .route("/metrics/docker/{container}", get(metrics::docker_history))
        .route(
            "/metrics/pressure/{resource}",
            get(metrics::pressure_history),
        )
        .route("/metrics/components", get(metrics::components_history))
        .route("/metrics/batch", get(metrics::batch_history))
        .route("/logs", get(logs::list_logs))
        .route("/processes", get(process::get_processes))
        .route("/processes/{pid}", delete(process::delete_process))
        // Init-system services (systemd / OpenRC / Windows SCM)
        .route("/services", get(services::list_services))
        .route("/services/{name}", get(services::get_service))
        .route("/services/{name}/start", post(services::start_service))
        .route("/services/{name}/stop", post(services::stop_service))
        .route("/services/{name}/restart", post(services::restart_service))
        .route("/services/{name}/reload", post(services::reload_service))
        .route("/services/{name}/enable", put(services::enable_service))
        .route("/services/{name}/disable", put(services::disable_service))
        // systemd timers (501 elsewhere)
        .route("/timers", get(services::list_timers))
        .route("/timers/{name}/enable", put(services::enable_timer))
        .route("/timers/{name}/disable", put(services::disable_timer))
        // Cron job listing (Unix; empty list elsewhere)
        .route("/cron", get(cron::list_cron_jobs))
        // Probes — pluggable external check scripts. Metric values
        // land in `metrics_probe` and are queryable like any other
        // host metric via `/metrics/probe/{probe}/{metric}`.
        .route("/probes", get(probes::list_probes))
        .route("/probes/{name}", get(probes::get_probe))
        .route("/probes/{name}/history", get(probes::get_probe_history))
        .route("/probes/reload", post(probes::reload_probes))
        .route(
            "/metrics/probe/{probe_name}/{metric_name}",
            get(probes::get_probe_metric_history),
        )
        // Heartbeat checks — push-model dead-man's switches. The paired
        // anonymous ping surface is `ping_routes()` above; alerting goes
        // through the `heartbeat` resolver namespace (heartbeat.up < 1).
        .route(
            "/heartbeats",
            get(heartbeats::list_heartbeats).post(heartbeats::create_heartbeat),
        )
        .route(
            "/heartbeats/{id}",
            get(heartbeats::get_heartbeat)
                .put(heartbeats::update_heartbeat)
                .delete(heartbeats::delete_heartbeat),
        )
        .route(
            "/heartbeats/{id}/pause",
            post(heartbeats::pause_heartbeat).delete(heartbeats::resume_heartbeat),
        )
        .route(
            "/heartbeats/{id}/rotate-slug",
            post(heartbeats::rotate_heartbeat_slug),
        )
        .route(
            "/heartbeats/{id}/pings",
            get(heartbeats::list_heartbeat_pings),
        )
        // Notification channels
        .route(
            "/notifications/channels",
            get(notifications::list_channels).post(notifications::create_channel),
        )
        .route(
            "/notifications/channels/{id}",
            put(notifications::update_channel).delete(notifications::delete_channel),
        )
        .route(
            "/notifications/channels/{id}/test",
            post(notifications::test_channel),
        );

    #[cfg(feature = "docker")]
    let protected_routes = protected_routes
        .route("/docker/status", get(docker::get_docker_status))
        .route("/docker/containers", get(docker::list_containers))
        .route(
            "/docker/containers/{id}/start",
            post(docker::start_container),
        )
        .route("/docker/containers/{id}/stop", post(docker::stop_container))
        .route(
            "/docker/containers/{id}/restart",
            post(docker::restart_container),
        )
        .route(
            "/docker/containers/{id}/pause",
            post(docker::pause_container),
        )
        .route(
            "/docker/containers/{id}/unpause",
            post(docker::unpause_container),
        )
        .route("/docker/containers/{id}", delete(docker::delete_container))
        .route(
            "/docker/containers/{id}/inspect",
            get(docker::inspect_container),
        )
        .route("/docker/containers/{id}/logs", get(docker::get_logs))
        .route(
            "/docker/containers/{id}/stats",
            get(docker::get_container_stats),
        )
        .route("/docker/containers/prune", post(docker::prune_containers))
        .route("/docker/images", get(docker::list_images))
        .route("/docker/images/{id}", delete(docker::delete_image))
        .route("/docker/images/prune", post(docker::prune_images));

    let protected_routes = protected_routes.layer(middleware::from_fn_with_state(
        state,
        crate::middleware::auth_middleware,
    ));

    public_routes.merge(protected_routes)
}
