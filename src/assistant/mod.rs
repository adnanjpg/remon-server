//! Read-only operator assistant.
//!
//! Runs an agentic tool-use loop against an OpenAI-compatible chat endpoint
//! (Gemini's compat surface by default; also Groq, Ollama, OpenRouter, ...).
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
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::AssistantConfig;
use crate::state::AppState;

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
/// drafted for operator confirmation.
#[derive(Debug, Clone)]
pub struct AskOutcome {
    pub answer: String,
    pub proposals: Vec<ProposedAction>,
}

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
(this daemon's recent errors), query_metric (current value of any metric), \
metric_history (min/max/avg/trend of a metric over a window), list_services \
(and list_containers / read_container_logs where present), and prometheus_query \
where a Prometheus server is configured.\n\
\n\
Method: start broad (get_summary, active_alerts), then drill down. For a slow \
or stalling host specifically, check the real stall signals — pressure \
(query_metric namespace 'pressure', resource cpu|memory|io), cpu.iowait_percent, \
memory.swap_used_bytes and load — before blaming a single process. Use \
metric_history to tell a spike from the steady state and recent_alert_events to \
place an incident in time.\n\
\n\
Ground every claim in concrete numbers from tool results and name the source. \
If the tools do not cover something, say so plainly rather than guessing. Keep \
answers short.\n\
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
    pub async fn ask(&self, question: &str) -> Result<AskOutcome> {
        let mut messages = vec![
            json!({ "role": "system", "content": SYSTEM_PROMPT }),
            json!({ "role": "user", "content": question }),
        ];
        // Write-actions the model drafts via `propose_*` tools accumulate here
        // and ride back on the outcome; the loop itself never mutates state.
        let mut proposals: Vec<ProposedAction> = Vec::new();

        for step in 0..MAX_STEPS {
            let message = self.chat(&messages).await?;

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
                    let result =
                        tools::dispatch_collecting(&self.state, name, &args, &mut proposals).await;
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
            return Ok(AskOutcome { answer, proposals });
        }

        bail!("assistant exceeded {MAX_STEPS} tool-use steps without answering");
    }

    /// One round trip to `{base_url}/chat/completions`; returns the assistant
    /// message object (`choices[0].message`). The response is navigated as
    /// untyped JSON so provider-specific extra fields pass through unharmed.
    async fn chat(&self, messages: &[Value]) -> Result<Value> {
        let url = format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        );
        let body = json!({
            "model": self.cfg.model,
            "max_tokens": self.cfg.max_tokens,
            "messages": messages,
            "tools": tools::definitions(&self.state),
        });

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.cfg.api_key)
            .json(&body)
            .send()
            .await
            .context("assistant request failed")?;

        let status = resp.status();
        let payload: Value = resp
            .json()
            .await
            .context("assistant response was not valid json")?;

        if !status.is_success() {
            let detail = payload
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown provider error");
            bail!("assistant provider error ({status}): {detail}");
        }

        payload
            .pointer("/choices/0/message")
            .cloned()
            .context("assistant response had no choices[0].message")
    }
}
