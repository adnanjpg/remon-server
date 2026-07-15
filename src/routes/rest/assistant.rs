//! Operator-assistant endpoint — natural-language questions answered from the
//! host's own telemetry via a read-only tool-use loop (see `crate::assistant`).
//!
//! The provider api_key lives in server config and never reaches the client;
//! the browser only ever sends a question and receives an answer.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::assistant::{AskParams, Assistant, DevOverrides, HistoryTurn, ProposedAction};
use crate::error::{AppError, AppResult};
use crate::routes::extractors::Claims;
use crate::state::AppState;

/// Longest question accepted. Diagnostic prompts are a sentence or two; the cap
/// rejects a pathological body before it reaches the provider.
const MAX_QUESTION_LEN: usize = 2000;

#[derive(Debug, Deserialize)]
pub struct AskRequest {
    pub question: String,
    /// Prior turns of this conversation, oldest first. The daemon is
    /// stateless; the client replays what it wants remembered (capped and
    /// clipped server-side).
    #[serde(default)]
    pub history: Vec<HistoryTurn>,
    /// Per-ask dev overrides — rejected unless `[assistant] dev = true`.
    #[serde(default)]
    pub dev: Option<DevOverrides>,
}

#[derive(Debug, Serialize)]
pub struct AskResponse {
    pub answer: String,
    /// Write-actions the assistant drafted. Empty for a pure read/answer. The
    /// client renders each for confirmation and only then calls its `method`
    /// `path`; the daemon performs nothing here.
    pub proposals: Vec<ProposedAction>,
    /// Loop trace (model turns + tool calls) — present only when dev mode
    /// requested it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<Vec<serde_json::Value>>,
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

    // Dev overrides are a config-gated capability: without the flag the
    // request is refused outright rather than silently stripped, so a client
    // never mistakes a locked-down answer for a dev-mode one.
    if req.dev.is_some() && !state.assistant_config.dev {
        return Err(AppError::Forbidden(
            "assistant dev mode is disabled ([assistant] dev = false)".to_string(),
        ));
    }

    // A disabled or key-less assistant is a 503 with a client-safe hint, not a
    // 500 — the operator can act on it.
    let assistant = Assistant::new(state.assistant_config.clone(), state.clone())
        .map_err(|e| AppError::ServiceUnavailable(e.to_string()))?;

    let outcome = assistant
        .ask(AskParams {
            question: question.to_string(),
            history: req.history,
            dev: req.dev,
        })
        .await
        .map_err(|e| {
            // Provider rate limits are a caller-actionable condition (wait,
            // re-ask) — answer 503 with a plain hint instead of an opaque 500.
            if e.downcast_ref::<crate::assistant::ProviderRateLimited>()
                .is_some()
            {
                AppError::ServiceUnavailable(
                    "assistant provider is rate-limited; try again in a minute".to_string(),
                )
            } else {
                AppError::Internal(e.to_string())
            }
        })?;

    Ok(Json(AskResponse {
        answer: outcome.answer,
        proposals: outcome.proposals,
        trace: outcome.trace,
    }))
}
