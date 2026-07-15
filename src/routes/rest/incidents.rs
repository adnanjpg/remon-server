//! Manual incident capture — the external-trigger side of the flight
//! recorder (`services::incidents`). Alert transitions capture on their own;
//! this endpoint lets an operator, a script, or an external detector
//! (fail2ban action, IDS hook) anchor a snapshot: "record the box, now".

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::routes::extractors::Claims;
use crate::services::incidents;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CaptureRequest {
    /// Why this moment is worth recording (stored, clamped server-side).
    pub reason: String,
    /// resource | availability | security | custom. Default custom.
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CaptureResponse {
    pub id: i64,
}

/// POST /incidents/capture — freeze the current host context.
pub async fn capture(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(req): Json<CaptureRequest>,
) -> AppResult<Json<CaptureResponse>> {
    let reason = req.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest("reason must not be empty".to_string()));
    }
    let category = req.category.as_deref().unwrap_or("custom");
    if !matches!(
        category,
        "resource" | "availability" | "security" | "custom"
    ) {
        return Err(AppError::BadRequest(
            "category must be resource|availability|security|custom".to_string(),
        ));
    }

    let id = incidents::capture_manual(&state, reason, category).await?;
    Ok(Json(CaptureResponse { id }))
}
