#![cfg(target_os = "linux")]

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::process::Command;

use super::{
    Service, ServiceBackend, ServiceError, ServiceFilter, ServiceManager, ServiceState, TimerUnit,
    normalize_unit_name,
};

pub struct SystemdManager;

// ===== systemctl JSON record shapes =====

#[derive(Deserialize)]
struct UnitRecord {
    unit: String,
    load: String,
    active: String,
    sub: String,
    description: String,
}

#[derive(Deserialize)]
struct UnitFileRecord {
    unit_file: String,
    state: String,
}

// ===== Internal helpers =====

async fn run_systemctl(args: &[&str]) -> Result<String, ServiceError> {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .await
        .map_err(|e| ServiceError::BackendError(format!("systemctl exec: {}", e)))?;

    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("Access denied") || stderr.contains("Failed to connect to bus") {
        return Err(ServiceError::PermissionDenied);
    }
    Err(ServiceError::BackendError(stderr.trim().to_string()))
}

async fn run_systemctl_action(args: &[&str], unit: &str) -> Result<(), ServiceError> {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .await
        .map_err(|e| ServiceError::BackendError(e.to_string()))?;

    if out.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("not found")
        || stderr.contains("No such file")
        || stderr.contains("could not be found")
    {
        return Err(ServiceError::NotFound(unit.to_string()));
    }
    if stderr.contains("Access denied") || stderr.contains("Failed to connect to bus") {
        return Err(ServiceError::PermissionDenied);
    }
    Err(ServiceError::BackendError(stderr.trim().to_string()))
}

/// Returns a map of unit_name → enablement_state for the given unit type.
async fn unit_file_states(unit_type: &str) -> HashMap<String, String> {
    let args = [
        "list-unit-files",
        "--output=json",
        "--no-pager",
        &format!("--type={}", unit_type),
    ];
    let Ok(raw) = run_systemctl(&args).await else {
        return HashMap::new();
    };
    let Ok(records) = serde_json::from_str::<Vec<UnitFileRecord>>(&raw) else {
        return HashMap::new();
    };
    records
        .into_iter()
        .map(|r| (r.unit_file, r.state))
        .collect()
}

fn parse_state(active: &str, sub: &str) -> ServiceState {
    match active {
        "active" => match sub {
            "running" => ServiceState::Running,
            "reload" => ServiceState::Reloading,
            // One-shot services that completed successfully land here.
            _ => ServiceState::Running,
        },
        "reloading" => ServiceState::Reloading,
        "activating" => ServiceState::Starting,
        "deactivating" => ServiceState::Stopping,
        "failed" => ServiceState::Failed,
        _ => ServiceState::Stopped, // inactive, dead
    }
}

/// One row from `systemctl list-timers --output=json`. Unlike `systemctl
/// show`, this output gives raw microsecond integers regardless of locale
/// or systemd version. `next`/`last` are unix-epoch microseconds.
#[derive(Deserialize)]
struct TimerListRecord {
    unit: String,
    /// Next scheduled trigger (unix usec). 0 / u64::MAX if unscheduled.
    #[serde(default)]
    next: u64,
    /// Last trigger (unix usec). 0 / u64::MAX if never triggered.
    #[serde(default)]
    last: u64,
}

/// Fetch a `unit_name → (next_secs, last_secs)` map by parsing
/// `systemctl list-timers --output=json` once. The legacy per-unit
/// `systemctl show … --property=NextElapseUSecRealtime,LastTriggerUSec`
/// path that this replaces returns localised human strings on systemd 257
/// (e.g. "Thu 2026-04-30 07:30:14 CEST"), which can't be parsed without a
/// timezone-aware date library and is fragile across locales. The
/// `list-timers` JSON output is locale-independent.
async fn fetch_all_timer_times() -> HashMap<String, (Option<i64>, Option<i64>)> {
    let Ok(raw) = run_systemctl(&["list-timers", "--output=json", "--no-pager", "--all"]).await
    else {
        return HashMap::new();
    };
    let Ok(records) = serde_json::from_str::<Vec<TimerListRecord>>(&raw) else {
        return HashMap::new();
    };
    records
        .into_iter()
        .map(|r| (r.unit, (usec_to_secs(r.next), usec_to_secs(r.last))))
        .collect()
}

/// Convert a systemd microsecond unix timestamp to seconds. Returns None
/// for 0 (unset) and u64::MAX (never).
fn usec_to_secs(us: u64) -> Option<i64> {
    if us == 0 || us == u64::MAX {
        return None;
    }
    Some((us / 1_000_000) as i64)
}

fn is_enabled(state: Option<&String>) -> Option<bool> {
    state.map(|s| {
        matches!(
            s.as_str(),
            "enabled" | "enabled-runtime" | "static" | "generated"
        )
    })
}

fn unit_name_to_service(unit: &UnitRecord, file_states: &HashMap<String, String>) -> Service {
    let state = parse_state(&unit.active, &unit.sub);
    let enabled = is_enabled(file_states.get(&unit.unit));
    Service {
        name: unit.unit.clone(),
        description: Some(unit.description.clone()),
        state,
        raw_state: format!("{}/{}", unit.active, unit.sub),
        enabled_at_boot: enabled,
        backend: ServiceBackend::Systemd,
    }
}

// ===== ServiceManager impl =====

#[async_trait]
impl ServiceManager for SystemdManager {
    async fn list(&self, filter: ServiceFilter) -> Result<Vec<Service>, ServiceError> {
        let raw = run_systemctl(&[
            "list-units",
            "--type=service",
            "--output=json",
            "--no-pager",
            "--all",
        ])
        .await?;

        let records: Vec<UnitRecord> = serde_json::from_str(&raw)
            .map_err(|e| ServiceError::BackendError(format!("parse error: {}", e)))?;

        let file_states = unit_file_states("service").await;

        let services: Vec<Service> = records
            .iter()
            .filter(|u| u.unit.ends_with(".service") && u.load != "not-found")
            .map(|u| unit_name_to_service(u, &file_states))
            .filter(|s| match &filter.state {
                Some(f) => &s.state == f,
                None => true,
            })
            .collect();

        Ok(services)
    }

    async fn get(&self, name: &str) -> Result<Service, ServiceError> {
        // Direct lookup via `systemctl show` — one invocation that returns
        // just the properties we need. Previously this delegated to
        // `list()` which scanned every loaded unit; on a typical box (200+
        // units) that was 100–500 ms vs ~10 ms for the targeted call.
        let unit = normalize_unit_name(name, "service");
        let raw = run_systemctl(&[
            "show",
            &unit,
            "--property=LoadState,ActiveState,SubState,Description,UnitFileState",
        ])
        .await?;

        let mut load_state = String::new();
        let mut active_state = String::new();
        let mut sub_state = String::new();
        let mut description = String::new();
        let mut unit_file_state: Option<String> = None;

        for line in raw.lines() {
            if let Some(v) = line.strip_prefix("LoadState=") {
                load_state = v.to_string();
            } else if let Some(v) = line.strip_prefix("ActiveState=") {
                active_state = v.to_string();
            } else if let Some(v) = line.strip_prefix("SubState=") {
                sub_state = v.to_string();
            } else if let Some(v) = line.strip_prefix("Description=") {
                description = v.to_string();
            } else if let Some(v) = line.strip_prefix("UnitFileState=") {
                if !v.is_empty() {
                    unit_file_state = Some(v.to_string());
                }
            }
        }

        // `systemctl show` returns success even for nonexistent units; the
        // signal is `LoadState=not-found`. Translate that into our 404 so
        // callers don't get a default-valued Service for a missing unit.
        if load_state == "not-found" || active_state.is_empty() {
            return Err(ServiceError::NotFound(unit));
        }

        let state = parse_state(&active_state, &sub_state);
        let enabled = is_enabled(unit_file_state.as_ref());

        Ok(Service {
            name: unit,
            description: if description.is_empty() {
                None
            } else {
                Some(description)
            },
            state,
            raw_state: format!("{}/{}", active_state, sub_state),
            enabled_at_boot: enabled,
            backend: ServiceBackend::Systemd,
        })
    }

    async fn start(&self, name: &str) -> Result<(), ServiceError> {
        let unit = normalize_unit_name(name, "service");
        run_systemctl_action(&["start", &unit], &unit).await
    }

    async fn stop(&self, name: &str) -> Result<(), ServiceError> {
        let unit = normalize_unit_name(name, "service");
        run_systemctl_action(&["stop", &unit], &unit).await
    }

    async fn restart(&self, name: &str) -> Result<(), ServiceError> {
        let unit = normalize_unit_name(name, "service");
        run_systemctl_action(&["restart", &unit], &unit).await
    }

    async fn reload(&self, name: &str) -> Result<(), ServiceError> {
        let unit = normalize_unit_name(name, "service");
        run_systemctl_action(&["reload", &unit], &unit).await
    }

    async fn enable_at_boot(&self, name: &str) -> Result<(), ServiceError> {
        let unit = normalize_unit_name(name, "service");
        run_systemctl_action(&["enable", &unit], &unit).await
    }

    async fn disable_at_boot(&self, name: &str) -> Result<(), ServiceError> {
        let unit = normalize_unit_name(name, "service");
        run_systemctl_action(&["disable", &unit], &unit).await
    }

    async fn list_timers(&self) -> Result<Vec<TimerUnit>, ServiceError> {
        let raw = run_systemctl(&[
            "list-units",
            "--type=timer",
            "--output=json",
            "--no-pager",
            "--all",
        ])
        .await?;

        let records: Vec<UnitRecord> = serde_json::from_str(&raw)
            .map_err(|e| ServiceError::BackendError(format!("parse error: {}", e)))?;

        // Bulk-load enablement and next/last triggers in two extra calls
        // (was N+1 calls when we did per-timer `systemctl show`).
        let file_states = unit_file_states("timer").await;
        let timer_times = fetch_all_timer_times().await;

        let mut timers = Vec::new();
        for u in records
            .iter()
            .filter(|u| u.unit.ends_with(".timer") && u.load != "not-found")
        {
            let (next_run, last_run) = timer_times.get(&u.unit).copied().unwrap_or((None, None));
            let service = Some(u.unit.replace(".timer", ".service"));
            let enabled = is_enabled(file_states.get(&u.unit));
            timers.push(TimerUnit {
                name: u.unit.clone(),
                service,
                description: Some(u.description.clone()),
                state: parse_state(&u.active, &u.sub),
                raw_state: format!("{}/{}", u.active, u.sub),
                enabled_at_boot: enabled,
                next_run,
                last_run,
            });
        }

        Ok(timers)
    }
}
