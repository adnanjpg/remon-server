//! Unified host-event timeline — `GET /events`.
//!
//! Read-time union of the three event stores, normalized into one shape
//! (see `dtos::events`): the `host_events` ledger (boots, OOM kills, SMART
//! transitions, operator audit), `alert_events` (fire/resolve, projected as
//! kinds `alert_fired`/`alert_resolved`), and `incident_snapshots`
//! (projected as `incident_captured`, `ref` pointing at the bundle).
//! Built for chart annotation overlays and timeline feeds: same range
//! contract as the `/metrics/*` endpoints, sorted newest first.

use axum::{Json, extract::State};
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::routes::dtos::events::{
    EventActorDto, EventDto, EventRefDto, ListEventsQuery, ListEventsResponse,
};
use crate::routes::extractors::{Claims, ValidatedQuery};
use crate::state::AppState;
use crate::storage::repositories::{AlertRepository, HostEventRepository, IncidentRepository};

/// Default span when the client omits start/end: last 24 hours. Events are
/// sparse compared to metric samples; a day is the natural first screenful.
const DEFAULT_SPAN_SECS: i64 = 86_400;
const DEFAULT_LIMIT: u32 = 500;
const MAX_LIMIT: u32 = 1000;

/// Kinds the alert_events projection produces; used to decide whether a
/// `kinds=` filter still needs that table queried at all.
const ALERT_KINDS: [&str; 2] = ["alert_fired", "alert_resolved"];
const INCIDENT_KIND: &str = "incident_captured";

/// A row from one of the three stores, still in its own shape.
///
/// The merge only needs a timestamp, so projection waits until the page is
/// decided: three stores each return up to `limit`, and mapping all of them
/// formats messages and parses `details` JSON for rows that lose the sort.
enum TimelineRow {
    Ledger(crate::storage::repositories::HostEventRow),
    Alert(crate::storage::repositories::AlertEventWithRule),
    Incident(crate::storage::repositories::IncidentSummaryRow),
}

impl TimelineRow {
    fn ts(&self) -> i64 {
        match self {
            Self::Ledger(r) => r.created_at,
            Self::Alert(e) => e.occurred_at,
            Self::Incident(i) => i.created_at,
        }
    }

    fn into_dto(self) -> EventDto {
        match self {
            Self::Ledger(r) => EventDto {
                ts: r.created_at,
                source: r.source,
                kind: r.kind,
                severity: r.severity,
                message: r.message,
                actor: r.actor_device_id.map(|device_id| EventActorDto {
                    device_id,
                    name: r.actor_name,
                }),
                reference: r
                    .ref_type
                    .zip(r.ref_id)
                    .map(|(t, id)| EventRefDto { ref_type: t, id }),
                details: r.details.and_then(|d| serde_json::from_str(&d).ok()),
            },
            Self::Alert(e) => {
                let (kind, severity, verb) = match e.event_type.as_str() {
                    // The rule's own scale maps onto the event scale: a firing
                    // crit is an error-level moment on the timeline.
                    "fired" => (
                        "alert_fired",
                        if e.severity == "crit" {
                            "error"
                        } else {
                            "warn"
                        },
                        "fired",
                    ),
                    _ => ("alert_resolved", "info", "resolved"),
                };
                let labels = if e.label_set != "{}" {
                    format!(" {}", e.label_set)
                } else {
                    String::new()
                };
                EventDto {
                    ts: e.occurred_at,
                    source: "system".to_string(),
                    kind: kind.to_string(),
                    severity: severity.to_string(),
                    message: format!("Alert '{}'{} {}", e.rule_name, labels, verb),
                    actor: None,
                    reference: Some(EventRefDto {
                        ref_type: "alert_rule".to_string(),
                        id: e.rule_id.to_string(),
                    }),
                    details: Some(serde_json::json!({
                        "rule_severity": e.severity,
                        "label_set": e.label_set,
                        "metric_value": e.metric_value,
                    })),
                }
            }
            Self::Incident(i) => {
                let source = if i.trigger_kind == "alert" {
                    "system"
                } else {
                    "operator"
                };
                let subject = i
                    .rule_name
                    .as_deref()
                    .or(i.reason.as_deref())
                    .unwrap_or("host snapshot");
                EventDto {
                    ts: i.created_at,
                    source: source.to_string(),
                    kind: INCIDENT_KIND.to_string(),
                    severity: "info".to_string(),
                    message: format!("Incident captured: {}", subject),
                    actor: None,
                    reference: Some(EventRefDto {
                        ref_type: "incident".to_string(),
                        id: i.id.to_string(),
                    }),
                    details: Some(serde_json::json!({
                        "trigger": i.trigger_kind,
                        "category": i.category,
                        "has_after": i.has_after,
                    })),
                }
            }
        }
    }
}

/// GET /events?start&end&kinds&sources&limit
pub async fn list_events(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    ValidatedQuery(q): ValidatedQuery<ListEventsQuery>,
) -> AppResult<Json<ListEventsResponse>> {
    let now = chrono::Utc::now().timestamp();
    let end = q.end.unwrap_or(now);
    let start = q.start.unwrap_or(end - DEFAULT_SPAN_SECS);
    if end < start {
        return Err(AppError::BadRequest("end must be >= start".to_string()));
    }
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);

    // `kinds` is an open vocabulary (unknown kind = empty result, not an
    // error); `sources` is a closed enum, so a typo is caught here.
    let kinds: Option<Vec<String>> = q.kinds.as_deref().map(parse_csv);
    let sources: Option<Vec<String>> = q.sources.as_deref().map(parse_csv);
    if let Some(s) = &sources
        && let Some(bad) = s
            .iter()
            .find(|v| !matches!(v.as_str(), "system" | "operator" | "agent"))
    {
        return Err(AppError::BadRequest(format!(
            "unknown source '{}'; expected system|operator|agent",
            bad
        )));
    }

    let kind_wanted = |k: &str| kinds.as_ref().is_none_or(|ks| ks.iter().any(|x| x == k));
    let source_wanted = |s: &str| sources.as_ref().is_none_or(|ss| ss.iter().any(|x| x == s));

    // Collected as rows and mapped only after the merge. Each store is read up
    // to `limit`, so a full page discards two thirds of what was fetched —
    // building a DTO for each, parsing its `details` JSON and formatting its
    // message, before finding out it lost the sort.
    let mut rows: Vec<TimelineRow> = Vec::new();

    // Every store below applies `kinds`/`sources` in its own SQL. Filtering
    // the merged result instead would be wrong rather than merely slower: each
    // store returns at most `limit` rows, so a window in which the unwanted
    // kind alone exceeds the limit yields an empty page while matching events
    // sit just outside it.

    // ── host_events ledger ──
    let ledger = HostEventRepository::new(state.db.clone())
        .list_range(start, end, kinds.as_deref(), sources.as_deref(), limit)
        .await?;
    rows.extend(ledger.into_iter().map(TimelineRow::Ledger));

    // ── alert fire/resolve (source: system) ──
    if source_wanted("system") && ALERT_KINDS.iter().any(|k| kind_wanted(k)) {
        // Stored as `fired`/`resolved` and projected to `alert_fired`/
        // `alert_resolved`, so a `kinds` filter has to be translated back into
        // the stored vocabulary before it can be pushed down.
        let event_types: Option<Vec<String>> = kinds.as_ref().map(|_| {
            let mut wanted = Vec::new();
            if kind_wanted("alert_fired") {
                wanted.push("fired".to_string());
            }
            if kind_wanted("alert_resolved") {
                wanted.push("resolved".to_string());
            }
            wanted
        });
        let alert_rows = AlertRepository::new(state.db.clone())
            .events_in_range(start, end, event_types.as_deref(), limit)
            .await?;
        rows.extend(alert_rows.into_iter().map(TimelineRow::Alert));
    }

    // ── incident captures (source follows the trigger) ──
    if kind_wanted(INCIDENT_KIND) && (source_wanted("system") || source_wanted("operator")) {
        // Alert-triggered captures read as `system`, everything else as
        // `operator`, so a `sources` filter is a restriction on the trigger.
        // The guard above guarantees at least one of the two is wanted.
        let alert_triggered = if source_wanted("system") && source_wanted("operator") {
            None
        } else {
            Some(source_wanted("system"))
        };
        let incident_rows = IncidentRepository::new(state.db.clone())
            .list_range(start, end, alert_triggered, limit)
            .await?;
        rows.extend(incident_rows.into_iter().map(TimelineRow::Incident));
    }

    // Each store returned ≤ limit rows already sorted; the merged stream
    // re-sorts and re-caps so the newest `limit` across all stores win.
    rows.sort_by_key(|r| std::cmp::Reverse(r.ts()));
    rows.truncate(limit as usize);
    let events: Vec<EventDto> = rows.into_iter().map(TimelineRow::into_dto).collect();

    Ok(Json(ListEventsResponse {
        start,
        end,
        count: events.len(),
        events,
    }))
}

fn parse_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .collect()
}
