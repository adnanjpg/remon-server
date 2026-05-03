#[cfg(feature = "docker")]
pub mod docker;
pub mod services;
pub mod stats;

use crate::state::AppState;

use axum::{Router, middleware, routing::get};
use std::sync::Arc;

/// SSE routes. All require authentication; `state` is passed through so the
/// auth middleware can perform the jti revocation check.
pub fn create_routes(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let router = Router::new()
        .route("/stats", get(stats::stream_stats))
        .route("/stats/cpu", get(stats::stream_cpu_stats))
        .route("/stats/memory", get(stats::stream_memory_stats))
        .route("/stats/disk", get(stats::stream_disk_stats))
        .route("/stats/network", get(stats::stream_network_stats))
        .route(
            "/services/{name}/logs",
            get(services::stream_service_logs),
        );

    #[cfg(feature = "docker")]
    let router = router.route(
        "/docker/containers/{id}/logs/stream",
        get(docker::stream_logs),
    );

    router.layer(middleware::from_fn_with_state(
        state,
        crate::middleware::auth_middleware,
    ))
}
