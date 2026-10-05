//! `propose_view`: the assistant answers "show me" questions with a dashboard
//! widget instead of prose. The model only picks a widget and its parameters;
//! the client renders it with its own widget catalog and fetches the data
//! itself, so no number in a view ever passes through the model.
//!
//! The emitted `config` is exactly remon-web's `WidgetConfig`
//! (src/lib/types/dashboard.ts) — see docs/assistant-views.md. Validation here
//! mirrors the web's `normalizeConfig` but is strict: a bad config comes back
//! to the model as a tool error naming the fix, instead of being dropped.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::ProposedView;
use crate::state::AppState;

/// Views per answer. A reply is a message, not a dashboard.
pub const MAX_VIEWS: usize = 6;
const MAX_TITLE_CHARS: usize = 80;

const RANGES: &[&str] = &["30m", "1h", "6h", "24h", "7d", "30d"];
const HISTORY_RESOURCES: &[&str] = &["cpu", "memory", "disk", "network"];
const LIVE_KPI_SOURCES: &[&str] = &["cpu", "memory", "disk-io", "network"];
const STATUS_SUMMARIES: &[&str] = &["host", "services", "containers", "alerts"];
const PROBE_VIZ: &[&str] = &["chart", "scalar"];
/// Kinds that take no parameters beyond `kind`.
const BARE_KINDS: &[&str] = &[
    "live-vitals",
    "memory-detail",
    "cpu-detail",
    "pressure",
    "network-detail",
    "disk-detail",
    "alert-timeline",
];

/// Every kind `propose_view` accepts, in the order the tool schema lists them.
pub fn kinds() -> Vec<&'static str> {
    let mut all = vec![
        "history-chart",
        "probe-metric",
        "live-kpi",
        "status-summary",
    ];
    all.extend_from_slice(BARE_KINDS);
    all
}

/// Tool definition, in the same OpenAI function shape as the rest.
pub fn definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "propose_view",
            "description": "Show the operator a live widget inline under your answer, \
    rendered by the app from its own data. Use it whenever the operator wants to see, \
    chart, watch or compare something, instead of describing the numbers in text. \
    Kinds and their fields:\n\
    - history-chart: resource (cpu|memory|disk|network), range (30m|1h|6h|24h|7d|30d) — \
    a host metric over a past window.\n\
    - probe-metric: probe, metric (names from list_probes), viz (chart|scalar, default \
    chart), optional labels (one label set of that metric, as an object).\n\
    - live-kpi: source (cpu|memory|disk-io|network) — one live number with a sparkline.\n\
    - status-summary: summary (host|services|containers|alerts).\n\
    - live-vitals, cpu-detail, memory-detail, pressure, network-detail, disk-detail, \
    alert-timeline: no other fields; live detail cards and the recent alert feed.\n\
    Call it once per widget. The app renders the data, so do not repeat its values in \
    your answer.",
            "parameters": {
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Short caption for the widget, e.g. 'CPU, last 6h'."
                    },
                    "config": {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string", "enum": kinds() },
                            "resource": { "type": "string", "enum": HISTORY_RESOURCES },
                            "range": { "type": "string", "enum": RANGES },
                            "source": { "type": "string", "enum": LIVE_KPI_SOURCES },
                            "summary": { "type": "string", "enum": STATUS_SUMMARIES },
                            "probe": { "type": "string" },
                            "metric": { "type": "string" },
                            "viz": { "type": "string", "enum": PROBE_VIZ },
                            "labels": {
                                "type": "object",
                                "description": "probe-metric only: the label set to plot."
                            }
                        },
                        "required": ["kind"]
                    }
                },
                "required": ["title", "config"]
            }
        }
    })
}

/// Validate one `propose_view` call and queue the view for the client.
pub async fn propose_view(
    state: &Arc<AppState>,
    args: &Value,
    views: &mut Vec<ProposedView>,
) -> Result<Value, String> {
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("missing 'title'")?;
    let title: String = title.chars().take(MAX_TITLE_CHARS).collect();
    let input = args
        .get("config")
        .and_then(Value::as_object)
        .ok_or("missing 'config' object")?;
    let config = normalize(state, input).await?;

    // The same widget twice adds nothing; hand back the one already shown.
    if let Some(existing) = views.iter().find(|v| v.config == config) {
        return Ok(shown(&existing.id, &existing.title));
    }
    if views.len() >= MAX_VIEWS {
        return Err(format!(
            "at most {MAX_VIEWS} views per answer; this one was not added"
        ));
    }
    let id = format!("v{}", views.len() + 1);
    let out = shown(&id, &title);
    views.push(ProposedView { id, title, config });
    Ok(out)
}

fn shown(id: &str, title: &str) -> Value {
    json!({
        "shown": title,
        "id": id,
        "status": "rendered under your answer from live data; refer to it, do not restate its numbers",
    })
}

/// Turn the model's config into the exact `WidgetConfig` the web renders.
async fn normalize(state: &Arc<AppState>, input: &Map<String, Value>) -> Result<Value, String> {
    let kind = input
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("config is missing 'kind'")?;
    match kind {
        "history-chart" => {
            only_fields(input, kind, &["resource", "range"])?;
            Ok(json!({
                "kind": kind,
                "resource": one_of(input, "resource", HISTORY_RESOURCES)?,
                "range": one_of(input, "range", RANGES)?,
            }))
        }
        "live-kpi" => {
            only_fields(input, kind, &["source"])?;
            Ok(json!({ "kind": kind, "source": one_of(input, "source", LIVE_KPI_SOURCES)? }))
        }
        "status-summary" => {
            only_fields(input, kind, &["summary"])?;
            Ok(json!({ "kind": kind, "summary": one_of(input, "summary", STATUS_SUMMARIES)? }))
        }
        "probe-metric" => {
            only_fields(input, kind, &["probe", "metric", "viz", "labels"])?;
            probe_metric(state, input).await
        }
        k if BARE_KINDS.contains(&k) => {
            only_fields(input, kind, &[])?;
            Ok(json!({ "kind": kind }))
        }
        other => Err(format!(
            "unknown view kind '{other}'. Valid kinds: {}",
            kinds().join(", ")
        )),
    }
}

/// Reject fields a kind does not take, so a model that thinks `range` applies
/// to a live card learns otherwise instead of showing the wrong thing.
fn only_fields(input: &Map<String, Value>, kind: &str, allowed: &[&str]) -> Result<(), String> {
    let extra: Vec<&str> = input
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "kind" && !allowed.contains(k))
        .collect();
    if extra.is_empty() {
        return Ok(());
    }
    let takes = if allowed.is_empty() {
        "no other fields".to_string()
    } else {
        allowed.join(", ")
    };
    Err(format!(
        "'{kind}' does not take {}; it takes {takes}",
        extra.join(", ")
    ))
}

fn one_of(input: &Map<String, Value>, field: &str, allowed: &[&str]) -> Result<String, String> {
    match input.get(field).and_then(Value::as_str) {
        Some(v) if allowed.contains(&v) => Ok(v.to_string()),
        Some(v) => Err(format!(
            "invalid {field} '{v}'. Valid: {}",
            allowed.join(", ")
        )),
        None => Err(format!("missing '{field}'. Valid: {}", allowed.join(", "))),
    }
}

/// Probe and metric must exist in the registry, and the probe must have
/// reported already: a widget for a series with no data is just an empty box.
/// `labels` becomes the web's `labelKey` (canonical sorted-key JSON, the same
/// encoding as `ProbeMetric::labels_canonical`); `unit` is filled from the
/// probe's own report.
async fn probe_metric(state: &Arc<AppState>, input: &Map<String, Value>) -> Result<Value, String> {
    let field = |name: &str| {
        input
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("probe-metric is missing '{name}' (see list_probes)"))
    };
    let probe = field("probe")?;
    let metric = field("metric")?;
    let viz = match input.get("viz") {
        None => "chart".to_string(),
        Some(_) => one_of(input, "viz", PROBE_VIZ)?,
    };
    let labels: Option<BTreeMap<String, String>> = match input.get("labels") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            serde_json::from_value(v.clone())
                .map_err(|_| "'labels' must be an object of string values".to_string())?,
        ),
    };

    let reg = state.probe_registry.read().await;
    let Some(entry) = reg.probes.get(probe) else {
        let mut known: Vec<&str> = reg.probes.keys().map(String::as_str).collect();
        known.sort_unstable();
        return Err(format!(
            "no probe named '{probe}'. Registered probes: {}",
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        ));
    };
    if entry.last_metrics.is_empty() {
        return Err(format!(
            "probe '{probe}' has not reported any metrics yet, so there is nothing to show"
        ));
    }
    let series: Vec<_> = entry
        .last_metrics
        .iter()
        .filter(|m| m.name == metric)
        .collect();
    if series.is_empty() {
        let mut names: Vec<&str> = entry.last_metrics.iter().map(|m| m.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        return Err(format!(
            "probe '{probe}' has no metric '{metric}'. Its metrics: {}",
            names.join(", ")
        ));
    }

    let picked = match &labels {
        None => series[0],
        Some(want) => *series.iter().find(|m| &m.labels == want).ok_or_else(|| {
            let sets: Vec<String> = series.iter().map(|m| m.labels_canonical()).collect();
            format!(
                "metric '{metric}' has no series with labels {}. Its label sets: {}",
                serde_json::to_string(want).unwrap_or_default(),
                sets.join(", ")
            )
        })?,
    };

    let mut config = json!({
        "kind": "probe-metric",
        "probe": probe,
        "metric": metric,
        "viz": viz,
    });
    if let Some(unit) = picked.unit.as_deref().filter(|u| !u.trim().is_empty()) {
        config["unit"] = json!(unit);
    }
    if labels.is_some() {
        config["labelKey"] = json!(picked.labels_canonical());
    }
    Ok(config)
}
