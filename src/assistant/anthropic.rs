//! Native Anthropic Messages API provider.
//!
//! Selected automatically when `base_url` points at api.anthropic.com; every
//! other provider keeps the OpenAI-compat path. The agentic loop in `mod.rs`
//! stays in OpenAI wire shapes — this module converts at the request/response
//! boundary only, so the loop, tools, and tests are provider-agnostic.
//!
//! Why native instead of Anthropic's own OpenAI-compat endpoint: prompt
//! caching. The tool schemas + system prompt prefix is re-sent on every round
//! trip of the tool loop; with a `cache_control` breakpoint it is served from
//! cache at ~0.1x input price after the first round. A second breakpoint on
//! the last message block caches the growing conversation within a question.
//! (Caching has a model-dependent minimum prefix size — small prefixes
//! silently don't cache; harmless, just no discount.)

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// Required `anthropic-version` header for the Messages API.
pub(super) const API_VERSION: &str = "2023-06-01";

/// Assistant messages synthesized from a native response carry the original
/// Anthropic content blocks under this key, so they can be echoed back
/// verbatim (tool_use blocks included) on the next round trip. The key is
/// stripped before sending and never reaches an OpenAI-compat provider.
const NATIVE_CONTENT_KEY: &str = "_anthropic_content";

/// True when the configured base_url targets the Anthropic API host.
pub(super) fn is_native(base_url: &str) -> bool {
    base_url.contains("api.anthropic.com")
}

/// Build a `/v1/messages` body from the loop's OpenAI-shaped state.
///
/// Conversions:
/// - leading `system` message → top-level `system` block with `cache_control`
///   (tools render before system, so this one breakpoint caches both)
/// - `user` string content → a text block
/// - `assistant` turns → their preserved native content blocks
/// - consecutive `tool` messages → ONE `user` message of `tool_result` blocks
///   (Anthropic requires all parallel results in a single following message)
/// - OpenAI `{type: function, function: {parameters}}` tools →
///   `{name, description, input_schema}`
pub(super) fn build_body(
    model: &str,
    max_tokens: u32,
    messages: &[Value],
    openai_tools: &Value,
) -> Value {
    let mut system: Option<Value> = None;
    let mut out: Vec<Value> = Vec::new();
    let mut pending_results: Vec<Value> = Vec::new();

    let flush_results = |out: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if !pending.is_empty() {
            out.push(json!({ "role": "user", "content": std::mem::take(pending) }));
        }
    };

    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or_default();
        match role {
            "system" => {
                let text = msg
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                system = Some(json!([{
                    "type": "text",
                    "text": text,
                    "cache_control": { "type": "ephemeral" },
                }]));
            }
            "user" => {
                flush_results(&mut out, &mut pending_results);
                let text = msg
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                out.push(json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": text }],
                }));
            }
            "assistant" => {
                flush_results(&mut out, &mut pending_results);
                // Echo the native blocks back verbatim; a text-only fallback
                // covers the (unexpected) case of a foreign assistant turn.
                let content = msg.get(NATIVE_CONTENT_KEY).cloned().unwrap_or_else(|| {
                    let text = msg
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    json!([{ "type": "text", "text": text }])
                });
                out.push(json!({ "role": "assistant", "content": content }));
            }
            "tool" => {
                pending_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": msg.get("tool_call_id").cloned().unwrap_or_default(),
                    "content": msg.get("content").cloned().unwrap_or_default(),
                }));
            }
            _ => {}
        }
    }
    flush_results(&mut out, &mut pending_results);

    // Second breakpoint: cache the conversation up to the newest block, so
    // each round of the tool loop re-reads the previous rounds from cache.
    if let Some(last_block) = out
        .last_mut()
        .and_then(|m| m.get_mut("content"))
        .and_then(Value::as_array_mut)
        .and_then(|blocks| blocks.last_mut())
        .and_then(Value::as_object_mut)
    {
        last_block.insert("cache_control".into(), json!({ "type": "ephemeral" }));
    }

    let tools: Vec<Value> = openai_tools
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let f = t.get("function")?;
                    Some(json!({
                        "name": f.get("name")?,
                        "description": f.get("description").cloned().unwrap_or_default(),
                        "input_schema": f.get("parameters").cloned()
                            .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": out,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(system) = system {
        body["system"] = system;
    }
    body
}

/// Map a Messages API response onto the OpenAI-shaped assistant message the
/// loop expects: text blocks concatenate into `content`, `tool_use` blocks
/// become `tool_calls`, and the original blocks ride along for echo-back.
pub(super) fn to_openai_message(payload: &Value) -> Result<Value> {
    let blocks = payload
        .get("content")
        .and_then(Value::as_array)
        .context("anthropic response had no content blocks")?;

    let mut text_parts: Vec<&str> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    text_parts.push(t);
                }
            }
            Some("tool_use") => {
                let args = block.get("input").cloned().unwrap_or_else(|| json!({}));
                tool_calls.push(json!({
                    "id": block.get("id").cloned().unwrap_or_default(),
                    "type": "function",
                    "function": {
                        "name": block.get("name").cloned().unwrap_or_default(),
                        // The loop parses `arguments` as a JSON string, so
                        // serialize the structured input back to a string.
                        "arguments": args.to_string(),
                    },
                }));
            }
            _ => {}
        }
    }

    let mut message = json!({
        "role": "assistant",
        "content": text_parts.join("\n"),
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    // Strip cache_control from the echoed copy: breakpoints are per-request
    // decisions made in build_body, not conversation state.
    message[NATIVE_CONTENT_KEY] = Value::Array(
        blocks
            .iter()
            .cloned()
            .map(|mut b| {
                if let Some(obj) = b.as_object_mut() {
                    obj.remove("cache_control");
                }
                b
            })
            .collect(),
    );
    Ok(message)
}

/// Read a Messages API SSE response (`"stream": true`) and reconstruct the
/// same payload shape the non-streaming endpoint returns, so
/// `to_openai_message` works identically on both paths. Text deltas are
/// forwarded to `sink` as they arrive; a dropped sink (client gone) aborts
/// the read so provider tokens stop burning.
pub(super) async fn read_stream(
    resp: reqwest::Response,
    sink: Option<&super::EventSink>,
) -> Result<Value> {
    use futures_util::StreamExt;

    let mut assembler = StreamAssembler::default();
    // Byte buffer split at b'\n': a multi-byte UTF-8 char never spans an SSE
    // line break, so per-line lossy decoding is safe across chunk boundaries.
    let mut buf: Vec<u8> = Vec::new();
    let mut event_name = String::new();
    let mut data_buf = String::new();
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("assistant stream read failed")?;
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line_owned = String::from_utf8_lossy(&line_bytes);
            let line = line_owned.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !event_name.is_empty() || !data_buf.is_empty() {
                    let text = assembler.apply(&event_name, &data_buf)?;
                    if let (Some(sink), Some(text)) = (sink, text)
                        && sink.send(super::StreamEvent::Delta { text }).await.is_err()
                    {
                        anyhow::bail!("assistant stream client disconnected");
                    }
                }
                event_name.clear();
                data_buf.clear();
            } else if let Some(rest) = line.strip_prefix("event:") {
                event_name = rest.trim_start().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data_buf.is_empty() {
                    data_buf.push('\n');
                }
                data_buf.push_str(rest.trim_start());
            }
            // ':' comments and other SSE fields are ignored.
        }
    }
    assembler.finish()
}

/// Incremental assembler for the Messages API streaming frames
/// (`message_start` → `content_block_*` → `message_delta` → `message_stop`).
/// `apply` returns the text of a `text_delta` frame so the caller can forward
/// it; every other frame returns `None`.
#[derive(Default)]
struct StreamAssembler {
    blocks: Vec<Value>,
    /// Accumulated `input_json_delta` fragments per tool_use block index.
    partial_json: std::collections::HashMap<usize, String>,
    usage: Value,
}

impl StreamAssembler {
    fn apply(&mut self, event: &str, data: &str) -> Result<Option<String>> {
        let v: Value = serde_json::from_str(data).unwrap_or(Value::Null);
        match event {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    self.usage = u.clone();
                }
            }
            "content_block_start" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let block = v.get("content_block").cloned().unwrap_or(Value::Null);
                while self.blocks.len() <= index {
                    self.blocks.push(Value::Null);
                }
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    self.partial_json.insert(index, String::new());
                }
                self.blocks[index] = block;
            }
            "content_block_delta" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                match v.pointer("/delta/type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = v
                            .pointer("/delta/text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if let Some(block) = self.blocks.get_mut(index)
                            && let Some(Value::String(cur)) = block.get_mut("text")
                        {
                            cur.push_str(&text);
                        }
                        return Ok(Some(text));
                    }
                    Some("input_json_delta") => {
                        let part = v
                            .pointer("/delta/partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        self.partial_json.entry(index).or_default().push_str(part);
                    }
                    // Extended thinking: the block starts as `{"type":"thinking","thinking":""}`
                    // and streams its reasoning text the same way a text block streams
                    // `text_delta` — then a trailing `signature_delta` seals it. Both must be
                    // captured verbatim: an echoed-back thinking block with an empty
                    // `thinking` field or a missing `signature` is a 400 on the next turn
                    // ("each thinking block must contain thinking"). Not forwarded to the
                    // client — thinking is scratch reasoning, not answer text.
                    Some("thinking_delta") => {
                        let text = v
                            .pointer("/delta/thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let Some(block) = self.blocks.get_mut(index)
                            && let Some(Value::String(cur)) = block.get_mut("thinking")
                        {
                            cur.push_str(text);
                        }
                    }
                    Some("signature_delta") => {
                        let sig = v
                            .pointer("/delta/signature")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let Some(block) = self.blocks.get_mut(index) {
                            block["signature"] = json!(sig);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(acc) = self.partial_json.remove(&index) {
                    // Empty accumulation means a no-arg tool call.
                    let input: Value = if acc.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&acc)
                            .with_context(|| format!("tool_use input was not valid json: {acc}"))?
                    };
                    if let Some(block) = self.blocks.get_mut(index) {
                        block["input"] = input;
                    }
                }
            }
            "message_delta" => {
                if let Some(out) = v.pointer("/usage/output_tokens").cloned()
                    && let Some(usage) = self.usage.as_object_mut()
                {
                    usage.insert("output_tokens".into(), out);
                }
            }
            "error" => {
                let msg = v
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown provider stream error");
                anyhow::bail!("assistant provider stream error: {msg}");
            }
            // ping / message_stop / unknown frames carry nothing to keep.
            _ => {}
        }
        Ok(None)
    }

    /// Produce the non-streaming-shaped payload: `{ content, usage }`.
    fn finish(self) -> Result<Value> {
        let blocks: Vec<Value> = self.blocks.into_iter().filter(|b| !b.is_null()).collect();
        if blocks.is_empty() {
            anyhow::bail!("assistant stream ended without content blocks");
        }
        Ok(json!({ "content": blocks, "usage": self.usage }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_native_host() {
        assert!(is_native("https://api.anthropic.com/v1"));
        assert!(!is_native(
            "https://generativelanguage.googleapis.com/v1beta/openai"
        ));
        assert!(!is_native("https://openrouter.ai/api/v1"));
    }

    #[test]
    fn body_converts_system_tools_and_groups_tool_results() {
        let messages = vec![
            json!({ "role": "system", "content": "be brief" }),
            json!({ "role": "user", "content": "cpu?" }),
            json!({
                "role": "assistant", "content": "",
                "tool_calls": [{ "id": "t1", "type": "function",
                    "function": { "name": "get_cpu", "arguments": "{}" } }],
                "_anthropic_content": [
                    { "type": "tool_use", "id": "t1", "name": "get_cpu", "input": {} }
                ],
            }),
            json!({ "role": "tool", "tool_call_id": "t1", "content": "91.4" }),
            json!({ "role": "tool", "tool_call_id": "t2", "content": "7.8" }),
        ];
        let tools = json!([{ "type": "function", "function": {
            "name": "get_cpu", "description": "d",
            "parameters": { "type": "object", "properties": {} } } }]);

        let body = build_body("claude-haiku-4-5", 1024, &messages, &tools);

        // System block carries the cache breakpoint.
        assert_eq!(body["system"][0]["text"], "be brief");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        // OpenAI tool wrapper unwrapped to the native shape.
        assert_eq!(body["tools"][0]["name"], "get_cpu");
        assert!(body["tools"][0]["input_schema"].is_object());
        // user, assistant, then BOTH tool results merged into one user message.
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2]["role"], "user");
        let results = msgs[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["type"], "tool_result");
        assert_eq!(results[0]["tool_use_id"], "t1");
        // Conversation breakpoint sits on the newest block only.
        assert_eq!(results[1]["cache_control"]["type"], "ephemeral");
        assert!(results[0].get("cache_control").is_none());
    }

    #[test]
    fn response_maps_text_and_tool_use() {
        let payload = json!({
            "content": [
                { "type": "text", "text": "checking" },
                { "type": "tool_use", "id": "toolu_1", "name": "get_summary",
                  "input": { "window": "1h" } },
            ],
        });
        let msg = to_openai_message(&payload).unwrap();
        assert_eq!(msg["content"], "checking");
        assert_eq!(msg["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "get_summary");
        // Arguments arrive as a JSON string, as the loop expects.
        let args: Value = serde_json::from_str(
            msg["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(args["window"], "1h");
        // Native blocks preserved for echo-back.
        assert_eq!(msg["_anthropic_content"][1]["type"], "tool_use");
    }

    /// The assembler must rebuild exactly what the non-streaming endpoint
    /// would have returned: text concatenated, tool_use input parsed from
    /// accumulated fragments, usage merged from start + delta frames.
    #[test]
    fn stream_assembler_rebuilds_payload() {
        let mut a = StreamAssembler::default();
        let frames: Vec<(&str, String)> = vec![
            (
                "message_start",
                json!({ "message": { "usage": { "input_tokens": 100, "cache_read_input_tokens": 90, "output_tokens": 1 } } }).to_string(),
            ),
            (
                "content_block_start",
                json!({ "index": 0, "content_block": { "type": "text", "text": "" } }).to_string(),
            ),
            (
                "content_block_delta",
                json!({ "index": 0, "delta": { "type": "text_delta", "text": "cpu is " } }).to_string(),
            ),
            (
                "content_block_delta",
                json!({ "index": 0, "delta": { "type": "text_delta", "text": "fine" } }).to_string(),
            ),
            ("content_block_stop", json!({ "index": 0 }).to_string()),
            (
                "content_block_start",
                json!({ "index": 1, "content_block": { "type": "tool_use", "id": "toolu_9", "name": "query_metric", "input": {} } }).to_string(),
            ),
            (
                "content_block_delta",
                json!({ "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"name\":" } }).to_string(),
            ),
            (
                "content_block_delta",
                json!({ "index": 1, "delta": { "type": "input_json_delta", "partial_json": "\"cpu.usage\"}" } }).to_string(),
            ),
            ("content_block_stop", json!({ "index": 1 }).to_string()),
            (
                "message_delta",
                json!({ "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 42 } }).to_string(),
            ),
            ("message_stop", json!({}).to_string()),
        ];

        let mut texts: Vec<String> = Vec::new();
        for (event, data) in frames {
            if let Some(t) = a.apply(event, &data).unwrap() {
                texts.push(t);
            }
        }
        assert_eq!(texts, vec!["cpu is ".to_string(), "fine".to_string()]);

        let payload = a.finish().unwrap();
        assert_eq!(payload["content"][0]["text"], "cpu is fine");
        assert_eq!(payload["content"][1]["type"], "tool_use");
        assert_eq!(payload["content"][1]["input"]["name"], "cpu.usage");
        assert_eq!(payload["usage"]["input_tokens"], 100);
        assert_eq!(payload["usage"]["output_tokens"], 42);

        // And the rebuilt payload must satisfy the existing converter.
        let msg = to_openai_message(&payload).unwrap();
        assert_eq!(msg["content"], "cpu is fine");
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "query_metric");
    }

    /// Reproduces the "each thinking block must contain thinking" 400: an
    /// extended-thinking block's `thinking_delta`/`signature_delta` frames
    /// must be captured, not dropped, or the echoed-back block is empty and
    /// the next turn's request is rejected by the Anthropic API.
    #[test]
    fn stream_assembler_captures_thinking_block() {
        let mut a = StreamAssembler::default();
        let frames: Vec<(&str, String)> = vec![
            (
                "content_block_start",
                json!({ "index": 0, "content_block": { "type": "thinking", "thinking": "" } })
                    .to_string(),
            ),
            (
                "content_block_delta",
                json!({ "index": 0, "delta": { "type": "thinking_delta", "thinking": "checking " } })
                    .to_string(),
            ),
            (
                "content_block_delta",
                json!({ "index": 0, "delta": { "type": "thinking_delta", "thinking": "cpu load" } })
                    .to_string(),
            ),
            (
                "content_block_delta",
                json!({ "index": 0, "delta": { "type": "signature_delta", "signature": "sig123" } })
                    .to_string(),
            ),
            ("content_block_stop", json!({ "index": 0 }).to_string()),
            (
                "content_block_start",
                json!({ "index": 1, "content_block": { "type": "text", "text": "" } }).to_string(),
            ),
            (
                "content_block_delta",
                json!({ "index": 1, "delta": { "type": "text_delta", "text": "cpu is fine" } })
                    .to_string(),
            ),
            ("content_block_stop", json!({ "index": 1 }).to_string()),
            ("message_stop", json!({}).to_string()),
        ];

        for (event, data) in frames {
            a.apply(event, &data).unwrap();
        }

        let payload = a.finish().unwrap();
        assert_eq!(payload["content"][0]["type"], "thinking");
        assert_eq!(payload["content"][0]["thinking"], "checking cpu load");
        assert_eq!(payload["content"][0]["signature"], "sig123");

        // Echoed back verbatim for the next round trip — this is exactly
        // what Anthropic validates on the following request.
        let msg = to_openai_message(&payload).unwrap();
        assert_eq!(
            msg["_anthropic_content"][0]["thinking"],
            "checking cpu load"
        );
        assert_eq!(msg["_anthropic_content"][0]["signature"], "sig123");
    }

    #[test]
    fn stream_assembler_surfaces_provider_error() {
        let mut a = StreamAssembler::default();
        let err = a
            .apply(
                "error",
                &json!({ "error": { "type": "overloaded_error", "message": "overloaded" } })
                    .to_string(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("overloaded"));
    }
}
