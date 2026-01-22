pub mod docker;

use axum::{Router, middleware, routing::get};

/// Creates and returns the SSE (Server-Sent Events) routes
/// All SSE routes require authentication
pub fn create_routes() -> Router {
    Router::new()
        // Docker log streaming
        .route(
            "/docker/containers/{id}/logs/stream",
            get(docker::stream_logs),
        )
        .layer(middleware::from_fn(crate::routes::middleware::auth_middleware))
}
