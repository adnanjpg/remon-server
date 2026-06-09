use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode};
use log::warn;

use crate::state::AppState;

pub async fn hello() -> &'static str {
    "Hello World!"
}

pub async fn teapot() -> (StatusCode, &'static str) {
    (StatusCode::IM_A_TEAPOT, "I'm a teapot!")
}

pub async fn healthcheck() -> &'static str {
    "Running smoothly!"
}

/// GET /ready — readiness, as opposed to `/health`'s liveness. A process
/// that answers `/health` can still be unable to serve anything real (DB
/// file locked, pool exhausted); this adds a DB round-trip so orchestrators
/// and peer-health probes can tell the two states apart. Unauthenticated by
/// design — the failure detail stays in the server log, not the response.
pub async fn ready(State(state): State<Arc<AppState>>) -> (StatusCode, Json<serde_json::Value>) {
    match sqlx::query("SELECT 1").execute(&state.db).await {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "ready" })),
        ),
        Err(e) => {
            warn!("ready: DB ping failed: {e}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "status": "not_ready", "failed_check": "db" })),
            )
        }
    }
}
