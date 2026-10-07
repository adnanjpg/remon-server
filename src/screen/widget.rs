//! The `widget` node: one of remon-web's built-in cards (`WidgetConfig` in
//! src/lib/types/dashboard.ts), for the rich ones a few primitives cannot
//! rebuild — per-core CPU bars, the alert feed, live detail cards.
//!
//! Strict on purpose: a bad config comes back to the model naming the fix,
//! instead of the client dropping it.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::RANGES;
use crate::state::AppState;

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

/// Every widget kind, in the order the tool description lists them.
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

/// Turn a model-written config into the exact `WidgetConfig` the web renders.
pub async fn normalize(state: &AppState, input: &Value) -> Result<Value, String> {
    let input = input.as_object().ok_or("widget config must be an object")?;
    let kind = input
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("widget config is missing 'kind'")?;
    match kind {
        "history-chart" => {
            only_fields(input, kind, &["resource", "range"])?;
            let ranges: Vec<&str> = RANGES.iter().map(|(n, _)| *n).collect();
            Ok(json!({
                "kind": kind,
                "resource": one_of(input, "resource", HISTORY_RESOURCES)?,
                "range": one_of(input, "range", &ranges)?,
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
            "unknown widget kind '{other}'. Valid kinds: {}",
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
async fn probe_metric(state: &AppState, input: &Map<String, Value>) -> Result<Value, String> {
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
