pub mod auth;
pub mod docker;
pub mod misc;
pub mod pairing;
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
        // Auth endpoints (pairing flow)
        .route("/auth/pair/initiate", post(pairing::initiate_pairing))
        .route("/auth/pair/complete", post(pairing::complete_pairing))
        .route("/auth/login", post(auth::login))
        .route("/auth/refresh", post(auth::refresh))
        // OTP endpoints
        .route("/auth/otp/qr", post(auth::get_otp_qr))
        .route("/auth/login/otp", post(auth::login_otp));

    let protected_routes = Router::new()
        // Process management
        .route("/processes", get(process::get_processes))
        .route("/processes/:pid", delete(process::delete_process))
        // Docker management
        .route("/docker/status", get(docker::get_docker_status))
        .route("/docker/containers", get(docker::list_containers))
        .route("/docker/containers/:id/start", post(docker::start_container))
        .route("/docker/containers/:id/stop", post(docker::stop_container))
        .route("/docker/containers/:id/restart", post(docker::restart_container))
        .route("/docker/containers/:id/pause", post(docker::pause_container))
        .route("/docker/containers/:id/unpause", post(docker::unpause_container))
        .route("/docker/containers/:id", delete(docker::delete_container))
        .route("/docker/containers/:id/inspect", get(docker::inspect_container))
        .route("/docker/containers/:id/logs", get(docker::get_logs))
        .route("/docker/containers/:id/stats", get(docker::get_container_stats))
        .route("/docker/containers/prune", post(docker::prune_containers))
        .route("/docker/images", get(docker::list_images))
        .route("/docker/images/:id", delete(docker::delete_image))
        .route("/docker/images/prune", post(docker::prune_images))
        .layer(middleware::from_fn(
            crate::middleware::auth_middleware,
        ));

    public_routes.merge(protected_routes)
}
