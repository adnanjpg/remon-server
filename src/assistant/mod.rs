//! Read-only operator assistant.
//!
//! Runs an agentic tool-use loop against an OpenAI-compatible chat endpoint
//! (Gemini's compat surface by default; also Groq, Ollama, OpenRouter, ...).
//! Anthropic hosts are detected from `base_url` and speak the native Messages
//! API instead (see `anthropic.rs`) — same loop, plus prompt caching and
//! richer error semantics. Transient provider failures (429/5xx/network)
//! retry with bounded backoff before surfacing.
//! The model is handed read-only tools over this host's own telemetry and
//! answers operator questions in plain language. It cannot change anything on
//! the host; every tool is a read.
//!
//! The provider key is server-side only. It is loaded from `[assistant]`
//! config and used to sign the outbound request; it never crosses to a client.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use log::{debug, warn};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Instant;

use crate::config::AssistantConfig;
use crate::state::AppState;

mod anthropic;
pub(crate) mod tools;

/// A write-action the assistant drafted but did **not** perform. The daemon
/// never mutates state from the assistant loop; a proposal is handed to the
/// client, which renders it for the operator and — only on explicit confirm —
/// calls `method path` (with `body`) against the normal REST API, where the
/// real validation and audit live. This keeps the LLM strictly propose-only.
#[derive(Debug, Clone, Serialize)]
pub struct ProposedAction {
    /// Machine tag for the client to pick an icon/label, e.g. "create_alert".
    pub kind: String,
    /// One-line human description shown on the confirm card.
    pub summary: String,
    /// HTTP method the confirm will use.
    pub method: String,
    /// REST path the confirm will call (may carry a query string).
    pub path: String,
    /// Request body for POST/PUT; absent for parameterless actions.
    pub body: Option<Value>,
}

/// Result of one `ask`: the natural-language answer plus any actions the model
/// drafted for operator confirmation. `trace` is populated only when dev mode
/// asked for it: one entry per model turn (usage, latency) and per tool call
/// (args, result preview, latency), for iterating on prompts and tools.
#[derive(Debug, Clone)]
pub struct AskOutcome {
    pub answer: String,
    pub proposals: Vec<ProposedAction>,
    pub trace: Option<Vec<Value>>,
}

/// One prior question/answer pair replayed for conversational context. The
/// client owns the conversation (the daemon stays stateless); it sends back
/// what it wants remembered.
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryTurn {
    pub question: String,
    pub answer: String,
}

/// Progress frames emitted while an ask streams. `Model`/`Tool` mark loop
/// activity (tier 1: perceived latency), `Delta` carries answer text as the
/// provider generates it (tier 2: native Anthropic only — OpenAI-compat
/// providers get tier 1 and the answer arrives whole in `Done`). `Done` and
/// `Failed` are terminal and appended by the REST layer, never by the loop.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamEvent {
    Model {
        step: usize,
    },
    Tool {
        step: usize,
        name: String,
    },
    Delta {
        text: String,
    },
    Done {
        answer: String,
        proposals: Vec<ProposedAction>,
        #[serde(skip_serializing_if = "Option::is_none")]
        trace: Option<Vec<Value>>,
    },
    Failed {
        message: String,
    },
}

/// Where a streaming ask reports progress. A dropped receiver (client gone)
/// makes the next send fail, which aborts the loop — no tokens burn for a
/// listener that left.
pub type EventSink = tokio::sync::mpsc::Sender<StreamEvent>;

/// Send one progress frame, translating a closed channel into an abort.
async fn emit(sink: Option<&EventSink>, ev: StreamEvent) -> Result<()> {
    if let Some(sink) = sink
        && sink.send(ev).await.is_err()
    {
        bail!("assistant stream client disconnected");
    }
    Ok(())
}

/// Marker error: the provider refused with 429 even after the bounded
/// retries. Unlike a genuine provider fault this is actionable by the caller
/// (wait a moment, ask again), so the REST layer downcasts it into a
/// client-safe 503 instead of an opaque 500. anyhow preserves the type
/// through added context, so the downcast survives the `ask` pipeline.
#[derive(Debug, thiserror::Error)]
#[error("assistant provider is rate-limited: {0}")]
pub struct ProviderRateLimited(pub String);

/// Dev-mode overrides for a single ask. Only honored when `[assistant]
/// dev = true`; the handler rejects them otherwise. Auth and the read-only /
/// propose-only tool contract still apply — this loosens the frame (persona,
/// limits), never the safety model.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DevOverrides {
    /// Replace the built-in system prompt for this ask (prompt iteration
    /// without a rebuild). Empty/absent keeps the default.
    #[serde(default)]
    pub system: Option<String>,
    /// Use a different model for this ask (same provider/base_url) — for
    /// side-by-side quality/cost comparisons, e.g. claude-haiku-4-5 vs
    /// claude-sonnet-5 on the same question.
    #[serde(default)]
    pub model: Option<String>,
    /// Raise the tool-loop cap (still bounded server-side).
    #[serde(default)]
    pub max_steps: Option<usize>,
    /// Raise the per-turn output ceiling (still bounded server-side).
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Talk to the bare model: no tools advertised at all.
    #[serde(default)]
    pub no_tools: bool,
    /// Return the loop trace (model turns + tool calls) with the answer.
    #[serde(default)]
    pub trace: bool,
}

/// Everything one `ask` needs. `history` is capped and replayed as plain
/// user/assistant turns ahead of the new question.
#[derive(Debug, Clone, Default)]
pub struct AskParams {
    pub question: String,
    pub history: Vec<HistoryTurn>,
    pub dev: Option<DevOverrides>,
}

/// Replayed history is bounded so a chatty client can't grow the prompt
/// without limit: at most this many most-recent turns...
const MAX_HISTORY_TURNS: usize = 12;
/// ...and each replayed answer is clipped to this many chars.
const MAX_HISTORY_ANSWER_CHARS: usize = 4000;

/// Dev mode can raise limits, but never unbounded.
const DEV_MAX_STEPS_CEILING: usize = 50;
const DEV_MAX_TOKENS_CEILING: u32 = 32_768;

/// Chars of each tool result echoed into the trace.
const TRACE_RESULT_PREVIEW_CHARS: usize = 600;

/// Hard cap on model/tool round trips per question. A read-only diagnostic
/// answer needs a handful of tool calls at most; the cap bounds cost and
/// stops a confused model from looping forever.
const MAX_STEPS: usize = 10;

/// Per-request timeout for a single chat/completions round trip.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

const SYSTEM_PROMPT: &str = "\
You are the operator assistant for a self-hosted server monitored by Remon. \
Answer the operator's question by calling the provided tools to read this \
server's live telemetry. Tools: get_summary (host overview), list_processes \
(top CPU/memory consumers), active_alerts (what is alarming now), \
recent_alert_events (fire/resolve timeline — when things started), read_logs \
(this daemon's recent errors), read_service_logs (any systemd unit's journal tail), query_metric (current value of any metric), \
metric_history (min/max/avg/trend of a metric over a window), list_services \
(and list_containers / read_container_logs where present), read_system_events \
(OS-level errors: OOM kills, segfaults, disk errors), and prometheus_query \
where a Prometheus server is configured. Incident snapshots: when an alert \
crossed its threshold the daemon froze the box's context — list_incidents \
then incident_detail answer 'what caused that alert at 03:12' with the \
processes, vitals and errors of that exact moment; capture_incident freezes \
the current moment when you see something anomalous no alert covers.\n\
\n\
Method: start broad (get_summary, active_alerts), then drill down. For a slow \
or stalling host specifically, check the real stall signals — pressure \
(query_metric namespace 'pressure', resource cpu|memory|io), cpu.iowait_percent, \
memory.swap_used_bytes and load — before blaming a single process. Use \
metric_history to tell a spike from the steady state and recent_alert_events to \
place an incident in time; for a past event, check list_incidents before \
reconstructing from metrics.\n\
\n\
Ground every claim in concrete numbers from tool results and name the source. \
Never assume host capacities (total memory, disk size, core count) — read them \
from get_summary before drawing conclusions from them. When citing a number, \
say whether it is the current value or a window average; they answer different \
questions. If the tools do not cover something, say so plainly rather than \
guessing. Keep answers short.\n\
\n\
Reading is free; changing anything is not. When the operator asks you to create \
or silence an alert, control a service or container, or kill a process, use the \
matching propose_* tool. Those tools do NOT perform the action — they draft it \
for the operator to confirm in the UI. Never claim you have done something; say \
you have prepared it and it is awaiting their confirmation. Prefer proposing the \
least drastic action that solves the problem, and briefly say why.";

/// A configured assistant bound to the shared app state its tools read from.
pub struct Assistant {
    http: Client,
    cfg: AssistantConfig,
    state: Arc<AppState>,
}

impl Assistant {
    /// Build an assistant, or fail if the feature is not usable yet. The two
    /// gates (disabled, missing key) surface as errors so the handler can turn
    /// them into a clear client message instead of a silent no-op.
    pub fn new(cfg: AssistantConfig, state: Arc<AppState>) -> Result<Self> {
        if !cfg.enabled {
            bail!("assistant is disabled in config ([assistant] enabled = false)");
        }
        if cfg.api_key.trim().is_empty() {
            bail!("assistant api_key is not set (REMON__ASSISTANT__API_KEY)");
        }
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("build assistant http client")?;
        Ok(Self { http, cfg, state })
    }

    /// Run the tool-use loop for one operator question. Returns the model's
    /// final answer plus any write-actions it drafted for confirmation.
    /// Prior turns arrive as `history` (client-owned conversation, replayed
    /// as plain text) so follow-ups like "do all of those" resolve.
    pub async fn ask(&self, params: AskParams) -> Result<AskOutcome> {
        self.ask_with_events(params, None).await
    }

    /// Same loop, reporting progress into `sink` as it goes: a frame per model
    /// turn and tool call, plus answer-text deltas on native Anthropic hosts.
    /// The final outcome still returns from the function — the sink carries
    /// progress only, so `ask` and the streaming route share one code path.
    pub async fn ask_with_events(
        &self,
        params: AskParams,
        sink: Option<&EventSink>,
    ) -> Result<AskOutcome> {
        let dev = params.dev.unwrap_or_default();
        let system = dev
            .system
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(SYSTEM_PROMPT);
        let model = dev
            .model
            .as_deref()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or(&self.cfg.model);
        let max_steps = dev
            .max_steps
            .unwrap_or(MAX_STEPS)
            .min(DEV_MAX_STEPS_CEILING);
        let max_tokens = dev
            .max_tokens
            .unwrap_or(self.cfg.max_tokens)
            .min(DEV_MAX_TOKENS_CEILING);
        let with_tools = !dev.no_tools;

        let mut messages = vec![json!({ "role": "system", "content": system })];
        let skip = params.history.len().saturating_sub(MAX_HISTORY_TURNS);
        for turn in params.history.iter().skip(skip) {
            let mut answer = turn.answer.as_str();
            if answer.len() > MAX_HISTORY_ANSWER_CHARS {
                let mut end = MAX_HISTORY_ANSWER_CHARS;
                while !answer.is_char_boundary(end) {
                    end -= 1;
                }
                answer = &answer[..end];
            }
            messages.push(json!({ "role": "user", "content": turn.question }));
            messages.push(json!({ "role": "assistant", "content": answer }));
        }
        messages.push(json!({ "role": "user", "content": params.question }));

        // Write-actions the model drafts via `propose_*` tools accumulate here
        // and ride back on the outcome; the loop itself never mutates state.
        let mut proposals: Vec<ProposedAction> = Vec::new();
        let mut trace: Vec<Value> = Vec::new();

        for step in 0..max_steps {
            emit(sink, StreamEvent::Model { step }).await?;
            let turn_started = Instant::now();
            let (message, usage) = self
                .chat(&messages, model, max_tokens, with_tools, sink)
                .await?;
            if dev.trace {
                trace.push(json!({
                    "type": "model",
                    "step": step,
                    "model": model,
                    "ms": turn_started.elapsed().as_millis() as u64,
                    "usage": usage,
                }));
            }

            let tool_calls = message.get("tool_calls").and_then(Value::as_array).cloned();
            if let Some(calls) = tool_calls
                && !calls.is_empty()
            {
                // Echo the assistant turn (with its tool_calls) back verbatim,
                // then answer each call with a `tool` message. The provider
                // pairs them by tool_call_id.
                messages.push(message);
                for call in &calls {
                    let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
                    let name = call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let args = call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or_else(|| json!({}));
                    debug!("assistant tool call: {name} {args}");
                    emit(
                        sink,
                        StreamEvent::Tool {
                            step,
                            name: name.to_string(),
                        },
                    )
                    .await?;
                    let tool_started = Instant::now();
                    let result =
                        tools::dispatch_collecting(&self.state, name, &args, &mut proposals).await;
                    if dev.trace {
                        let preview: String =
                            result.chars().take(TRACE_RESULT_PREVIEW_CHARS).collect();
                        trace.push(json!({
                            "type": "tool",
                            "step": step,
                            "name": name,
                            "args": args,
                            "result_preview": preview,
                            "ms": tool_started.elapsed().as_millis() as u64,
                        }));
                    }
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "content": result,
                    }));
                }
                continue;
            }

            // No tool calls: this turn is the final answer.
            let answer = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if answer.is_empty() {
                warn!("assistant returned an empty answer at step {step}");
                bail!("assistant returned an empty answer");
            }
            return Ok(AskOutcome {
                answer,
                proposals,
                trace: dev.trace.then_some(trace),
            });
        }

        bail!("assistant exceeded {max_steps} tool-use steps without answering");
    }

    /// One provider round trip; returns the assistant message in OpenAI shape
    /// (plus the provider's `usage` object, when present) regardless of the
    /// wire format underneath. Anthropic hosts get the native Messages API
    /// (for prompt caching); everything else speaks OpenAI-compat. Responses
    /// are navigated as untyped JSON so provider-specific extra fields pass
    /// through unharmed. With a `sink`, native hosts stream the turn and
    /// forward text deltas; compat hosts keep the buffered round trip.
    async fn chat(
        &self,
        messages: &[Value],
        model: &str,
        max_tokens: u32,
        with_tools: bool,
        sink: Option<&EventSink>,
    ) -> Result<(Value, Option<Value>)> {
        let base = self.cfg.base_url.trim_end_matches('/');

        if anthropic::is_native(base) {
            let url = format!("{base}/messages");
            let tools = if with_tools {
                tools::definitions(&self.state)
            } else {
                json!([])
            };
            let mut body = anthropic::build_body(model, max_tokens, messages, &tools);
            if sink.is_some() {
                body["stream"] = json!(true);
            }
            let request = || {
                self.http
                    .post(&url)
                    .header("x-api-key", &self.cfg.api_key)
                    .header("anthropic-version", anthropic::API_VERSION)
                    .json(&body)
            };
            let payload = if sink.is_some() {
                let resp = self.send_checked_with_retry(request).await?;
                anthropic::read_stream(resp, sink).await?
            } else {
                self.send_with_retry(request).await?
            };
            let usage = payload.get("usage").cloned();
            if let Some(usage) = &usage {
                log::info!(
                    "assistant usage: in={} out={} cache_write={} cache_read={}",
                    usage
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    usage
                        .get("cache_creation_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    usage
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                );
            }
            return Ok((anthropic::to_openai_message(&payload)?, usage));
        }

        let url = format!("{base}/chat/completions");
        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": messages,
        });
        if with_tools {
            body["tools"] = tools::definitions(&self.state);
        }
        let payload = self
            .send_with_retry(|| {
                self.http
                    .post(&url)
                    .bearer_auth(&self.cfg.api_key)
                    .json(&body)
            })
            .await?;

        let message = payload
            .pointer("/choices/0/message")
            .cloned()
            .context("assistant response had no choices[0].message")?;
        Ok((message, payload.get("usage").cloned()))
    }

    /// `send_checked_with_retry` + JSON body parse, for buffered round trips.
    async fn send_with_retry(&self, build: impl Fn() -> reqwest::RequestBuilder) -> Result<Value> {
        self.send_checked_with_retry(build)
            .await?
            .json()
            .await
            .context("assistant response was not valid json")
    }

    /// Send a provider request with bounded retries on transient failures:
    /// network errors, 429 (rate limit) and 5xx (incl. Anthropic's 529
    /// overloaded). Waits honor Retry-After when present (capped so a hostile
    /// header can't stall the loop), otherwise back off 1s → 3s. Anything else
    /// — or exhausted retries — surfaces as the usual provider error. Returns
    /// the raw success response so streaming callers can read the body as SSE.
    async fn send_checked_with_retry(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        const BACKOFFS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];
        const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);

        let mut attempt = 0;
        loop {
            let resp = match build().send().await {
                Ok(resp) => resp,
                Err(e) if attempt < BACKOFFS.len() => {
                    warn!("assistant request failed ({e}), retrying");
                    tokio::time::sleep(BACKOFFS[attempt]).await;
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err(e).context("assistant request failed"),
            };

            let status = resp.status();
            if (status.as_u16() == 429 || status.is_server_error()) && attempt < BACKOFFS.len() {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(BACKOFFS[attempt])
                    .min(MAX_RETRY_AFTER);
                warn!(
                    "assistant provider {status}, retrying in {}s (attempt {}/{})",
                    wait.as_secs(),
                    attempt + 1,
                    BACKOFFS.len()
                );
                tokio::time::sleep(wait).await;
                attempt += 1;
                continue;
            }

            if !status.is_success() {
                let payload: Value = resp
                    .json()
                    .await
                    .context("assistant response was not valid json")?;
                let detail = payload
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown provider error");
                // A 429 that survived the retries is a state the CALLER can
                // act on (wait and re-ask), unlike a genuine provider fault —
                // surface it typed so the REST layer can answer 503, not 500.
                if status.as_u16() == 429 {
                    return Err(anyhow::Error::new(ProviderRateLimited(detail.to_string())));
                }
                bail!("assistant provider error ({status}): {detail}");
            }
            return Ok(resp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The REST layer's 503 mapping depends on downcasting the typed marker
    /// out of an anyhow chain — verify context wrapping doesn't bury it.
    #[test]
    fn rate_limited_marker_survives_context_layers() {
        let err = anyhow::Error::new(ProviderRateLimited("quota exceeded".into()))
            .context("assistant request failed");
        assert!(err.downcast_ref::<ProviderRateLimited>().is_some());
    }
}
