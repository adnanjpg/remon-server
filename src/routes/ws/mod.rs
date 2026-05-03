#[cfg(feature = "docker")]
pub mod docker;

use crate::state::AppState;

use axum::{Router, middleware};
#[cfg(feature = "docker")]
use axum::routing::get;
use std::sync::Arc;

/// WebSocket routes. All require authentication.
pub fn create_routes(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let router = Router::new();

    #[cfg(feature = "docker")]
    let router = router.route("/docker/containers/{id}/exec", get(docker::docker_exec));

    router.layer(middleware::from_fn_with_state(
        state,
        crate::middleware::auth_middleware,
    ))
}
