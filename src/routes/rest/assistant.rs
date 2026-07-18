//! Operator-assistant endpoint — natural-language questions answered from the
//! host's own telemetry via a read-only tool-use loop (see `crate::assistant`).
//!
//! The provider api_key lives in server config and never reaches the client;
//! the browser only ever sends a question and receives an answer.

use axum::{
    Json,
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::stream::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio_stream::wrappers::ReceiverStream;

use crate::assistant::{
    AskParams, Assistant, DevOverrides, HistoryTurn, ProposedAction, StreamEvent,
};
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

/// Shared request gate for both ask variants: question bounds and the
/// dev-override config check. Returns the loop-ready params.
fn validate(state: &AppState, req: AskRequest) -> Result<AskParams, AppError> {
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

    Ok(AskParams {
        question: question.to_string(),
        history: req.history,
        dev: req.dev,
    })
}

/// POST /assistant — ask a plain-language question about this host.
pub async fn ask(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(req): Json<AskRequest>,
) -> AppResult<Json<AskResponse>> {
    let params = validate(&state, req)?;

    // A disabled or key-less assistant is a 503 with a client-safe hint, not a
    // 500 — the operator can act on it.
    let assistant = Assistant::new(state.assistant_config.clone(), state.clone())
        .map_err(|e| AppError::ServiceUnavailable(e.to_string()))?;

    let outcome = assistant.ask(params).await.map_err(|e| {
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

/// POST /assistant/stream — the same ask, streamed as SSE.
///
/// Events: `step` (model turn / tool call starting), `delta` (answer text as
/// the provider generates it — native Anthropic hosts only), then exactly one
/// terminal `done` (full `AskResponse`, authoritative — clients replace any
/// accumulated deltas with it) or `error`. Pre-loop failures (bad request,
/// disabled assistant) stay plain HTTP errors so clients can distinguish
/// "can't start" from "died mid-answer". A closed connection aborts the loop
/// on its next frame.
pub async fn ask_stream(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(req): Json<AskRequest>,
) -> AppResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let params = validate(&state, req)?;
    let assistant = Assistant::new(state.assistant_config.clone(), state.clone())
        .map_err(|e| AppError::ServiceUnavailable(e.to_string()))?;

    let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(256);
    tokio::spawn(async move {
        let terminal = match assistant.ask_with_events(params, Some(&tx)).await {
            Ok(outcome) => StreamEvent::Done {
                answer: outcome.answer,
                proposals: outcome.proposals,
                trace: outcome.trace,
            },
            Err(e) => {
                let message = if e
                    .downcast_ref::<crate::assistant::ProviderRateLimited>()
                    .is_some()
                {
                    "assistant provider is rate-limited; try again in a minute".to_string()
                } else {
                    // Parity with the non-streaming 500: log the detail, hand
                    // the client a generic message.
                    log::error!("assistant stream failed: {e:#}");
                    "assistant failed — check the server logs".to_string()
                };
                StreamEvent::Failed { message }
            }
        };
        // A send failure just means the client already left.
        let _ = tx.send(terminal).await;
    });

    let stream = ReceiverStream::new(rx).map(|ev| {
        let name = match &ev {
            StreamEvent::Model { .. } | StreamEvent::Tool { .. } => "step",
            StreamEvent::Delta { .. } => "delta",
            StreamEvent::Done { .. } => "done",
            StreamEvent::Failed { .. } => "error",
        };
        let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".to_string());
        Ok(Event::default().event(name).data(data))
    });

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}
