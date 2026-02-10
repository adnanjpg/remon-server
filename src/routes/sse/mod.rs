pub mod docker;
pub mod stats;

use crate::state::AppState;

use axum::{Router, middleware, routing::get};
use std::sync::Arc;

/// Creates and returns the SSE (Server-Sent Events) routes
/// All SSE routes require authentication
pub fn create_routes() -> Router<Arc<AppState>> {
    Router::new()
        // Unified stats stream
        .route("/stats", get(stats::stream_stats))
        .route("/stats/cpu", get(stats::stream_cpu_stats))
        .route("/stats/memory", get(stats::stream_memory_stats))
        .route("/stats/disk", get(stats::stream_disk_stats))
        .route("/stats/network", get(stats::stream_network_stats))
        // Docker log streaming
        .route(
            "/docker/containers/:id/logs/stream",
            get(docker::stream_logs),
        )
        .layer(middleware::from_fn(
            crate::middleware::auth_middleware,
        ))
}
