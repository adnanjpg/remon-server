//! Read-only tools exposed to the assistant, in OpenAI function-calling shape.
//!
//! Each tool reuses the same repositories and services the REST handlers do,
//! so the model sees exactly what an operator would. Adding a tool is: append
//! a definition in [`definitions`], add a match arm in [`dispatch`], write the
//! `async fn` that returns a `Value`. Keep every tool a pure read.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use super::ProposedAction;
use crate::models::process::ProcessState;
use crate::platform::services::{ServiceFilter, ServiceState};
use crate::services::alerting::expression::{self, MetricRef};
use crate::services::alerting::resolver::{
    HISTORY_RESOLUTIONS, history_summary, resolve_with_state,
};
use crate::services::system as system_svc;
use crate::state::AppState;
use crate::storage::repositories::{AlertRepository, LogRepository};

/// Default look-back for `read_logs`, matching `GET /logs`. Widenable per call
/// up to [`LOG_RETENTION_SECS`], past which there is nothing left to read —
/// the `('logs', 'raw')` retention seed in the migration.
const LOG_LOOKBACK_SECS: i64 = 86_400;
const LOG_RETENTION_SECS: i64 = 2_592_000;

/// OpenAI-format `tools` array advertised to the model on every turn. Built at
/// call time so the Docker tool (compile-gated) and the Prometheus tool
/// (config-gated) appear only when actually available.
pub fn definitions(state: &AppState) -> Value {
    let mut tools = vec![
        json!({
            "type": "function",
            "function": {
                "name": "get_summary",
                "description": "One-call overview of this host: server name, OS, uptime, \
        latest CPU usage percent, memory used/total bytes, the fullest disk mount, and \
        pending/firing alert counts. Call this first for any 'how is my server' question.",
                "parameters": { "type": "object", "properties": {} }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_logs",
                "description": "Read this daemon's log lines, newest first. Use it to find \
        errors or warnings that explain a problem. Covers the last day by default; widen \
        `since_minutes` to look further back, up to the 30 days retained.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "level": {
                            "type": "string",
                            "enum": ["error", "warn", "info", "debug", "trace"],
                            "description": "Minimum severity to include; 'warn' also returns errors. Default 'warn'."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum lines to return (1-200). Default 50."
                        },
                        "since_minutes": {
                            "type": "integer",
                            "description": "How far back to look, in minutes. Default 1440 (a day), \
        maximum 43200 (30 days, the retention window). A narrow window is much cheaper: a selective \
        level over a wide one walks every row in it."
                        }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_processes",
                "description": "Top processes by resource use — the direct answer to \
        'what is eating CPU/memory'. Per process: pid, name, cmdline, exe (binary path), \
        cwd (working dir — tells same-named processes apart), parent_pid, user, state, \
        uptime_seconds, cpu/memory now, and (when sampling is on) `history` with avg/max cpu, \
        avg memory and disk read/write bytes-per-sec over up to the last 15 minutes — use it to \
        tell a momentary spike from a sustained problem before proposing kill/restart. On Linux, \
        `container` carries the docker container short-id when the process runs in one \
        (cross-check with list_containers).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "sort_by": {
                            "type": "string",
                            "enum": ["cpu", "memory"],
                            "description": "Rank by CPU (default) or memory."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "How many to return (1-50). Default 10."
                        },
                        "min_cpu_percent": {
                            "type": "number",
                            "description": "Only processes at or above this current CPU percent."
                        },
                        "min_memory_percent": {
                            "type": "number",
                            "description": "Only processes at or above this current memory percent."
                        },
                        "name_contains": {
                            "type": "string",
                            "description": "Only processes whose name contains this (case-insensitive)."
                        }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "active_alerts",
                "description": "Alert rules currently pending or firing, with rule name, \
        severity, lifecycle state, the offending label set, last observed value and since-when. \
        Empty means nothing is alarming right now.",
                "parameters": { "type": "object", "properties": {} }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "query_metric",
                "description": "Read the latest value(s) of a specific host metric. A \
        namespace keyed by a label returns one sample per key unless you pass that label to narrow \
        it. Namespaces and example fields:\n\
        - cpu: usage_percent, iowait_percent, steal_percent\n\
        - memory: used_bytes, available_bytes, swap_used_bytes (no percent here — use get_summary)\n\
        - disk (label mount_point): used_percent, used_bytes, available_bytes\n\
        - network (label interface_name): rx_bytes_per_sec, tx_bytes_per_sec\n\
        - pressure (label resource=cpu|memory|io): some_avg10, some_avg60, full_avg10\n\
        - components (label label): temperature_c, max_c, critical_c\n\
        - smart (label device): health_passed, temperature_c, percentage_used\n\
        - docker (label container_id = container name): cpu_percent, memory_percent, memory_used_bytes\n\
        - process (label name = process name, pids grouped): cpu_percent, memory_bytes, pid_count, \
        disk_read_bps, disk_write_bps — only the top consumers are recorded each minute, so a quiet \
        process may have no samples\n\
        - service (label name): up (1 = running, 0 = not)\n\
        - heartbeat (label slug): up\n\
        - probe (label probe_name = the probe's name): field is a metric_name the probe emits (see list_probes)\n\
        If a field is invalid the tool returns an error naming the namespace so you can retry.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "namespace": { "type": "string", "description": "e.g. cpu, memory, disk, network, pressure, components, smart, docker, process, service, heartbeat, probe." },
                        "field": { "type": "string", "description": "Metric field within the namespace, e.g. used_percent." },
                        "labels": { "type": "object", "description": "Optional label filter, e.g. {\"mount_point\": \"/\"}." }
                    },
                    "required": ["namespace", "field"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_services",
                "description": "Init-system services (systemd / OpenRC / Windows SCM) with \
        their state (running/failed/stopped/...), boot-enablement and description. Use it to check \
        whether a specific service is up.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Case-insensitive substring filter on the unit name." },
                        "state": {
                            "type": "string",
                            "enum": ["running", "stopped", "starting", "stopping", "paused", "failed", "reloading"],
                            "description": "Only return services in this state."
                        },
                        "limit": { "type": "integer", "description": "Max services to return (1-200). Default 50." }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "metric_history",
                "description": "Aggregate a metric over a past time window — the answer to \
        'is it climbing', 'was there a spike', 'what's normal'. Returns count, min, max, avg, the \
        current value and a trend (rising/falling/flat) per key. Same namespaces/fields as \
        query_metric, limited to the performance ones: cpu, memory, disk (mount_point), network \
        (interface_name), pressure (resource), docker (container_id), process (name — answers \
        'which process was eating cpu at 3am'; only per-minute top consumers are recorded). Pair \
        with active_alerts to place an incident in time.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "namespace": { "type": "string", "description": "cpu, memory, disk, network, pressure, docker or process." },
                        "field": { "type": "string", "description": "Metric field, e.g. usage_percent, used_percent." },
                        "labels": { "type": "object", "description": "Optional label filter, e.g. {\"mount_point\": \"/\"}." },
                        "window_secs": { "type": "integer", "description": "Look-back window in seconds. Default 3600 (1h)." },
                        "resolution": {
                            "type": "string",
                            "enum": ["raw", "1m", "5m", "1h"],
                            "description": "Rollup granularity. Omit to auto-pick from the window."
                        }
                    },
                    "required": ["namespace", "field"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "recent_alert_events",
                "description": "The alert fire/resolve timeline, newest first, with rule name, \
        severity, event type (fired/resolved), when it happened and the value at the time. Use it to \
        answer 'when did this start' and 'what fired recently'.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "limit": { "type": "integer", "description": "Max events (1-100). Default 20." }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_alert_rules",
                "description": "All configured alert rules with id, name, expression, severity, \
        whether enabled and whether currently silenced. Use it before proposing to silence or reference \
        an existing rule.",
                "parameters": { "type": "object", "properties": {} }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_incidents",
                "description": "Flight-recorder snapshots: whenever an alert first crossed its \
        threshold (or someone asked), the daemon froze the box's context. Returns id, time, \
        trigger, category, rule and value per snapshot, newest first. Follow up with \
        incident_detail for the bundle.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "limit": { "type": "integer", "description": "Max snapshots (1-50). Default 10." }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "incident_detail",
                "description": "One incident snapshot's full context bundle: host vitals at \
        capture time, top processes with their recent in-memory history (spike vs steady), the \
        daemon's recent errors, system-level errors (OOM kills etc., Linux), co-active alerts and \
        failed units — plus a follow-up sample from ~60s later. THE tool for 'what caused that \
        alert at 03:12'.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer", "description": "Snapshot id from list_incidents." }
                    },
                    "required": ["id"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "capture_incident",
                "description": "Freeze the current host context into a persistent incident \
        snapshot (same bundle as alert-triggered ones, follow-up sample included). Use it when \
        you notice something anomalous that no alert covers, so the moment stays diagnosable \
        later. Writes only to the monitoring database — it does not touch the host.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "reason": { "type": "string", "description": "Why this moment is worth recording." },
                        "category": {
                            "type": "string",
                            "enum": ["resource", "availability", "security", "custom"],
                            "description": "Default custom."
                        }
                    },
                    "required": ["reason"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_system_events",
                "description": "System-level error/warning events — journald on Linux (OOM \
        kills, segfaults, disk errors, service crashes), the System+Application event logs on \
        Windows. This is the OS's view; read_logs is the daemon's own log and read_service_logs \
        is one unit's journal.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "level": { "type": "string", "enum": ["err", "warn"], "description": "Minimum severity. Default err." },
                        "lines": { "type": "integer", "description": "Max events (10-200). Default 50." },
                        "since_minutes": { "type": "integer", "description": "Look-back window. Default 60." }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "propose_alert_rule",
                "description": "Draft a new alert rule for the operator to confirm (it is NOT \
        created until they do). The expression is 'metric.field [labels] <op> number', e.g. \
        'cpu.usage_percent > 90' or 'disk.used_percent{mount_point=\"/\"} >= 95'; ops are > >= < <= == !=. \
        Same namespaces/fields as query_metric.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Short rule name, e.g. 'high-cpu'." },
                        "expression": { "type": "string", "description": "e.g. 'cpu.usage_percent > 90'." },
                        "severity": { "type": "string", "enum": ["warn", "crit"], "description": "Default warn." },
                        "for_secs": { "type": "integer", "description": "How long the condition must hold before firing. Default 60." },
                        "description": { "type": "string", "description": "Optional human note." }
                    },
                    "required": ["name", "expression"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "propose_silence_alert",
                "description": "Draft silencing an existing alert rule (by name) for a while, for \
        the operator to confirm. Silencing suppresses fired notifications; it does not disable evaluation.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Name of the rule to silence (see list_alert_rules)." },
                        "minutes": { "type": "integer", "description": "Silence duration in minutes. Default 60." }
                    },
                    "required": ["name"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "propose_service_action",
                "description": "Draft starting, stopping or restarting an init-system service, for \
        the operator to confirm. Propose the least drastic action that fixes the problem.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Unit name, e.g. 'nginx.service' (see list_services)." },
                        "action": { "type": "string", "enum": ["start", "stop", "restart"] }
                    },
                    "required": ["name", "action"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "propose_kill_process",
                "description": "Draft sending a signal to a process, for the operator to confirm. \
        Prefer SIGTERM (15); use SIGKILL (9) only when asked or when a process is unresponsive.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pid": { "type": "integer", "description": "Process id (see list_processes)." },
                        "signal": { "type": "integer", "enum": [15, 9], "description": "15=SIGTERM (default), 9=SIGKILL." },
                        "name": { "type": "string", "description": "Optional process name, for a clearer confirm message." }
                    },
                    "required": ["pid"]
                }
            }
        }),
    ];

    if !state.assistant_config.prometheus_url.trim().is_empty() {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "prometheus_query",
                "description": "Run a PromQL query against the configured Prometheus server \
for metrics beyond this daemon's own (node_exporter, cAdvisor, application metrics). Use an \
instant query for a current value, or a range query (set range_secs) to see a trend. \
Read-only.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "PromQL expression, e.g. rate(node_cpu_seconds_total[5m])." },
                        "range_secs": { "type": "integer", "description": "If set, run a range query over the last N seconds instead of an instant query." },
                        "step_secs": { "type": "integer", "description": "Range query resolution in seconds. Default 60." }
                    },
                    "required": ["query"]
                }
            }
        }));
    }

    tools.push(json!({
        "type": "function",
        "function": {
            "name": "list_probes",
            "description": "Custom probes registered on this host — user-defined scripts the daemon runs on a schedule (e.g. ClickHouse storage, DEBE freshness, entries-gap, fail2ban). Per probe: name, description, enabled, schedule, last run time / status / message, and its latest emitted metrics (name, value, unit, labels). Use this when asked about a probe by name, about scraper/pipeline/domain-specific health the built-in metrics don't cover, or \"what probes are configured\". For a probe metric's trend over time, follow up with metric_history (namespace 'probe').",
            "parameters": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Optional: only this probe (exact name). Omit to list all." }
                }
            }
        }
    }));

    if cfg!(target_os = "linux") {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "read_service_logs",
                "description": "Tail a systemd unit's journal (one-shot journalctl) — the real \
logs of any service on this host (nginx, a scraper, a database...), for diagnosing why a unit \
is failing or what it did recently. Use list_services first if unsure of the unit name.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "service": {
                            "type": "string",
                            "description": "Unit name, with or without the .service suffix (see list_services)."
                        },
                        "lines": {
                            "type": "integer",
                            "description": "How many recent lines (10-200). Default 50."
                        },
                        "since_minutes": {
                            "type": "integer",
                            "description": "Only entries from the last N minutes (optional)."
                        }
                    },
                    "required": ["service"]
                }
            }
        }));
    }

    #[cfg(feature = "docker")]
    {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "list_containers",
                "description": "Docker/Podman containers with name, image, state (running/exited/...) \
and status line. Use it to see which containers are up or down; per-container resource use is \
available via query_metric namespace 'docker'.",
                "parameters": { "type": "object", "properties": {} }
            }
        }));
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "read_container_logs",
                "description": "Tail a container's stdout/stderr logs — the real logs from the \
workload, for diagnosing a crash or error inside a container (not this daemon's own logs).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "container": { "type": "string", "description": "Container id or name (see list_containers)." },
                        "tail": { "type": "integer", "description": "How many trailing lines (1-500). Default 100." }
                    },
                    "required": ["container"]
                }
            }
        }));
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "propose_container_action",
                "description": "Draft starting, stopping or restarting a container, for the \
operator to confirm.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "container": { "type": "string", "description": "Container id or name (see list_containers)." },
                        "action": { "type": "string", "enum": ["start", "stop", "restart"] }
                    },
                    "required": ["container", "action"]
                }
            }
        }));
    }

    Value::Array(tools)
}

/// Execute one tool call and return its result as a string for the `tool`
/// message. Never panics and never propagates an error to the caller: a failed
/// tool comes back as a JSON `{"error": ...}` string so the model can read the
/// failure and adjust, exactly as it would a normal result.
pub async fn dispatch_collecting(
    state: &Arc<AppState>,
    name: &str,
    args: &Value,
    proposals: &mut Vec<ProposedAction>,
) -> String {
    let outcome = match name {
        // ----- read-only -----
        "get_summary" => get_summary(state).await,
        "read_logs" => read_logs(state, args).await,
        "list_processes" => list_processes(state, args).await,
        "active_alerts" => active_alerts(state).await,
        "list_alert_rules" => list_alert_rules(state).await,
        "query_metric" => query_metric(state, args).await,
        "metric_history" => metric_history(state, args).await,
        "recent_alert_events" => recent_alert_events(state, args).await,
        "list_services" => list_services(state, args).await,
        "prometheus_query" => prometheus_query(state, args).await,
        "list_incidents" => list_incidents(state, args).await,
        "incident_detail" => incident_detail(state, args).await,
        "read_system_events" => read_system_events(args).await,
        // Writes observability data into remon's own DB only — the host
        // itself stays untouched, so no propose/confirm round trip.
        "capture_incident" => capture_incident(state, args).await,
        #[cfg(feature = "docker")]
        "list_containers" => list_containers().await,
        #[cfg(feature = "docker")]
        "read_container_logs" => read_container_logs(args).await,
        "read_service_logs" => read_service_logs(args).await,
        "list_probes" => list_probes(state, args).await,
        // ----- propose-only (never mutate; drafted for operator confirm) -----
        "propose_alert_rule" => propose_alert_rule(args, proposals),
        "propose_silence_alert" => propose_silence_alert(state, args, proposals).await,
        "propose_service_action" => propose_service_action(args, proposals),
        "propose_kill_process" => propose_kill_process(args, proposals),
        #[cfg(feature = "docker")]
        "propose_container_action" => propose_container_action(args, proposals),
        other => Err(format!("unknown tool '{other}'")),
    };
    match outcome {
        Ok(value) => value.to_string(),
        Err(err) => json!({ "error": err }).to_string(),
    }
}

/// Read-only convenience: run a tool and discard any proposal. Used by tests
/// that only exercise read tools; the agent loop uses [`dispatch_collecting`].
#[cfg(test)]
pub async fn dispatch(state: &Arc<AppState>, name: &str, args: &Value) -> String {
    dispatch_collecting(state, name, args, &mut Vec::new()).await
}

/// Mirrors `GET /summary` (routes/rest/system.rs): the fullest mount wins the
/// disk slot; removable/pseudo mounts are already filtered by the collector.
async fn get_summary(state: &Arc<AppState>) -> Result<Value, String> {
    let desc = system_svc::get_description();
    let server_name = state.effective_config.read().await.server_name.clone();
    let (alerts_pending, alerts_firing) = AlertRepository::new(state.db.clone())
        .count_active_state()
        .await
        .map_err(|e| e.to_string())?;

    let stats = state.stats_latest.read().await.clone();
    let (cpu, mem_used, mem_total, fullest_disk) = match &stats {
        Some(s) => {
            let disk = s
                .disks
                .iter()
                .filter(|d| d.total_bytes > 0)
                .map(|d| {
                    (
                        d.used_bytes as f64 / d.total_bytes as f64 * 100.0,
                        d.mount_point.clone(),
                    )
                })
                .max_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(pct, mount)| json!({ "mount_point": mount, "used_percent": pct }));
            (
                Some(s.cpu.usage_percent),
                Some(s.memory.used_bytes),
                Some(s.memory.total_bytes),
                disk,
            )
        }
        None => (None, None, None, None),
    };

    Ok(json!({
        "server_name": server_name,
        "hostname": desc.hostname,
        "os": desc.os,
        "os_version": desc.os_version,
        "uptime_secs": desc.uptime_secs,
        "cpu_usage_percent": cpu,
        "memory_used_bytes": mem_used,
        "memory_total_bytes": mem_total,
        "fullest_disk": fullest_disk,
        "alerts_pending": alerts_pending,
        "alerts_firing": alerts_firing,
    }))
}

/// Mirrors `GET /logs` (routes/rest/logs.rs): `level <= max_level` with the
/// same 1=error .. 5=trace ordering the logging layer persists.
async fn read_logs(state: &Arc<AppState>, args: &Value) -> Result<Value, String> {
    let level = args.get("level").and_then(Value::as_str).unwrap_or("warn");
    let max_level = level_to_int(level)?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as u32;

    // Bounded like `GET /logs`, and widenable: an unbounded read walks every
    // row in the retained window to fill one page, but past incidents are a
    // real question, so the caller can ask for more.
    let since_secs = args
        .get("since_minutes")
        .and_then(Value::as_u64)
        .map(|m| (m as i64).saturating_mul(60))
        .unwrap_or(LOG_LOOKBACK_SECS)
        .clamp(60, LOG_RETENTION_SECS);

    let now = chrono::Utc::now().timestamp();
    let rows = LogRepository::new(state.db.clone())
        .list(max_level, now - since_secs, now, limit)
        .await
        .map_err(|e| e.to_string())?;

    let logs: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "timestamp": r.timestamp,
                "level": int_to_level(r.level),
                "target": r.target,
                "message": r.message,
            })
        })
        .collect();

    Ok(json!({ "count": logs.len(), "logs": logs }))
}

/// Top processes by CPU or memory. Reuses the same cached/serialized snapshot
/// path as `GET /processes`, so it never pays a second sysinfo scan when the
/// cache is warm.
async fn list_processes(state: &Arc<AppState>, args: &Value) -> Result<Value, String> {
    let sort_by = args.get("sort_by").and_then(Value::as_str).unwrap_or("cpu");
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .clamp(1, 50) as usize;
    let min_cpu = args.get("min_cpu_percent").and_then(Value::as_f64);
    let min_mem = args.get("min_memory_percent").and_then(Value::as_f64);
    let name_needle = args
        .get("name_contains")
        .and_then(Value::as_str)
        .map(str::to_lowercase);

    let list = crate::routes::rest::process::get_or_refresh_processes(state).await;
    let mut procs: Vec<_> = list
        .processes
        .iter()
        .filter(|p| {
            min_cpu.is_none_or(|t| p.cpu_percent >= t)
                && min_mem.is_none_or(|t| p.memory_percent >= t)
                && name_needle
                    .as_deref()
                    .is_none_or(|n| p.name.to_lowercase().contains(n))
        })
        .collect();
    let matched = procs.len();
    match sort_by {
        "memory" => procs.sort_by_key(|p| std::cmp::Reverse(p.memory_bytes)),
        _ => procs.sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent)),
    }

    let now = chrono::Utc::now().timestamp();
    let history = state.process_history.read().await;
    let top: Vec<Value> = procs
        .into_iter()
        .take(limit)
        .map(|p| {
            let mut row = json!({
                "pid": p.pid,
                "name": p.name,
                "cmdline": clipped_cmdline(&p.cmd),
                "exe": p.exe,
                "cwd": p.cwd,
                "parent_pid": p.parent_pid,
                "cpu_percent": p.cpu_percent,
                "memory_bytes": p.memory_bytes,
                "memory_percent": p.memory_percent,
                "user": p.user,
                "state": process_state_str(&p.state),
                "uptime_seconds": p.started_at.map(|t| (now - t).max(0)),
                "threads": p.threads,
            });
            // Sampled short history: lets the model separate "spiking right
            // now" from "hot for the last N minutes". Present only when the
            // process collector runs and has ≥2 samples for this pid.
            if let Some(h) = history.get(&p.pid).filter(|h| h.samples.len() >= 2) {
                let n = h.samples.len() as f64;
                let (mut cpu_sum, mut cpu_max, mut mem_sum) = (0.0f64, 0.0f64, 0.0f64);
                let (mut rd_sum, mut wr_sum) = (0.0f64, 0.0f64);
                for s in &h.samples {
                    cpu_sum += s.cpu_percent as f64;
                    cpu_max = cpu_max.max(s.cpu_percent as f64);
                    mem_sum += s.memory_bytes as f64;
                    rd_sum += s.disk_read_bps as f64;
                    wr_sum += s.disk_write_bps as f64;
                }
                let span = h
                    .samples
                    .back()
                    .zip(h.samples.front())
                    .map(|(b, f)| b.ts - f.ts)
                    .unwrap_or(0);
                row["history"] = json!({
                    "window_seconds": span,
                    "cpu_avg_percent": round1(cpu_sum / n),
                    "cpu_max_percent": round1(cpu_max),
                    "memory_avg_bytes": (mem_sum / n) as u64,
                    "disk_read_bps_avg": (rd_sum / n) as u64,
                    "disk_write_bps_avg": (wr_sum / n) as u64,
                });
            }
            if let Some(c) = container_of(p.pid) {
                row["container"] = json!(c);
            }
            row
        })
        .collect();

    Ok(json!({
        "total": list.total_count,
        "matched": matched,
        "sorted_by": if sort_by == "memory" { "memory" } else { "cpu" },
        "processes": top,
    }))
}

/// Command line, joined and clipped: enough to tell two `python3`s apart
/// without letting a pathological argv blow up the model context.
fn clipped_cmdline(cmd: &[String]) -> String {
    let joined = cmd.join(" ");
    if joined.chars().count() <= 160 {
        return joined;
    }
    let clipped: String = joined.chars().take(160).collect();
    format!("{clipped}…")
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Docker container short-id for a pid, read from its cgroup (Linux only) —
/// best effort, `None` for host processes or on any read/parse failure.
/// Covers both cgroup v2 systemd scopes (`…/docker-<id>.scope`) and the
/// legacy `/docker/<id>` layout.
#[cfg(target_os = "linux")]
fn container_of(pid: u32) -> Option<String> {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    for line in cgroup.lines() {
        let id = line
            .rsplit_once("docker-")
            .map(|(_, rest)| rest.trim_end_matches(".scope"))
            .or_else(|| line.rsplit_once("/docker/").map(|(_, rest)| rest));
        if let Some(id) = id
            && id.len() >= 12
            && id.chars().take(12).all(|c| c.is_ascii_hexdigit())
        {
            return Some(id.chars().take(12).collect());
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn container_of(_pid: u32) -> Option<String> {
    None
}

/// Custom probes from the in-memory registry: definition, last-run meta and
/// latest emitted metrics — the same view `GET /probes` serves. Optional
/// `name` narrows to one probe; an unknown name is an error naming what does
/// exist so the model can retry.
async fn list_probes(state: &Arc<AppState>, args: &Value) -> Result<Value, String> {
    let filter = args.get("name").and_then(Value::as_str);
    let reg = state.probe_registry.read().await;

    if let Some(name) = filter
        && !reg.probes.contains_key(name)
    {
        let mut known: Vec<&str> = reg.probes.keys().map(String::as_str).collect();
        known.sort_unstable();
        return Err(format!(
            "no probe named '{name}'. Registered probes: {}",
            known.join(", ")
        ));
    }

    let mut probes: Vec<Value> = reg
        .probes
        .values()
        .filter(|e| filter.is_none_or(|n| e.manifest.name == n))
        .map(|e| {
            let metrics: Vec<Value> = e
                .last_metrics
                .iter()
                .map(|m| {
                    json!({
                        "name": m.name,
                        "value": m.value,
                        "unit": m.unit,
                        "labels": m.labels,
                    })
                })
                .collect();
            json!({
                "name": e.manifest.name,
                "description": e.manifest.description,
                "enabled": e.manifest.enabled,
                "schedule": e.manifest.schedule.as_db_string(),
                "last_run_at": e.last_run.as_ref().map(|r| r.timestamp),
                "last_run_ok": e.last_run.as_ref().map(|r| r.parse_ok),
                "last_message": e.last_run.as_ref().and_then(|r| r.message.clone()),
                "latest_metrics": metrics,
            })
        })
        .collect();
    probes.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });

    Ok(json!({ "count": probes.len(), "probes": probes }))
}

/// Newest-first incident snapshots, summaries only.
async fn list_incidents(state: &Arc<AppState>, args: &Value) -> Result<Value, String> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .clamp(1, 50) as u32;
    let rows = crate::storage::repositories::IncidentRepository::new(state.db.clone())
        .list(limit)
        .await
        .map_err(|e| e.to_string())?;
    let out: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.id,
                "captured_at": r.created_at,
                "trigger": r.trigger_kind,
                "category": r.category,
                "rule_name": r.rule_name,
                "label_set": r.label_set,
                "metric_value": r.metric_value,
                "reason": r.reason,
                "has_after": r.has_after,
            })
        })
        .collect();
    Ok(json!({ "count": out.len(), "incidents": out }))
}

/// One snapshot's full bundle (+ the T+60s follow-up when present). Bundles
/// are bounded at capture time, so returning them whole is safe.
async fn incident_detail(state: &Arc<AppState>, args: &Value) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(Value::as_i64)
        .ok_or("missing 'id'")?;
    let row = crate::storage::repositories::IncidentRepository::new(state.db.clone())
        .get(id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no incident snapshot with id {id}"))?;

    let bundle: Value = serde_json::from_str(&row.bundle).unwrap_or(Value::Null);
    let after: Value = row
        .after_bundle
        .as_deref()
        .map(|s| serde_json::from_str(s).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);
    Ok(json!({
        "id": row.id,
        "captured_at": row.created_at,
        "trigger": row.trigger_kind,
        "category": row.category,
        "rule_name": row.rule_name,
        "label_set": row.label_set,
        "metric_value": row.metric_value,
        "reason": row.reason,
        "bundle": bundle,
        "after": after,
    }))
}

/// On-demand flight-recorder capture. Deliberately NOT a propose_* tool: it
/// only appends observability data to remon's own DB.
async fn capture_incident(state: &Arc<AppState>, args: &Value) -> Result<Value, String> {
    let reason = args
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .ok_or("missing 'reason'")?;
    let category = args
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or("custom");
    if !matches!(
        category,
        "resource" | "availability" | "security" | "custom"
    ) {
        return Err("category must be resource|availability|security|custom".to_string());
    }
    let id = crate::services::incidents::capture_manual(state, reason, category)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "captured": true,
        "id": id,
        "note": "context bundle stored; a follow-up sample lands in ~60s",
    }))
}

/// OS-level error/warning events (journald / Windows event log), one bounded
/// one-shot read. Implementation shared with the incident bundle builder.
async fn read_system_events(args: &Value) -> Result<Value, String> {
    let level = args.get("level").and_then(Value::as_str).unwrap_or("err");
    let lines = args.get("lines").and_then(Value::as_u64).unwrap_or(50);
    let since_minutes = args.get("since_minutes").and_then(Value::as_u64);
    crate::services::incidents::system_events(level, lines, since_minutes).await
}

/// One-shot `journalctl -u <unit>` tail for the assistant — the read-only
/// sibling of the SSE follow stream in `routes::sse::services`. Same charset
/// validation and unit normalization; bounded lines, optional look-back
/// window, hard timeout, and a total-size clamp so a chatty unit can't blow
/// up the model context.
async fn read_service_logs(args: &Value) -> Result<Value, String> {
    let Some(service) = args.get("service").and_then(Value::as_str) else {
        return Err("missing required arg: service".to_string());
    };
    if service.is_empty()
        || service.len() > 256
        || !service
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | ':'))
    {
        return Err("service name may only contain alphanumerics and `._-@:`".to_string());
    }
    let lines = args
        .get("lines")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(10, 200)
        .to_string();
    let since_minutes = args
        .get("since_minutes")
        .and_then(Value::as_u64)
        .filter(|m| *m > 0);

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (lines, since_minutes);
        Err("service logs are only available on Linux hosts with journald".to_string())
    }

    #[cfg(target_os = "linux")]
    {
        use crate::platform::services::normalize_unit_name;

        let unit = normalize_unit_name(service, "service");
        let mut cmd = tokio::process::Command::new("journalctl");
        cmd.args([
            "-u",
            &unit,
            "--output=short-precise",
            "--no-pager",
            "-n",
            &lines,
        ]);
        let since = since_minutes.map(|m| format!("-{m}min"));
        if let Some(since) = &since {
            cmd.args(["--since", since]);
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let output = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            cmd.spawn()
                .map_err(|e| format!("journalctl spawn failed: {e}"))?
                .wait_with_output()
                .await
                .map_err(|e| format!("journalctl failed: {e}"))
        })
        .await
        .map_err(|_| "journalctl timed out after 10s".to_string())??;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "journalctl exited with {}: {}",
                output.status,
                err.chars().take(300).collect::<String>()
            ));
        }

        // Clamp total size, keeping the NEWEST lines — the tail is where the
        // diagnosis usually lives.
        const MAX_CHARS: usize = 12_000;
        let text = String::from_utf8_lossy(&output.stdout);
        let mut kept: Vec<&str> = Vec::new();
        let mut total = 0usize;
        for line in text.lines().rev() {
            total += line.chars().count() + 1;
            if total > MAX_CHARS {
                break;
            }
            kept.push(line);
        }
        kept.reverse();
        let truncated = total > MAX_CHARS;

        Ok(json!({
            "unit": unit,
            "requested_lines": lines.parse::<u64>().unwrap_or(0),
            "returned_lines": kept.len(),
            "truncated_to_fit": truncated,
            "since": since,
            "log": kept.join("\n"),
        }))
    }
}

/// Mirrors `GET /alerts/state`: the (rule, label_set) pairs currently pending
/// or firing, joined to their rule name and severity.
async fn active_alerts(state: &Arc<AppState>) -> Result<Value, String> {
    let rows = AlertRepository::new(state.db.clone())
        .list_active_state()
        .await
        .map_err(|e| e.to_string())?;

    let alerts: Vec<Value> = rows
        .into_iter()
        .map(|(row, name, severity)| {
            json!({
                "name": name,
                "severity": severity.as_str(),
                "state": row.state.as_str(),
                "label_set": row.label_set,
                "last_value": row.last_value,
                "since": row.state_since,
            })
        })
        .collect();

    Ok(json!({ "count": alerts.len(), "alerts": alerts }))
}

/// Mirrors `GET /alerts`: every configured rule with its expression and state.
/// Lets the model reference an existing rule by name before proposing to
/// silence it.
async fn list_alert_rules(state: &Arc<AppState>) -> Result<Value, String> {
    let rules = AlertRepository::new(state.db.clone())
        .list()
        .await
        .map_err(|e| e.to_string())?;

    let out: Vec<Value> = rules
        .into_iter()
        .map(|r| {
            json!({
                "id": r.id,
                "name": r.name,
                "expression": r.expression,
                "severity": r.severity.as_str(),
                "enabled": r.enabled,
                "for_duration_secs": r.for_duration_secs,
                "silenced": r.silenced_until.is_some(),
            })
        })
        .collect();

    Ok(json!({ "count": out.len(), "rules": out }))
}

/// Latest value(s) for an arbitrary metric via the alert resolver — the same
/// engine alert rules evaluate against, so the model reaches every namespace
/// (disk mounts, interfaces, pressure, temperatures, per-container, ...).
async fn query_metric(state: &Arc<AppState>, args: &Value) -> Result<Value, String> {
    let namespace = args
        .get("namespace")
        .and_then(Value::as_str)
        .ok_or("missing 'namespace'")?
        .to_string();
    let field = args
        .get("field")
        .and_then(Value::as_str)
        .ok_or("missing 'field'")?
        .to_string();

    let mut labels = BTreeMap::new();
    if let Some(obj) = args.get("labels").and_then(Value::as_object) {
        for (k, v) in obj {
            let value = v
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string());
            labels.insert(k.clone(), value);
        }
    }

    let metric = MetricRef {
        namespace,
        field,
        labels,
    };
    let samples = resolve_with_state(state, &metric)
        .await
        .map_err(|e| e.message)?;

    let out: Vec<Value> = samples
        .into_iter()
        .map(|s| {
            json!({
                "labels": s.label_set,
                "value": s.value,
                "detail": s.meta,
            })
        })
        .collect();

    Ok(json!({
        "metric": metric.to_string(),
        "count": out.len(),
        "samples": out,
    }))
}

/// Mirrors `GET /services`: init-system units and their state. Listing every
/// unit can be large, so the tool applies an optional name substring and a
/// hard limit before returning.
async fn list_services(state: &Arc<AppState>, args: &Value) -> Result<Value, String> {
    let name_filter = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_lowercase);
    let state_filter = args
        .get("state")
        .and_then(Value::as_str)
        .and_then(parse_service_state);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;

    let services = state
        .service_manager
        .list(ServiceFilter {
            state: state_filter,
        })
        .await
        .map_err(|e| e.to_string())?;

    let filtered: Vec<Value> = services
        .into_iter()
        .filter(|s| {
            name_filter
                .as_deref()
                .is_none_or(|needle| s.name.to_lowercase().contains(needle))
        })
        .take(limit)
        .map(|s| {
            json!({
                "name": s.name,
                "state": service_state_str(&s.state),
                "raw_state": s.raw_state,
                "enabled_at_boot": s.enabled_at_boot,
                "description": s.description,
            })
        })
        .collect();

    Ok(json!({ "count": filtered.len(), "services": filtered }))
}

/// Windowed aggregate (min/max/avg/last + trend) for a performance metric via
/// the resolver's history path. The trend compares the current value to the
/// window average — a cheap, readable "rising/falling/flat" the model can act
/// on without us shipping the full series (which would bloat the context).
async fn metric_history(state: &Arc<AppState>, args: &Value) -> Result<Value, String> {
    let namespace = args
        .get("namespace")
        .and_then(Value::as_str)
        .ok_or("missing 'namespace'")?
        .to_string();
    let field = args
        .get("field")
        .and_then(Value::as_str)
        .ok_or("missing 'field'")?
        .to_string();

    let mut labels = BTreeMap::new();
    if let Some(obj) = args.get("labels").and_then(Value::as_object) {
        for (k, v) in obj {
            let value = v
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string());
            labels.insert(k.clone(), value);
        }
    }

    let window_secs = args
        .get("window_secs")
        .and_then(Value::as_u64)
        .unwrap_or(3600)
        .clamp(60, 30 * 24 * 3600) as i64;
    let resolution = args
        .get("resolution")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| pick_resolution(window_secs).to_string());

    let now = chrono::Utc::now().timestamp();
    let metric = MetricRef {
        namespace,
        field,
        labels,
    };
    let summaries = history_summary(state, &metric, &resolution, now - window_secs, now)
        .await
        .map_err(|e| e.message)?;

    let series: Vec<Value> = summaries
        .into_iter()
        .map(|s| {
            let (last, trend) = if s.last.is_nan() {
                (Value::Null, "unknown")
            } else if s.last > s.avg * 1.05 {
                (json!(s.last), "rising")
            } else if s.last < s.avg * 0.95 {
                (json!(s.last), "falling")
            } else {
                (json!(s.last), "flat")
            };
            json!({
                "labels": s.label_set,
                "count": s.count,
                "min": s.min,
                "max": s.max,
                "avg": s.avg,
                "last": last,
                "trend": trend,
            })
        })
        .collect();

    Ok(json!({
        "metric": metric.to_string(),
        "resolution": resolution,
        "window_secs": window_secs,
        "series": series,
    }))
}

/// Alert fire/resolve timeline. Each event carries the rule's name, so the
/// model reads "high-cpu fired" rather than a bare id without a second read,
/// and an event whose rule has since been deleted still reads as itself.
/// Answers "when did this start".
async fn recent_alert_events(state: &Arc<AppState>, args: &Value) -> Result<Value, String> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 100) as u32;

    let repo = AlertRepository::new(state.db.clone());
    let events = repo
        .recent_events(limit, 0)
        .await
        .map_err(|e| e.to_string())?;

    let out: Vec<Value> = events
        .into_iter()
        .map(|e| {
            json!({
                // Straight off the event: the rule it names may be deleted, and
                // the lookup below would then have nothing to offer.
                "rule": e.rule_name,
                "event": e.event_type.as_str(),
                "severity": e.severity.as_str(),
                "occurred_at": e.occurred_at,
                "value": e.metric_value,
                "label_set": e.label_set,
            })
        })
        .collect();

    Ok(json!({ "count": out.len(), "events": out }))
}

/// PromQL passthrough to the operator-configured Prometheus server. The host is
/// fixed by config (not the model), so there is no SSRF surface — the model
/// only chooses the query and time range.
async fn prometheus_query(state: &Arc<AppState>, args: &Value) -> Result<Value, String> {
    let base = state
        .assistant_config
        .prometheus_url
        .trim()
        .trim_end_matches('/');
    if base.is_empty() {
        return Err("prometheus_url is not configured".to_string());
    }
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or("missing 'query'")?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;

    let (url, form): (String, Vec<(&str, String)>) =
        if let Some(range) = args.get("range_secs").and_then(Value::as_u64) {
            let now = chrono::Utc::now().timestamp();
            let step = args
                .get("step_secs")
                .and_then(Value::as_u64)
                .unwrap_or(60)
                .max(1);
            (
                format!("{base}/api/v1/query_range"),
                vec![
                    ("query", query.to_string()),
                    ("start", (now - range as i64).to_string()),
                    ("end", now.to_string()),
                    ("step", step.to_string()),
                ],
            )
        } else {
            (
                format!("{base}/api/v1/query"),
                vec![("query", query.to_string())],
            )
        };

    let qs = serde_urlencoded::to_string(&form).map_err(|e| e.to_string())?;
    let resp = client
        .get(format!("{url}?{qs}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("prometheus response was not json: {e}"))?;
    if !status.is_success() {
        let detail = body
            .pointer("/error")
            .and_then(Value::as_str)
            .unwrap_or("unknown prometheus error");
        return Err(format!("prometheus error ({status}): {detail}"));
    }

    Ok(json!({
        "status": body.get("status"),
        "data": body.get("data"),
    }))
}

/// Auto-pick a rollup resolution from the look-back window so a wide window
/// doesn't return thousands of raw rows to aggregate.
fn pick_resolution(window_secs: i64) -> &'static str {
    debug_assert!(HISTORY_RESOLUTIONS.contains(&"1m"));
    match window_secs {
        w if w <= 3 * 3600 => "1m",
        w if w <= 2 * 86_400 => "5m",
        _ => "1h",
    }
}

// ===== Propose-only actions =====
//
// These never mutate state. Each validates its arguments, builds the exact REST
// request the confirm will issue, pushes it as a `ProposedAction`, and returns a
// note to the model so it phrases the answer as "prepared, awaiting confirm".
// The real validation/audit happens when the operator confirms and the normal
// REST handler runs.

/// A uniform "drafted" acknowledgement returned to the model.
fn proposed(summary: &str) -> Result<Value, String> {
    Ok(json!({ "proposed": summary, "status": "awaiting operator confirmation" }))
}

/// Guard for model-supplied values interpolated into a proposal path. The
/// operator confirms `method path` verbatim, so a value must not be able to
/// smuggle a different endpoint (`/`, `..`), a query (`?`), or a fragment
/// (`#`) into the request that the confirm actually issues.
fn safe_path_segment<'a>(value: &'a str, what: &str) -> Result<&'a str, String> {
    let v = value.trim();
    if v.is_empty() {
        return Err(format!("'{what}' must not be empty"));
    }
    if v == "." || v == ".." {
        return Err(format!("'{what}' must be a name, not a path"));
    }
    if v.contains(['/', '\\', '?', '#', '%'])
        || v.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!(
            "'{what}' contains characters that are not allowed in a path segment"
        ));
    }
    Ok(v)
}

/// Draft a new alert rule (POST /alerts). The expression is validated up front
/// so the model can't propose something the create endpoint would reject.
fn propose_alert_rule(args: &Value, proposals: &mut Vec<ProposedAction>) -> Result<Value, String> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or("missing 'name'")?
        .trim()
        .to_string();
    if name.is_empty() {
        return Err("'name' must not be empty".to_string());
    }
    let expression = args
        .get("expression")
        .and_then(Value::as_str)
        .ok_or("missing 'expression'")?
        .trim()
        .to_string();
    expression::parse(&expression)
        .map_err(|e| format!("invalid expression '{expression}': {e}"))?;

    let severity = args
        .get("severity")
        .and_then(Value::as_str)
        .unwrap_or("warn");
    if severity != "warn" && severity != "crit" {
        return Err("'severity' must be 'warn' or 'crit'".to_string());
    }
    let for_secs = args
        .get("for_secs")
        .and_then(Value::as_u64)
        .unwrap_or(60)
        .clamp(0, 86_400) as i64;

    let mut body = json!({
        "name": name,
        "expression": expression,
        "severity": severity,
        "for_duration_secs": for_secs,
        "enabled": true,
    });
    if let Some(desc) = args.get("description").and_then(Value::as_str) {
        body["description"] = json!(desc);
    }

    let summary = format!("create {severity} alert '{name}': {expression} for {for_secs}s");
    proposals.push(ProposedAction {
        kind: "create_alert".to_string(),
        summary: summary.clone(),
        method: "POST".to_string(),
        path: "/alerts".to_string(),
        body: Some(body),
    });
    proposed(&summary)
}

/// Draft silencing an existing rule (POST /alerts/{id}/silence), resolving the
/// rule id from its name so the model can speak in names.
async fn propose_silence_alert(
    state: &Arc<AppState>,
    args: &Value,
    proposals: &mut Vec<ProposedAction>,
) -> Result<Value, String> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or("missing alert 'name'")?
        .trim();
    let minutes = args
        .get("minutes")
        .and_then(Value::as_u64)
        .unwrap_or(60)
        .clamp(1, 60 * 24 * 7);

    let rules = AlertRepository::new(state.db.clone())
        .list()
        .await
        .map_err(|e| e.to_string())?;
    let rule = rules
        .iter()
        .find(|r| r.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| format!("no alert rule named '{name}'"))?;

    let summary = format!("silence alert '{}' for {minutes} min", rule.name);
    proposals.push(ProposedAction {
        kind: "silence_alert".to_string(),
        summary: summary.clone(),
        method: "POST".to_string(),
        path: format!("/alerts/{}/silence", rule.id),
        body: Some(json!({ "duration_secs": (minutes * 60) as i64 })),
    });
    proposed(&summary)
}

/// Draft a service start/stop/restart (POST /services/{name}/{action}).
fn propose_service_action(
    args: &Value,
    proposals: &mut Vec<ProposedAction>,
) -> Result<Value, String> {
    let name = safe_path_segment(
        args.get("name")
            .and_then(Value::as_str)
            .ok_or("missing service 'name'")?,
        "name",
    )?;
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .ok_or("missing 'action'")?;
    if !matches!(action, "start" | "stop" | "restart") {
        return Err("'action' must be start, stop or restart".to_string());
    }

    let summary = format!("{action} service '{name}'");
    proposals.push(ProposedAction {
        kind: "service_action".to_string(),
        summary: summary.clone(),
        method: "POST".to_string(),
        path: format!("/services/{name}/{action}"),
        body: None,
    });
    proposed(&summary)
}

/// Draft killing a process (DELETE /processes/{pid}?signal=N). Only SIGTERM and
/// SIGKILL are accepted, matching the REST handler.
fn propose_kill_process(
    args: &Value,
    proposals: &mut Vec<ProposedAction>,
) -> Result<Value, String> {
    let pid = args
        .get("pid")
        .and_then(Value::as_u64)
        .ok_or("missing 'pid'")?;
    let signal = args.get("signal").and_then(Value::as_u64).unwrap_or(15);
    if signal != 9 && signal != 15 {
        return Err("'signal' must be 15 (SIGTERM) or 9 (SIGKILL)".to_string());
    }
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();

    let summary = if name.is_empty() {
        format!("kill process {pid} with signal {signal}")
    } else {
        format!("kill process {pid} ('{name}') with signal {signal}")
    };
    proposals.push(ProposedAction {
        kind: "kill_process".to_string(),
        summary: summary.clone(),
        method: "DELETE".to_string(),
        path: format!("/processes/{pid}?signal={signal}"),
        body: None,
    });
    proposed(&summary)
}

/// Draft a container start/stop/restart (POST /docker/containers/{id}/{action}).
#[cfg(feature = "docker")]
fn propose_container_action(
    args: &Value,
    proposals: &mut Vec<ProposedAction>,
) -> Result<Value, String> {
    let container = safe_path_segment(
        args.get("container")
            .and_then(Value::as_str)
            .ok_or("missing 'container'")?,
        "container",
    )?;
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .ok_or("missing 'action'")?;
    if !matches!(action, "start" | "stop" | "restart") {
        return Err("'action' must be start, stop or restart".to_string());
    }

    let summary = format!("{action} container '{container}'");
    proposals.push(ProposedAction {
        kind: "container_action".to_string(),
        summary: summary.clone(),
        method: "POST".to_string(),
        path: format!("/docker/containers/{container}/{action}"),
        body: None,
    });
    proposed(&summary)
}

/// Mirrors `GET /docker/containers`: name/image/state/status of every
/// container. Compiled only in Docker builds.
#[cfg(feature = "docker")]
async fn list_containers() -> Result<Value, String> {
    let containers = crate::services::docker::list_containers()
        .await
        .map_err(|e| e.to_string())?;

    let out: Vec<Value> = containers
        .into_iter()
        .map(|c| {
            let name = c
                .names
                .as_ref()
                .and_then(|n| n.first())
                .map(|s| s.trim_start_matches('/').to_string());
            json!({
                "name": name,
                "image": c.image,
                "state": c.state.map(|s| s.to_string()),
                "status": c.status,
            })
        })
        .collect();

    Ok(json!({ "count": out.len(), "containers": out }))
}

/// Tail a container's own stdout/stderr — the workload's logs, distinct from
/// this daemon's logs that `read_logs` serves. Compiled only in Docker builds.
#[cfg(feature = "docker")]
async fn read_container_logs(args: &Value) -> Result<Value, String> {
    let container = args
        .get("container")
        .and_then(Value::as_str)
        .ok_or("missing 'container'")?;
    let tail = args
        .get("tail")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;

    let lines = crate::services::docker::get_container_logs(container, Some(tail), None)
        .await
        .map_err(|e| e.to_string())?;

    Ok(json!({ "container": container, "count": lines.len(), "logs": lines }))
}

/// Severity string to the integer the `logs.level` column stores. Must match
/// `LogLevel::as_i32` in services/logging.rs.
fn level_to_int(level: &str) -> Result<i32, String> {
    match level {
        "error" => Ok(1),
        "warn" => Ok(2),
        "info" => Ok(3),
        "debug" => Ok(4),
        "trace" => Ok(5),
        other => Err(format!(
            "unknown level '{other}'; expected error|warn|info|debug|trace"
        )),
    }
}

fn int_to_level(level: i32) -> &'static str {
    match level {
        1 => "error",
        2 => "warn",
        3 => "info",
        4 => "debug",
        _ => "trace",
    }
}

fn process_state_str(state: &ProcessState) -> &'static str {
    match state {
        ProcessState::Running => "running",
        ProcessState::Sleeping => "sleeping",
        ProcessState::Stopped => "stopped",
        ProcessState::Zombie => "zombie",
        ProcessState::Idle => "idle",
        ProcessState::Unknown => "unknown",
    }
}

fn parse_service_state(s: &str) -> Option<ServiceState> {
    match s.to_ascii_lowercase().as_str() {
        "running" => Some(ServiceState::Running),
        "stopped" => Some(ServiceState::Stopped),
        "starting" => Some(ServiceState::Starting),
        "stopping" => Some(ServiceState::Stopping),
        "paused" => Some(ServiceState::Paused),
        "failed" => Some(ServiceState::Failed),
        "reloading" => Some(ServiceState::Reloading),
        _ => None,
    }
}

fn service_state_str(state: &ServiceState) -> &'static str {
    match state {
        ServiceState::Running => "running",
        ServiceState::Stopped => "stopped",
        ServiceState::Starting => "starting",
        ServiceState::Stopping => "stopping",
        ServiceState::Paused => "paused",
        ServiceState::Failed => "failed",
        ServiceState::Reloading => "reloading",
        ServiceState::Unknown => "unknown",
    }
}
