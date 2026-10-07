//! `propose_screen`: the assistant answers "show me" with a composed screen
//! instead of prose. The spec goes through the same validator every screen
//! does (`crate::screen`); a failure comes back as a tool error listing what
//! does exist, so the model fixes it and calls again.

use serde_json::{Value, json};

use super::ProposedScreen;
use crate::screen::{self, RANGES, widget};
use crate::state::AppState;
use crate::storage::repositories::series_catalog;

/// Screens per answer. One is the norm; a few cover "and also show me...".
pub const MAX_SCREENS: usize = 3;

/// Tool definition. The chartable catalog is generated, so the model is never
/// told about a field `POST /query` would reject.
pub fn definition() -> Value {
    let catalog: Vec<String> = series_catalog()
        .into_iter()
        .map(|(ns, fields, key)| {
            let label = key.map(|k| format!(" (label: {k})")).unwrap_or_default();
            format!("  {ns}{label}: {}", fields.join(", "))
        })
        .collect();
    let ranges: Vec<&str> = RANGES.iter().map(|(n, _)| *n).collect();
    let description = format!(
        "Compose a screen from live data, drawn under your answer. Use it whenever the \
operator wants to see, chart, watch or compare something, instead of describing numbers. \
The app fetches the data itself, so write no figures in your answer.\n\
\n\
A screen is {{title, range, root}}; range is one of {ranges} (default 1h). root is a node; \
nodes by `type`:\n\
- grid {{columns 1-4, children: [node]}}: side by side, wrapping.\n\
- tabs {{tabs: [{{title, child: node}}]}}.\n\
- line {{title, series: [{{label?, query}}], range?}}: history, up to 8 series on one chart \
(e.g. two processes compared).\n\
- stat {{title, query}}: one current value; the query must match a single series.\n\
- table {{title, columns: [{{label, query, agg?}}], limit?, range?}}: a row per label set, \
ranked by the first column. agg is current (default), or avg|max|min over the window \
(table range, else the screen's), which needs a charted namespace. \"Busiest over the \
day\" is agg avg: it counts the time a key was absent as zero, so steady load outranks \
a short burst.\n\
- widget {{title?, config}}: a built-in card. config.kind: {kinds}. history-chart takes \
resource (cpu|memory|disk|network) and range; live-kpi takes source \
(cpu|memory|disk-io|network); status-summary takes summary \
(host|services|containers|alerts); probe-metric takes probe, metric, viz?, labels?; the \
rest take nothing.\n\
\n\
A query is {{namespace, field, labels?, limit?}}, the names query_metric uses. line \
queries must be one of these charted namespaces:\n{catalog}\n\
A keyed line query without its label shows the busiest `limit` keys (default 10). stat \
and table queries may use any namespace query_metric reads. If validation fails, the \
error names what exists; fix the spec and call again.",
        ranges = ranges.join("|"),
        kinds = widget::kinds().join(", "),
        catalog = catalog.join("\n"),
    );
    json!({
        "type": "function",
        "function": {
            "name": "propose_screen",
            "description": description,
            "parameters": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short screen title." },
                    "range": { "type": "string", "enum": ranges },
                    "root": {
                        "type": "object",
                        "description": "The top node (see the tool description); usually a grid."
                    }
                },
                "required": ["title", "root"]
            }
        }
    })
}

/// Validate one `propose_screen` call and queue the screen for the client.
pub async fn propose_screen(
    state: &AppState,
    args: &Value,
    screens: &mut Vec<ProposedScreen>,
) -> Result<Value, String> {
    let spec = screen::validate(state, args).await?;
    let title = spec["title"].as_str().unwrap_or_default().to_string();

    // The same screen twice adds nothing; hand back the one already shown.
    if let Some(existing) = screens.iter().find(|s| s.screen == spec) {
        return Ok(shown(&existing.id, &title));
    }
    if screens.len() >= MAX_SCREENS {
        return Err(format!(
            "at most {MAX_SCREENS} screens per answer; this one was not added"
        ));
    }
    let id = format!("s{}", screens.len() + 1);
    let out = shown(&id, &title);
    screens.push(ProposedScreen { id, screen: spec });
    Ok(out)
}

fn shown(id: &str, title: &str) -> Value {
    json!({
        "shown": title,
        "id": id,
        "status": "drawn under your answer from live data; point to it, write no figures",
    })
}
