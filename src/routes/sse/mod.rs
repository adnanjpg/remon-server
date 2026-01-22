pub mod docker;
pub mod system;

use crate::state::AppState;

use axum::{Router, middleware, routing::get};
use std::sync::Arc;

/// Creates and returns the SSE (Server-Sent Events) routes
/// All SSE routes require authentication
pub fn create_routes() -> Router<Arc<AppState>> {
    Router::new()
        // System metrics streaming
        .route("/monitor/cpu", get(system::stream_cpu_stats))
        .route("/monitor/memory", get(system::stream_memory_stats))
        .route("/monitor/disk", get(system::stream_disk_stats))
        .route("/monitor/network", get(system::stream_network_stats))
        // Docker log streaming
        .route(
            "/docker/containers/{id}/logs/stream",
            get(docker::stream_logs),
        )
        .layer(middleware::from_fn(
            crate::routes::middleware::auth_middleware,
        ))
}
