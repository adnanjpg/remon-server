pub mod admin;
pub mod alerts;
pub mod auth;
pub mod cron;
#[cfg(feature = "docker")]
pub mod docker;
pub mod me;
pub mod metrics;
pub mod misc;
pub mod notifications;
pub mod pairing;
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
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};

/// Build the REST router. Public routes are merged with protected routes; the
/// latter run through `auth_middleware` (which needs `AppState` for the
/// session/jti revocation check, hence the explicit `state` argument).
pub fn create_routes(state: Arc<AppState>) -> Router<Arc<AppState>> {
    // Per-IP rate limit on unauthenticated auth endpoints. Replenishes one
    // token every 12 seconds with a burst of 5 — i.e. a single IP can fire
    // five quick attempts then settles to ~5 req/min. Combined with the
    // 8-digit pairing code + 3-attempts cap this puts online
    // brute-force well out of reach.
    let governor_conf = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(12)
            .burst_size(5)
            .finish()
            .expect("valid governor config"),
    );

    // Background reaper to evict idle IP entries from the limiter so memory
    // stays bounded under abuse. Runs once a minute on the tokio runtime
    // so it shares scheduler threads with the rest of the server (no extra
    // OS thread parked in blocking sleep).
    let limiter = governor_conf.limiter().clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.tick().await; // first tick fires immediately; discard it
        loop {
            tick.tick().await;
            limiter.retain_recent();
        }
    });

    let rate_limited_auth = Router::new()
        .route("/auth/pair/initiate", post(pairing::initiate_pairing))
        .route("/auth/pair/complete", post(pairing::complete_pairing))
        .route("/auth/login", post(auth::login))
        .route("/auth/refresh", post(auth::refresh))
        .layer(GovernorLayer::new(governor_conf));

    let public_routes = Router::new()
        .route("/hello", get(misc::hello))
        .route("/teapot", get(misc::teapot))
        .route("/health", get(misc::healthcheck))
        .merge(rate_limited_auth);

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
        // Runtime configuration
        .route("/config", get(admin::get_config).patch(admin::patch_config))
        // Alert engine v2: rule CRUD + active-state + event log
        .route("/alerts", get(alerts::list_alerts).post(alerts::create_alert))
        .route("/alerts/state", get(alerts::list_active_state))
        .route("/alerts/events", get(alerts::list_recent_events))
        .route(
            "/alerts/{id}",
            get(alerts::get_alert)
                .put(alerts::update_alert)
                .delete(alerts::delete_alert),
        )
        .route("/alerts/{id}/events", get(alerts::list_events_for_rule))
        // Time-series history
        .route("/metrics/cpu", get(metrics::cpu_history))
        .route("/metrics/cpu/cores", get(metrics::cpu_cores_history))
        .route("/metrics/memory", get(metrics::memory_history))
        .route("/metrics/disk", get(metrics::disk_history))
        .route("/metrics/network", get(metrics::network_history))
        .route("/metrics/pressure/{resource}", get(metrics::pressure_history))
        .route("/metrics/components", get(metrics::components_history))
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
        .route("/admin/probes/reload", post(probes::reload_probes))
        .route(
            "/metrics/probe/{probe_name}/{metric_name}",
            get(probes::get_probe_metric_history),
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
        .route("/docker/containers/{id}/start", post(docker::start_container))
        .route("/docker/containers/{id}/stop", post(docker::stop_container))
        .route("/docker/containers/{id}/restart", post(docker::restart_container))
        .route("/docker/containers/{id}/pause", post(docker::pause_container))
        .route("/docker/containers/{id}/unpause", post(docker::unpause_container))
        .route("/docker/containers/{id}", delete(docker::delete_container))
        .route("/docker/containers/{id}/inspect", get(docker::inspect_container))
        .route("/docker/containers/{id}/logs", get(docker::get_logs))
        .route("/docker/containers/{id}/stats", get(docker::get_container_stats))
        .route("/docker/containers/prune", post(docker::prune_containers))
        .route("/docker/images", get(docker::list_images))
        .route("/docker/images/{id}", delete(docker::delete_image))
        .route("/docker/images/prune", post(docker::prune_images));

    let protected_routes = protected_routes
        .layer(middleware::from_fn_with_state(
            state,
            crate::middleware::auth_middleware,
        ));

    public_routes.merge(protected_routes)
}
