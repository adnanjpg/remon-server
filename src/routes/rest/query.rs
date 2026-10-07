//! `POST /query` — the one data endpoint a composed screen needs.
//!
//! A screen spec names its data as `namespace.field{labels}` queries, the same
//! metric references the alert engine and the assistant's `metric_history`
//! use. The renderer sends all of a screen's queries in one request and gets
//! one result per query: a failed query carries its own `error` and the rest
//! of the screen still draws.
//!
//! `series` mode reads history on the plan the resource charts use (tier
//! stitching, point budget, coverage), so a composed chart and a built-in one
//! never disagree. `latest` mode reads the current value through the alert
//! resolver and so covers every namespace a rule can watch, not only the
//! charted ones.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::routes::extractors::Claims;
use crate::screen::{MAX_QUERIES, RANGES};
use crate::services::alerting::expression::MetricRef;
use crate::services::alerting::resolver::resolve_with_state;
use crate::state::AppState;
use crate::storage::repositories::{ChartMetadata, read_field_series};

/// Series a keyed query returns when no label picks one, highest average first.
const DEFAULT_SERIES_LIMIT: usize = 10;
const MAX_SERIES_LIMIT: usize = 50;
const DEFAULT_MAX_POINTS: u32 = 300;

#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    /// A named window ending now. Mutually exclusive with `start`/`end`.
    #[serde(default)]
    pub range: Option<String>,
    /// Explicit window, unix seconds. `end` defaults to now.
    #[serde(default)]
    pub start: Option<i64>,
    #[serde(default)]
    pub end: Option<i64>,
    /// Points per series for `series` queries (16-2000, default 300).
    #[serde(default)]
    pub max_points: Option<u32>,
    pub queries: Vec<SeriesQuery>,
}

#[derive(Debug, Deserialize)]
pub struct SeriesQuery {
    /// Caller's handle for matching the result; unique within the request.
    pub id: String,
    pub namespace: String,
    pub field: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub mode: QueryMode,
    /// `series`/`summary` on a keyed namespace without a label: how many
    /// series to return, busiest over the window first.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryMode {
    /// History over the window.
    #[default]
    Series,
    /// The current value, through the alert resolver.
    Latest,
    /// One `stats` per series over the window instead of points.
    Summary,
}

#[derive(Debug, Serialize)]
pub struct QueryResponse {
    pub start: i64,
    pub end: i64,
    pub results: Vec<QueryResult>,
}

#[derive(Debug, Serialize)]
pub struct QueryResult {
    pub id: String,
    /// `percent`, `bytes`, `bytes/s`, `/s`, `celsius`; absent when the value
    /// is a plain count or ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<&'static str>,
    pub series: Vec<SeriesOut>,
    /// Plan behind a `series` result: bucket width, sources, gaps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chart: Option<ChartMetadata>,
    /// Set when this query failed; `series` is then empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SeriesOut {
    pub labels: BTreeMap<String, String>,
    /// `[timestamp, value]`, oldest first; `value` is null for an empty bucket.
    /// Empty in `summary` mode.
    pub points: Vec<(i64, Option<f64>)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<Stats>,
}

/// A series over the window, from the same buckets `series` draws.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Stats {
    /// Over every tick the namespace reported in, a missing key counting as
    /// zero: a process outside the top set or a stopped container used
    /// (next to) nothing. A short burst cannot outrank steady load this way.
    pub avg: f64,
    /// Lowest and highest bucket where the key was present. Bucket means, so
    /// a spike shorter than a bucket is flattened.
    pub min: f64,
    pub max: f64,
}

/// What a field measures, from the naming the collectors follow.
pub fn unit_of(namespace: &str, field: &str) -> Option<&'static str> {
    if namespace == "pressure" || field.ends_with("_percent") {
        Some("percent")
    } else if field.ends_with("_bytes_per_sec") || field.ends_with("_bps") {
        Some("bytes/s")
    } else if field.ends_with("_per_sec") || field.ends_with("_iops") {
        Some("/s")
    } else if field.ends_with("_bytes") {
        Some("bytes")
    } else if field.ends_with("_c") {
        Some("celsius")
    } else {
        None
    }
}

/// `range` or `start`/`end` → a concrete window; neither means the last hour.
pub fn window(
    range: Option<&str>,
    start: Option<i64>,
    end: Option<i64>,
    now: i64,
) -> Result<(i64, i64), String> {
    match (range, start, end) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            Err("use either `range` or `start`/`end`, not both".to_string())
        }
        (Some(r), None, None) => RANGES
            .iter()
            .find(|(name, _)| *name == r)
            .map(|(_, secs)| (now - secs, now))
            .ok_or_else(|| {
                let names: Vec<&str> = RANGES.iter().map(|(n, _)| *n).collect();
                format!("unknown range '{r}'; ranges: {}", names.join(", "))
            }),
        (None, start, end) => {
            let end = end.unwrap_or(now);
            let start = start.unwrap_or(end - 3600);
            if end <= start {
                return Err("end must be after start".to_string());
            }
            Ok((start, end))
        }
    }
}

/// POST /query
pub async fn query(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(req): Json<QueryRequest>,
) -> AppResult<Json<QueryResponse>> {
    if req.queries.is_empty() || req.queries.len() > MAX_QUERIES {
        return Err(AppError::BadRequest(format!(
            "send between 1 and {MAX_QUERIES} queries"
        )));
    }
    let mut seen = HashSet::new();
    for q in &req.queries {
        if q.id.trim().is_empty() || !seen.insert(q.id.as_str()) {
            return Err(AppError::BadRequest(format!(
                "query ids must be non-empty and unique; '{}' is not",
                q.id
            )));
        }
    }
    let now = chrono::Utc::now().timestamp();
    let (start, end) =
        window(req.range.as_deref(), req.start, req.end, now).map_err(AppError::BadRequest)?;
    let max_points = req.max_points.unwrap_or(DEFAULT_MAX_POINTS);

    let mut results = Vec::with_capacity(req.queries.len());
    for q in &req.queries {
        let outcome = match q.mode {
            QueryMode::Series => series(&state, q, start, end, max_points, false).await,
            QueryMode::Summary => series(&state, q, start, end, max_points, true).await,
            QueryMode::Latest => latest(&state, q, now).await.map(|s| (s, None)),
        };
        let unit = unit_of(&q.namespace, &q.field);
        results.push(match outcome {
            Ok((series, chart)) => QueryResult {
                id: q.id.clone(),
                unit,
                series,
                chart,
                error: None,
            },
            // A bad query is the caller's to fix and the screen's to show.
            // Anything else is ours, and fails the request as usual.
            Err(AppError::BadRequest(message)) => QueryResult {
                id: q.id.clone(),
                unit,
                series: Vec::new(),
                chart: None,
                error: Some(message),
            },
            Err(e) => return Err(e),
        });
    }
    Ok(Json(QueryResponse {
        start,
        end,
        results,
    }))
}

async fn series(
    state: &AppState,
    q: &SeriesQuery,
    start: i64,
    end: i64,
    max_points: u32,
    summarize: bool,
) -> AppResult<(Vec<SeriesOut>, Option<ChartMetadata>)> {
    let key_label = crate::storage::repositories::series_catalog()
        .into_iter()
        .find(|(ns, ..)| *ns == q.namespace)
        .and_then(|(_, _, key)| key);
    let mut key_filter = None;
    for (k, v) in &q.labels {
        match key_label {
            Some(label) if label == k => key_filter = Some(v.as_str()),
            Some(label) => {
                return Err(AppError::BadRequest(format!(
                    "namespace '{}' takes only the '{label}' label, got '{k}'",
                    q.namespace
                )));
            }
            None => {
                return Err(AppError::BadRequest(format!(
                    "namespace '{}' has no label dimensions",
                    q.namespace
                )));
            }
        }
    }

    // A summary reads every key even for one label: its average needs the
    // ticks the whole namespace reported in.
    let read_filter = if summarize { None } else { key_filter };
    let (rows, meta) = read_field_series(
        &state.db,
        &q.namespace,
        &q.field,
        read_filter,
        start,
        end,
        max_points,
    )
    .await?;
    // Collector ticks in the window. A bucket holds as many as its most
    // present key does: some key (the busiest process, a mount) is in every
    // tick, while a quiet one shows up in only a few.
    let mut per_bucket: HashMap<i64, i64> = HashMap::new();
    for s in &rows {
        for ((t, v), n) in s.points.iter().zip(&s.samples) {
            if v.is_some() {
                let seen = per_bucket.entry(*t).or_default();
                *seen = (*seen).max(*n);
            }
        }
    }
    let ticks = per_bucket.values().sum::<i64>().max(1) as f64;

    let mut out: Vec<SeriesOut> = rows
        .into_iter()
        .filter(|s| !summarize || key_filter.is_none() || s.key.as_deref() == key_filter)
        .map(|s| {
            let values: Vec<f64> = s.points.iter().filter_map(|(_, v)| *v).collect();
            // Each bucket mean stands for the samples the key was present in.
            let load: f64 = s
                .points
                .iter()
                .zip(&s.samples)
                .filter_map(|((_, v), n)| v.map(|v| v * *n as f64))
                .sum();
            let stats = (!values.is_empty()).then(|| Stats {
                avg: load / ticks,
                min: values.iter().copied().fold(f64::INFINITY, f64::min),
                max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            });
            let labels = match (key_label, s.key) {
                (Some(label), Some(key)) => BTreeMap::from([(label.to_string(), key)]),
                _ => BTreeMap::new(),
            };
            SeriesOut {
                labels,
                points: if summarize { Vec::new() } else { s.points },
                stats,
            }
        })
        .collect();
    if key_filter.is_none() && key_label.is_some() {
        // Busiest over the window: the same average a summary reports, so a
        // burst seen in a handful of buckets ranks below steady load.
        let load = |s: &SeriesOut| s.stats.map_or(f64::NEG_INFINITY, |st| st.avg);
        out.sort_by(|a, b| load(b).total_cmp(&load(a)));
        out.truncate(
            q.limit
                .unwrap_or(DEFAULT_SERIES_LIMIT)
                .clamp(1, MAX_SERIES_LIMIT),
        );
    }
    if !summarize {
        // Ranking only; a drawn series carries its points, not a summary.
        for s in &mut out {
            s.stats = None;
        }
    }
    Ok((out, Some(meta)))
}

async fn latest(state: &AppState, q: &SeriesQuery, now: i64) -> AppResult<Vec<SeriesOut>> {
    let metric = MetricRef {
        namespace: q.namespace.clone(),
        field: q.field.clone(),
        labels: q.labels.clone(),
    };
    let samples = resolve_with_state(state, &metric)
        .await
        .map_err(|e| AppError::BadRequest(e.message))?;
    Ok(samples
        .into_iter()
        .map(|s| SeriesOut {
            labels: serde_json::from_str(&s.label_set).unwrap_or_default(),
            points: vec![(now, s.value.is_finite().then_some(s.value))],
            stats: None,
        })
        .collect())
}
