pub mod auth;
pub mod docker;
pub mod logs;
pub mod misc;
pub mod monitor;
pub mod process;

use crate::state::AppState;

use axum::{
    Router, middleware,
    routing::{delete, get, post},
};
use std::sync::Arc;

/// Creates and returns the REST API routes
/// This includes both public (no auth) and protected (auth required) routes
pub fn create_routes() -> Router<Arc<AppState>> {
    let public_routes = Router::new()
        .route("/hello", get(misc::hello))
        .route("/teapot", get(misc::teapot))
        .route("/health", get(misc::healthcheck))
        .route("/auth/otp/qr", post(auth::get_otp_qr))
        .route("/auth/login", post(auth::login));

    let protected_routes = Router::new()
        // Monitor
        .route("/monitor/description", get(monitor::get_desc))
        .route("/monitor/hardware", get(monitor::get_hardware_info))
        .route("/monitor/cpu", get(monitor::get_cpu_status))
        .route("/monitor/memory", get(monitor::get_mem_status))
        .route("/monitor/disk", get(monitor::get_disk_status))
        .route("/monitor/system", get(monitor::get_system_info))
        .route("/monitor/network", get(monitor::get_network_status))
        .route("/monitor/config", post(monitor::update_info))
        .route("/auth/validate-token", get(monitor::validate_token_test))
        // Process
        .route("/processes", get(process::get_processes))
        .route("/processes/{pid}", delete(process::delete_process))
        // Logs
        .route("/logs/apps", get(logs::get_app_ids_handler))
        .route("/logs", get(logs::get_app_logs_handler))
        // Docker
        .route("/docker/status", get(docker::get_docker_status))
        .route("/docker/containers", get(docker::list_containers))
        .route("/docker/containers/prune", post(docker::prune_containers))
        .route(
            "/docker/containers/{id}",
            get(docker::inspect_container).delete(docker::delete_container),
        )
        .route("/docker/stats", get(docker::get_stats))
        .route(
            "/docker/containers/{id}/stats",
            get(docker::get_container_stats),
        )
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
        .route("/docker/containers/{id}/logs", get(docker::get_logs))
        .route("/docker/images", get(docker::list_images))
        .route("/docker/images/prune", post(docker::prune_images))
        .route("/docker/images/{id}", delete(docker::delete_image))
        .layer(middleware::from_fn(
            crate::routes::middleware::auth_middleware,
        ));

    public_routes.merge(protected_routes)
}
