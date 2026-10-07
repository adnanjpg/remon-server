//! Screen specs: a screen described as data, composed on demand and drawn by
//! the client's renderer. The assistant writes them, the command bar will,
//! and the dashboard is one. Contract: docs/screens.md.
//!
//! A spec names its data as `namespace.field{labels}` queries — the metric
//! references the alert engine and `POST /query` share — so the model picks
//! *what* to show and the numbers never pass through it.
//!
//! [`validate`] is the gate every spec passes: shape (serde, unknown fields
//! rejected), limits, and data — a query that would draw an empty panel is
//! an error naming what does exist, so a model can fix it in one turn.

pub mod widget;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::services::alerting::expression::MetricRef;
use crate::services::alerting::resolver::resolve_with_state;
use crate::state::AppState;
use crate::storage::repositories::{read_field_series, series_catalog};

/// Named windows, the same set the web's range picker offers.
pub const RANGES: &[(&str, i64)] = &[
    ("30m", 1800),
    ("1h", 3600),
    ("6h", 21_600),
    ("24h", 86_400),
    ("7d", 604_800),
    ("30d", 2_592_000),
];
/// Queries per screen; one `POST /query` carries them all.
pub const MAX_QUERIES: usize = 16;
const MAX_PANELS: usize = 24;
const MAX_DEPTH: usize = 4;
const MAX_TITLE_CHARS: usize = 80;
const MAX_COLUMNS: u8 = 4;
const MAX_CHILDREN: usize = 12;
const MAX_TABS: usize = 6;
const MAX_LINE_SERIES: usize = 8;
const MAX_TABLE_COLUMNS: usize = 6;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screen {
    pub title: String,
    /// Window every time-based panel uses unless it sets its own.
    #[serde(default = "default_range")]
    pub range: String,
    pub root: Node,
}

fn default_range() -> String {
    "1h".to_string()
}
fn one() -> u8 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    /// Children laid out left to right, wrapping after `columns`.
    Grid {
        #[serde(default = "one")]
        columns: u8,
        children: Vec<Node>,
    },
    Tabs {
        tabs: Vec<Tab>,
    },
    /// History of one or more series on shared axes.
    Line {
        title: String,
        series: Vec<SeriesSpec>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        range: Option<String>,
    },
    /// One current value.
    Stat {
        title: String,
        query: Query,
    },
    /// Current values, one row per label set, one column per query.
    Table {
        title: String,
        columns: Vec<Column>,
        /// Rows kept, ranked by the first column, highest first.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    /// A built-in card (`WidgetConfig`), see [`widget`].
    Widget {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        config: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tab {
    pub title: String,
    pub child: Node,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesSpec {
    /// Legend text; the client derives one from the query when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub query: Query,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub label: String,
    pub query: Query,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub namespace: String,
    pub field: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Keyed `line` series without a label: how many keys, busiest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl Query {
    fn metric(&self) -> MetricRef {
        MetricRef {
            namespace: self.namespace.clone(),
            field: self.field.clone(),
            labels: self.labels.clone(),
        }
    }
}

pub fn range_secs(name: &str) -> Option<i64> {
    RANGES.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

fn range_names() -> String {
    RANGES
        .iter()
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Validate a model- or user-written spec and return it canonical: defaults
/// filled, titles trimmed, widget configs normalized. Every problem found is
/// reported, one per line, each prefixed with where it is.
pub async fn validate(state: &AppState, input: &Value) -> Result<Value, String> {
    let mut screen: Screen =
        serde_json::from_value(input.clone()).map_err(|e| format!("invalid screen: {e}"))?;
    let mut ctx = Ctx::default();

    screen.title = ctx.title(&screen.title, "title");
    if range_secs(&screen.range).is_none() {
        ctx.err(
            "range",
            format!(
                "unknown range '{}'; ranges: {}",
                screen.range,
                range_names()
            ),
        );
    }
    let range = screen.range.clone();
    walk(
        state,
        &mut screen.root,
        "root".to_string(),
        1,
        &range,
        &mut ctx,
    )
    .await;

    if ctx.panels == 0 {
        ctx.err("root", "a screen needs at least one panel".to_string());
    }
    if ctx.panels > MAX_PANELS {
        ctx.err(
            "root",
            format!("{} panels; at most {MAX_PANELS}", ctx.panels),
        );
    }
    if ctx.queries > MAX_QUERIES {
        ctx.err(
            "root",
            format!("{} queries; at most {MAX_QUERIES} per screen", ctx.queries),
        );
    }
    if !ctx.errors.is_empty() {
        return Err(ctx.errors.join("\n"));
    }
    serde_json::to_value(&screen).map_err(|e| e.to_string())
}

#[derive(Default)]
struct Ctx {
    panels: usize,
    queries: usize,
    errors: Vec<String>,
}

impl Ctx {
    fn err(&mut self, path: &str, message: String) {
        self.errors.push(format!("{path}: {message}"));
    }

    fn title(&mut self, title: &str, path: &str) -> String {
        let t = title.trim();
        if t.is_empty() {
            self.err(path, "title must not be empty".to_string());
        }
        t.chars().take(MAX_TITLE_CHARS).collect()
    }
}

fn walk<'a>(
    state: &'a AppState,
    node: &'a mut Node,
    path: String,
    depth: usize,
    range: &'a str,
    ctx: &'a mut Ctx,
) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        if depth > MAX_DEPTH {
            ctx.err(&path, format!("nested deeper than {MAX_DEPTH} levels"));
            return;
        }
        match node {
            Node::Grid { columns, children } => {
                if !(1..=MAX_COLUMNS).contains(columns) {
                    ctx.err(&path, format!("columns must be 1-{MAX_COLUMNS}"));
                }
                if children.is_empty() || children.len() > MAX_CHILDREN {
                    ctx.err(&path, format!("a grid holds 1-{MAX_CHILDREN} children"));
                }
                for (i, child) in children.iter_mut().enumerate() {
                    let at = format!("{path}.children[{i}]");
                    walk(state, child, at, depth + 1, range, ctx).await;
                }
            }
            Node::Tabs { tabs } => {
                if tabs.is_empty() || tabs.len() > MAX_TABS {
                    ctx.err(&path, format!("tabs holds 1-{MAX_TABS} tabs"));
                }
                for (i, tab) in tabs.iter_mut().enumerate() {
                    let at = format!("{path}.tabs[{i}]");
                    tab.title = ctx.title(&tab.title, &at);
                    walk(
                        state,
                        &mut tab.child,
                        format!("{at}.child"),
                        depth + 1,
                        range,
                        ctx,
                    )
                    .await;
                }
            }
            Node::Line {
                title,
                series,
                range: own,
            } => {
                ctx.panels += 1;
                *title = ctx.title(title, &path);
                let window = match own.as_deref() {
                    Some(r) if range_secs(r).is_none() => {
                        ctx.err(
                            &path,
                            format!("unknown range '{r}'; ranges: {}", range_names()),
                        );
                        return;
                    }
                    Some(r) => r,
                    None => range,
                };
                if series.is_empty() || series.len() > MAX_LINE_SERIES {
                    ctx.err(&path, format!("a line holds 1-{MAX_LINE_SERIES} series"));
                }
                for (i, s) in series.iter().enumerate() {
                    ctx.queries += 1;
                    let at = format!("{path}.series[{i}]");
                    if let Err(e) = check_history(state, &s.query, window).await {
                        ctx.err(&at, e);
                    }
                }
            }
            Node::Stat { title, query } => {
                ctx.panels += 1;
                ctx.queries += 1;
                *title = ctx.title(title, &path);
                if let Err(e) = check_current(state, query, true).await {
                    ctx.err(&path, e);
                }
            }
            Node::Table {
                title,
                columns,
                limit,
            } => {
                ctx.panels += 1;
                *title = ctx.title(title, &path);
                if columns.is_empty() || columns.len() > MAX_TABLE_COLUMNS {
                    ctx.err(
                        &path,
                        format!("a table holds 1-{MAX_TABLE_COLUMNS} columns"),
                    );
                }
                if limit.is_some_and(|l| l == 0 || l > 50) {
                    ctx.err(&path, "limit must be 1-50".to_string());
                }
                for (i, c) in columns.iter_mut().enumerate() {
                    ctx.queries += 1;
                    let at = format!("{path}.columns[{i}]");
                    c.label = ctx.title(&c.label, &at);
                    if let Err(e) = check_current(state, &c.query, false).await {
                        ctx.err(&at, e);
                    }
                }
            }
            Node::Widget { title, config } => {
                ctx.panels += 1;
                if let Some(t) = title {
                    *t = ctx.title(t, &path);
                }
                match widget::normalize(state, config).await {
                    Ok(canonical) => *config = canonical,
                    Err(e) => ctx.err(&path, e),
                }
            }
        }
    })
}

/// A `line` series: chartable, and with data in the window it will draw.
async fn check_history(state: &AppState, q: &Query, range: &str) -> Result<(), String> {
    let catalog = series_catalog();
    let Some((_, fields, key)) = catalog.iter().find(|(ns, ..)| *ns == q.namespace) else {
        let names: Vec<&str> = catalog.iter().map(|(ns, ..)| *ns).collect();
        return Err(format!(
            "no history for namespace '{}'; charted namespaces: {}. Use a stat or table for a current value.",
            q.namespace,
            names.join(", ")
        ));
    };
    if !fields.contains(&q.field.as_str()) {
        return Err(format!(
            "unknown field '{}' for '{}'; fields: {}",
            q.field,
            q.namespace,
            fields.join(", ")
        ));
    }
    let mut key_filter = None;
    for (k, v) in &q.labels {
        match key {
            Some(label) if label == k => key_filter = Some(v.as_str()),
            Some(label) => {
                return Err(format!(
                    "'{}' takes only the '{label}' label, got '{k}'",
                    q.namespace
                ));
            }
            None => return Err(format!("'{}' has no labels", q.namespace)),
        }
    }
    if q.limit.is_some_and(|l| l == 0 || l > 50) {
        return Err("limit must be 1-50".to_string());
    }

    let now = chrono::Utc::now().timestamp();
    let start = now - range_secs(range).unwrap_or(3600);
    let has_data = |rows: &[crate::storage::repositories::FieldSeries]| {
        rows.iter()
            .any(|s| s.points.iter().any(|(_, v)| v.is_some()))
    };
    let (rows, _) = read_field_series(
        &state.db,
        &q.namespace,
        &q.field,
        key_filter,
        start,
        now,
        16,
    )
    .await
    .map_err(|e| e.to_string())?;
    if has_data(&rows) {
        return Ok(());
    }
    let (Some(label), Some(value)) = (key, key_filter) else {
        return Err(format!(
            "no {}.{} data in the last {range}",
            q.namespace, q.field
        ));
    };
    // Name what does exist, so the model can correct a guessed key.
    let (all, _) = read_field_series(&state.db, &q.namespace, &q.field, None, start, now, 16)
        .await
        .map_err(|e| e.to_string())?;
    let mut seen: Vec<String> = all
        .into_iter()
        .filter(|s| s.points.iter().any(|(_, v)| v.is_some()))
        .filter_map(|s| s.key)
        .collect();
    seen.sort_unstable();
    seen.truncate(30);
    Err(format!(
        "no {} with {label}='{value}' in the last {range}; seen: {}",
        q.namespace,
        if seen.is_empty() {
            "none".to_string()
        } else {
            seen.join(", ")
        }
    ))
}

/// A `stat` or `table` query: resolvable right now. A stat must land on one
/// series, so a keyed query without its label is an error listing the keys.
async fn check_current(state: &AppState, q: &Query, single: bool) -> Result<(), String> {
    if q.limit.is_some() {
        return Err("limit applies to line series only; a table takes its own limit".to_string());
    }
    let samples = resolve_with_state(state, &q.metric())
        .await
        .map_err(|e| e.message)?;
    if samples.is_empty() {
        return Err(format!("{} has no current value", q.metric()));
    }
    if single && samples.len() > 1 {
        let sets: Vec<&str> = samples
            .iter()
            .take(10)
            .map(|s| s.label_set.as_str())
            .collect();
        return Err(format!(
            "{} matches {} series; a stat shows one, pick it with labels: {}",
            q.metric(),
            samples.len(),
            sets.join(", ")
        ));
    }
    Ok(())
}
