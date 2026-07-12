//! Operator-assistant endpoint — natural-language questions answered from the
//! host's own telemetry via a read-only tool-use loop (see `crate::assistant`).
//!
//! The provider api_key lives in server config and never reaches the client;
//! the browser only ever sends a question and receives an answer.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::assistant::{Assistant, ProposedAction};
use crate::error::{AppError, AppResult};
use crate::routes::extractors::Claims;
use crate::state::AppState;

/// Longest question accepted. Diagnostic prompts are a sentence or two; the cap
/// rejects a pathological body before it reaches the provider.
const MAX_QUESTION_LEN: usize = 2000;

#[derive(Debug, Deserialize)]
pub struct AskRequest {
    pub question: String,
}

#[derive(Debug, Serialize)]
pub struct AskResponse {
    pub answer: String,
    /// Write-actions the assistant drafted. Empty for a pure read/answer. The
    /// client renders each for confirmation and only then calls its `method`
    /// `path`; the daemon performs nothing here.
    pub proposals: Vec<ProposedAction>,
}

/// POST /assistant — ask a plain-language question about this host.
pub async fn ask(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(req): Json<AskRequest>,
) -> AppResult<Json<AskResponse>> {
    let question = req.question.trim();
    if question.is_empty() {
        return Err(AppError::BadRequest(
            "question must not be empty".to_string(),
        ));
    }
    if question.len() > MAX_QUESTION_LEN {
        return Err(AppError::BadRequest(format!(
            "question too long ({} chars, max {MAX_QUESTION_LEN})",
            question.len()
        )));
    }

    // A disabled or key-less assistant is a 503 with a client-safe hint, not a
    // 500 — the operator can act on it.
    let assistant = Assistant::new(state.assistant_config.clone(), state.clone())
        .map_err(|e| AppError::ServiceUnavailable(e.to_string()))?;

    let outcome = assistant
        .ask(question)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(AskResponse {
        answer: outcome.answer,
        proposals: outcome.proposals,
    }))
}
