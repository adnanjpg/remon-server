//! Alert engine REST endpoints.
//!
//! Surface:
//! - `GET    /alerts`              — list rules
//! - `GET    /alerts/{id}`         — single rule
//! - `POST   /alerts`              — create rule
//! - `PUT    /alerts/{id}`         — update rule (full body)
//! - `DELETE /alerts/{id}`         — delete rule
//! - `GET    /alerts/state`        — currently pending or firing
//! - `GET    /alerts/events`       — recent transitions, newest first
//! - `GET    /alerts/{id}/events`  — recent transitions for one rule
//! - `GET    /alerts/schema`       — namespace/metric/label catalogue

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::routes::dtos::alerts::{
    AlertEventDto, AlertRuleDto, AlertStateDto, AlertsSchemaResponse, ComparatorSchemaDto,
    CreateAlertRuleRequest, LabelSchemaDto, LabelSourceDto, ListAlertEventsResponse,
    ListAlertRulesResponse, ListAlertStateResponse, MetricSchemaDto, NamespaceSchemaDto,
    SilenceAlertRequest, UpdateAlertRuleRequest, state_dto_from,
};
use crate::routes::extractors::Claims;
use crate::services::alerting::{expression, resolver};
use crate::state::AppState;
use crate::storage::repositories::{AlertRepository, UpsertAlertRule};

const DEFAULT_EVENT_LIMIT: u32 = 100;
const MAX_EVENT_LIMIT: u32 = 1000;
/// Sanity cap on `?offset=` — at the default 90-day retention an
/// `alert_events` table holding 100 k rows would be unusual; capping at
/// that bounds the SQLite scan-past cost on pathological inputs.
const MAX_EVENT_OFFSET: u32 = 100_000;

const MIN_EVAL_INTERVAL: i64 = 3;
const MAX_EVAL_INTERVAL: i64 = 3600;
const MAX_FOR_DURATION: i64 = 86_400; // 24h
const MAX_COOLDOWN: i64 = 86_400;

/// Smallest meaningful silence window — anything shorter is almost
/// certainly a client bug. Server treats negatives and zero as 400.
const MIN_SILENCE_DURATION: i64 = 1;
/// 30-day ceiling. Guards against an accidentally-permanent silence;
/// operators who truly want "never alert" should `enabled=false` instead.
const MAX_SILENCE_DURATION: i64 = 30 * 86_400;

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// Validate a create/update body. A dry resolve catches bad metric refs
/// at write time instead of every eval tick.
async fn validate(
    state: &AppState,
    req_expression: &str,
    for_secs: i64,
    eval_secs: i64,
    cooldown_secs: i64,
) -> AppResult<()> {
    let expr = expression::parse(req_expression)
        .map_err(|e| AppError::BadRequest(format!("expression: {}", e)))?;
    if let Err(e) = resolver::resolve_with_state(state, &expr.metric).await {
        return Err(AppError::BadRequest(format!("expression: {}", e.message)));
    }
    if !(MIN_EVAL_INTERVAL..=MAX_EVAL_INTERVAL).contains(&eval_secs) {
        return Err(AppError::BadRequest(format!(
            "eval_interval_secs {} out of range [{}..{}]",
            eval_secs, MIN_EVAL_INTERVAL, MAX_EVAL_INTERVAL
        )));
    }
    if !(0..=MAX_FOR_DURATION).contains(&for_secs) {
        return Err(AppError::BadRequest(format!(
            "for_duration_secs {} out of range [0..{}]",
            for_secs, MAX_FOR_DURATION
        )));
    }
    if !(0..=MAX_COOLDOWN).contains(&cooldown_secs) {
        return Err(AppError::BadRequest(format!(
            "cooldown_secs {} out of range [0..{}]",
            cooldown_secs, MAX_COOLDOWN
        )));
    }
    Ok(())
}

// ===== Rule CRUD =====

pub async fn list_alerts(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ListAlertRulesResponse>> {
    let repo = AlertRepository::new(state.db.clone());
    let rules = repo.list().await?;
    Ok(Json(ListAlertRulesResponse {
        rules: rules.into_iter().map(AlertRuleDto::from).collect(),
    }))
}

pub async fn get_alert(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<Json<AlertRuleDto>> {
    let repo = AlertRepository::new(state.db.clone());
    let rule = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Alert rule {}", id)))?;
    Ok(Json(rule.into()))
}

pub async fn create_alert(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateAlertRuleRequest>,
) -> AppResult<(StatusCode, Json<AlertRuleDto>)> {
    validate(
        &state,
        &req.expression,
        req.for_duration_secs,
        req.eval_interval_secs,
        req.cooldown_secs,
    )
    .await?;

    let repo = AlertRepository::new(state.db.clone());
    let upsert = UpsertAlertRule {
        name: req.name,
        description: req.description,
        enabled: req.enabled,
        expression: req.expression,
        severity: req.severity,
        for_duration_secs: req.for_duration_secs,
        eval_interval_secs: req.eval_interval_secs,
        cooldown_secs: req.cooldown_secs,
        silenced_until: None,
    };
    let id = repo.insert(&upsert).await?;
    let stored = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::Internal("created rule not readable".into()))?;
    Ok((StatusCode::CREATED, Json(stored.into())))
}

pub async fn update_alert(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateAlertRuleRequest>,
) -> AppResult<Json<AlertRuleDto>> {
    let repo = AlertRepository::new(state.db.clone());
    let mut current = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Alert rule {}", id)))?;

    // Selective overlay — only fields set in the request body are
    // applied. `description: Some(None)` explicitly clears it.
    if let Some(v) = req.name {
        current.name = v;
    }
    if let Some(v) = req.description {
        current.description = v;
    }
    if let Some(v) = req.enabled {
        current.enabled = v;
    }
    if let Some(v) = req.expression {
        current.expression = v;
    }
    if let Some(v) = req.severity {
        current.severity = v;
    }
    if let Some(v) = req.for_duration_secs {
        current.for_duration_secs = v;
    }
    if let Some(v) = req.eval_interval_secs {
        current.eval_interval_secs = v;
    }
    if let Some(v) = req.cooldown_secs {
        current.cooldown_secs = v;
    }
    if let Some(v) = req.silenced_until {
        current.silenced_until = v;
    }

    validate(
        &state,
        &current.expression,
        current.for_duration_secs,
        current.eval_interval_secs,
        current.cooldown_secs,
    )
    .await?;

    let merged = UpsertAlertRule {
        name: current.name.clone(),
        description: current.description.clone(),
        enabled: current.enabled,
        expression: current.expression.clone(),
        severity: current.severity,
        for_duration_secs: current.for_duration_secs,
        eval_interval_secs: current.eval_interval_secs,
        cooldown_secs: current.cooldown_secs,
        silenced_until: current.silenced_until,
    };
    let updated = repo.update(id, &merged).await?;
    if !updated {
        return Err(AppError::NotFound(format!("Alert rule {}", id)));
    }
    let stored = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::Internal("updated rule not readable".into()))?;
    Ok(Json(stored.into()))
}

pub async fn delete_alert(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let repo = AlertRepository::new(state.db.clone());
    let removed = repo.delete(id).await?;
    if !removed {
        return Err(AppError::NotFound(format!("Alert rule {}", id)));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ===== Silence =====

/// Temporarily suppress Fired notifications for one rule. The evaluator
/// keeps running — state transitions and event history continue, and
/// Resolved notifications still go through.
pub async fn silence_alert(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(req): Json<SilenceAlertRequest>,
) -> AppResult<Json<AlertRuleDto>> {
    if !(MIN_SILENCE_DURATION..=MAX_SILENCE_DURATION).contains(&req.duration_secs) {
        return Err(AppError::BadRequest(format!(
            "duration_secs {} out of range [{}..{}]",
            req.duration_secs, MIN_SILENCE_DURATION, MAX_SILENCE_DURATION
        )));
    }
    let until = chrono::Utc::now().timestamp() + req.duration_secs;

    let repo = AlertRepository::new(state.db.clone());
    let updated = repo.set_silence(id, Some(until)).await?;
    if !updated {
        return Err(AppError::NotFound(format!("Alert rule {}", id)));
    }
    let stored = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::Internal("silenced rule not readable".into()))?;
    Ok(Json(stored.into()))
}

/// Lift any active silence on a rule. Idempotent — returns 204 even if
/// the rule wasn't silenced.
pub async fn unsilence_alert(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let repo = AlertRepository::new(state.db.clone());
    let updated = repo.set_silence(id, None).await?;
    if !updated {
        return Err(AppError::NotFound(format!("Alert rule {}", id)));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ===== Active state =====

pub async fn list_active_state(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ListAlertStateResponse>> {
    let repo = AlertRepository::new(state.db.clone());
    // Joined with alert_rules in SQL — one round-trip, no second query
    // to materialise every rule just to look up two columns.
    let rows = repo.list_active_state().await?;

    let dtos: Vec<AlertStateDto> = rows
        .into_iter()
        .map(|(s, name, severity)| state_dto_from(s, name, severity))
        .collect();

    Ok(Json(ListAlertStateResponse { states: dtos }))
}

// ===== Event log =====

pub async fn list_recent_events(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Query(q): Query<EventsQuery>,
) -> AppResult<Json<ListAlertEventsResponse>> {
    let limit = q.limit.unwrap_or(DEFAULT_EVENT_LIMIT).min(MAX_EVENT_LIMIT);
    let offset = q.offset.unwrap_or(0).min(MAX_EVENT_OFFSET);
    let repo = AlertRepository::new(state.db.clone());
    let events = repo.recent_events(limit, offset).await?;
    Ok(Json(ListAlertEventsResponse {
        events: events.into_iter().map(AlertEventDto::from).collect(),
    }))
}

pub async fn list_events_for_rule(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<EventsQuery>,
) -> AppResult<Json<ListAlertEventsResponse>> {
    let limit = q.limit.unwrap_or(DEFAULT_EVENT_LIMIT).min(MAX_EVENT_LIMIT);
    let offset = q.offset.unwrap_or(0).min(MAX_EVENT_OFFSET);
    let repo = AlertRepository::new(state.db.clone());
    let events = repo.events_for_rule(id, limit, offset).await?;
    Ok(Json(ListAlertEventsResponse {
        events: events.into_iter().map(AlertEventDto::from).collect(),
    }))
}

// ===== Schema =====

/// Schema for the web rule editor. Mirrors the resolver whitelists in
/// `services/alerting/resolver.rs` — keep both in sync.
pub async fn get_alerts_schema(_claims: Claims) -> Json<AlertsSchemaResponse> {
    Json(build_alerts_schema())
}

fn build_alerts_schema() -> AlertsSchemaResponse {
    AlertsSchemaResponse {
        namespaces: vec![
            NamespaceSchemaDto {
                name: "cpu",
                description: "CPU usage, load average, and kernel-event counters",
                dynamic_metrics: false,
                metrics: vec![
                    MetricSchemaDto {
                        name: "usage_percent",
                        unit: Some("%"),
                        description: Some("Overall CPU usage 0-100"),
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "load_1m",
                        unit: None,
                        description: Some("Load average over the last 1 minute"),
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "load_5m",
                        unit: None,
                        description: Some("Load average over the last 5 minutes"),
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "load_15m",
                        unit: None,
                        description: Some("Load average over the last 15 minutes"),
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "steal_percent",
                        unit: Some("%"),
                        description: Some("Linux only: hypervisor steal time"),
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "iowait_percent",
                        unit: Some("%"),
                        description: Some("Linux only: time idle waiting for I/O"),
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "guest_percent",
                        unit: Some("%"),
                        description: Some("Linux only: time running a guest VM"),
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "context_switches_per_sec",
                        unit: Some("/s"),
                        description: Some("Linux only: kernel-wide context switches"),
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "process_forks_per_sec",
                        unit: Some("/s"),
                        description: Some("Linux only: process forks per second"),
                        value_type: "int",
                    },
                ],
                labels: vec![],
            },
            NamespaceSchemaDto {
                name: "memory",
                description: "Memory pressure and paging counters",
                dynamic_metrics: false,
                metrics: vec![
                    MetricSchemaDto {
                        name: "used_bytes",
                        unit: Some("bytes"),
                        description: None,
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "available_bytes",
                        unit: Some("bytes"),
                        description: None,
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "cached_bytes",
                        unit: Some("bytes"),
                        description: Some("Linux: Cached+Buffers+SReclaimable"),
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "swap_used_bytes",
                        unit: Some("bytes"),
                        description: None,
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "page_faults_minor_per_sec",
                        unit: Some("/s"),
                        description: Some("Linux only"),
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "page_faults_major_per_sec",
                        unit: Some("/s"),
                        description: Some("Linux only — disk-backed faults"),
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "swap_in_pages_per_sec",
                        unit: Some("/s"),
                        description: Some("Linux only — active thrashing if >0"),
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "swap_out_pages_per_sec",
                        unit: Some("/s"),
                        description: Some("Linux only"),
                        value_type: "int",
                    },
                ],
                labels: vec![],
            },
            NamespaceSchemaDto {
                name: "disk",
                description: "Per-mount usage and I/O. Container-runtime overlay mounts are filtered server-side.",
                dynamic_metrics: false,
                metrics: vec![
                    MetricSchemaDto {
                        name: "total_bytes",
                        unit: Some("bytes"),
                        description: None,
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "used_bytes",
                        unit: Some("bytes"),
                        description: None,
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "available_bytes",
                        unit: Some("bytes"),
                        description: None,
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "used_percent",
                        unit: Some("%"),
                        description: Some("Computed: used_bytes/total_bytes*100"),
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "read_bytes_per_sec",
                        unit: Some("bytes/s"),
                        description: None,
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "write_bytes_per_sec",
                        unit: Some("bytes/s"),
                        description: None,
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "inode_used_percent",
                        unit: Some("%"),
                        description: Some("Linux only via statvfs"),
                        value_type: "float",
                    },
                ],
                labels: vec![LabelSchemaDto {
                    name: "mount_point",
                    required: false,
                    values: None,
                    source: Some(LabelSourceDto {
                        endpoint: "/system/info",
                        json_path: "hardware.disks[].mount_point",
                    }),
                }],
            },
            NamespaceSchemaDto {
                name: "network",
                description: "Per-NIC byte/packet rates. veth/docker/br/tap interfaces are filtered server-side.",
                dynamic_metrics: false,
                metrics: vec![
                    MetricSchemaDto {
                        name: "rx_bytes_per_sec",
                        unit: Some("bytes/s"),
                        description: None,
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "tx_bytes_per_sec",
                        unit: Some("bytes/s"),
                        description: None,
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "rx_packets_per_sec",
                        unit: Some("/s"),
                        description: None,
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "tx_packets_per_sec",
                        unit: Some("/s"),
                        description: None,
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "errors_in_per_sec",
                        unit: Some("/s"),
                        description: Some("Frames dropped on receive"),
                        value_type: "int",
                    },
                    MetricSchemaDto {
                        name: "errors_out_per_sec",
                        unit: Some("/s"),
                        description: Some("Frames dropped on transmit"),
                        value_type: "int",
                    },
                ],
                labels: vec![LabelSchemaDto {
                    name: "interface_name",
                    required: false,
                    values: None,
                    source: Some(LabelSourceDto {
                        endpoint: "/system/info",
                        json_path: "hardware.network_interfaces[].name",
                    }),
                }],
            },
            NamespaceSchemaDto {
                name: "pressure",
                description: "PSI (Linux 4.20+) saturation averages. Empty on non-Linux hosts.",
                dynamic_metrics: false,
                metrics: vec![
                    MetricSchemaDto {
                        name: "some_avg10",
                        unit: Some("%"),
                        description: Some("≥1 task stalled, 10s average"),
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "some_avg60",
                        unit: Some("%"),
                        description: Some("60s average"),
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "some_avg300",
                        unit: Some("%"),
                        description: Some("300s average"),
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "full_avg10",
                        unit: Some("%"),
                        description: Some("All tasks stalled, 10s — not emitted for cpu"),
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "full_avg60",
                        unit: Some("%"),
                        description: None,
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "full_avg300",
                        unit: Some("%"),
                        description: None,
                        value_type: "float",
                    },
                ],
                labels: vec![LabelSchemaDto {
                    name: "resource",
                    required: false,
                    values: Some(&["cpu", "memory", "io"]),
                    source: None,
                }],
            },
            NamespaceSchemaDto {
                name: "components",
                description: "Hardware sensor readings. Empty on hosts without exposed sensors.",
                dynamic_metrics: false,
                metrics: vec![
                    MetricSchemaDto {
                        name: "temperature_c",
                        unit: Some("°C"),
                        description: None,
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "max_c",
                        unit: Some("°C"),
                        description: Some("Highest seen since boot"),
                        value_type: "float",
                    },
                    MetricSchemaDto {
                        name: "critical_c",
                        unit: Some("°C"),
                        description: Some("Vendor-declared shutdown threshold"),
                        value_type: "float",
                    },
                ],
                labels: vec![LabelSchemaDto {
                    name: "label",
                    required: false,
                    values: None,
                    source: None,
                }],
            },
            NamespaceSchemaDto {
                name: "probe",
                description: "Metrics emitted by user-defined probe scripts. Metric name is whatever the script reported.",
                dynamic_metrics: true,
                metrics: vec![],
                labels: vec![LabelSchemaDto {
                    name: "probe_name",
                    required: false,
                    values: None,
                    source: Some(LabelSourceDto {
                        endpoint: "/probes",
                        json_path: "probes[].name",
                    }),
                }],
            },
            NamespaceSchemaDto {
                name: "service",
                description: "Live OS service state. Resolver returns 1 when Running, 0 otherwise; the actual state name (failed/stopped/...) is included in the notification body.",
                dynamic_metrics: false,
                metrics: vec![MetricSchemaDto {
                    name: "up",
                    unit: None,
                    description: Some("1 if Running, 0 otherwise"),
                    value_type: "bool",
                }],
                labels: vec![LabelSchemaDto {
                    name: "unit",
                    required: true,
                    values: None,
                    source: Some(LabelSourceDto {
                        endpoint: "/services",
                        json_path: "services[].name",
                    }),
                }],
            },
        ],
        comparators: vec![
            ComparatorSchemaDto {
                op: ">",
                display: "greater than",
            },
            ComparatorSchemaDto {
                op: ">=",
                display: "greater than or equal",
            },
            ComparatorSchemaDto {
                op: "<",
                display: "less than",
            },
            ComparatorSchemaDto {
                op: "<=",
                display: "less than or equal",
            },
            ComparatorSchemaDto {
                op: "==",
                display: "equals",
            },
            ComparatorSchemaDto {
                op: "!=",
                display: "not equal",
            },
        ],
    }
}
