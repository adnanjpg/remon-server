use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use log::debug;
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use sysinfo::{MINIMUM_CPU_UPDATE_INTERVAL, ProcessesToUpdate, System};

use crate::error::{AppError, AppResult};
use crate::models::process::{ProcessList, ProcessState};
use crate::routes::{dtos::process::GetProcessesResponse, extractors::Claims};
use crate::services::process;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct GetProcessesQuery {
    /// Filter by process name (case-insensitive substring match).
    pub search: Option<String>,
    /// Filter by state: running | sleeping | stopped | zombie | idle
    pub state: Option<String>,
    /// Sort field: cpu (default) | memory | pid | name
    #[serde(default = "default_sort")]
    pub sort: String,
    /// Maximum number of processes to return (default: 100, max: 1000).
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Zero-based offset for pagination.
    #[serde(default)]
    pub offset: usize,
}

fn default_sort() -> String {
    "cpu".to_string()
}

fn default_limit() -> usize {
    100
}

#[derive(Debug, Deserialize)]
pub struct KillProcessQuery {
    /// POSIX signal number. Defaults to 15 (SIGTERM). Only 9 (SIGKILL) and
    /// 15 (SIGTERM) are accepted — anything else is rejected as a bad
    /// request rather than silently coerced. Ignored on Windows, which
    /// has no POSIX signals.
    pub signal: Option<i32>,
}

/// GET /processes — return the latest process snapshot with optional filtering and pagination.
pub async fn get_processes(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Query(q): Query<GetProcessesQuery>,
) -> Json<GetProcessesResponse> {
    let start = std::time::Instant::now();

    let process_list = get_or_refresh_processes(&state).await;
    let total = process_list.processes.len();

    let limit = q.limit.min(1000);

    // Fold filter inputs once; the closure runs per-process.
    let search_needle = q.search.as_deref().map(str::to_lowercase);
    let state_needle = q.state.as_deref().map(str::to_ascii_lowercase);

    let mut processes: Vec<_> = process_list
        .processes
        .iter()
        .filter(|p| {
            if let Some(needle) = &search_needle
                && !p.name.to_lowercase().contains(needle)
            {
                return false;
            }
            if let Some(sf) = state_needle.as_deref() {
                let matches = matches!(
                    (&p.state, sf),
                    (ProcessState::Running, "running")
                        | (ProcessState::Sleeping, "sleeping")
                        | (ProcessState::Stopped, "stopped")
                        | (ProcessState::Zombie, "zombie")
                        | (ProcessState::Idle, "idle")
                );
                if !matches {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect();

    match q.sort.as_str() {
        "memory" => processes.sort_unstable_by_key(|b| std::cmp::Reverse(b.memory_bytes)),
        "pid" => processes.sort_unstable_by_key(|p| p.pid),
        "name" => processes.sort_unstable_by(|a, b| a.name.cmp(&b.name)),
        _ => processes.sort_unstable_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
    }

    let filtered_total = processes.len();
    let processes: Vec<_> = processes.into_iter().skip(q.offset).take(limit).collect();

    debug!(
        "get_processes total={} filtered={} returned={} took={:?}",
        total,
        filtered_total,
        processes.len(),
        start.elapsed()
    );

    Json(GetProcessesResponse {
        processes,
        total,
        filtered_total,
    })
}

async fn get_or_refresh_processes(state: &AppState) -> Arc<ProcessList> {
    if let Some(cached) = fresh_cached_processes(state).await {
        return cached;
    }

    let _guard = state.processes_refresh_lock.lock().await;

    // Another request may have refreshed while we waited for the lock.
    if let Some(cached) = fresh_cached_processes(state).await {
        return cached;
    }

    let refreshed = Arc::new(refresh_process_snapshot().await);
    *state.processes_latest.write().await = Some(Arc::clone(&refreshed));
    let _ = state.processes_tx.send(Arc::clone(&refreshed));
    refreshed
}

async fn fresh_cached_processes(state: &AppState) -> Option<Arc<ProcessList>> {
    let now = chrono::Utc::now().timestamp();
    let ttl_secs = state
        .processes_cache_ttl_ms
        .load(Ordering::Relaxed)
        .div_ceil(1000)
        .max(1) as i64;
    state
        .processes_latest
        .read()
        .await
        .as_ref()
        .filter(|p| now.saturating_sub(p.timestamp) <= ttl_secs)
        .cloned()
}

async fn refresh_process_snapshot() -> ProcessList {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    tokio::time::sleep(MINIMUM_CPU_UPDATE_INTERVAL).await;
    sys.refresh_processes(ProcessesToUpdate::All, true);
    process::get_processes(&sys)
}

/// DELETE /processes/{pid}?signal=N — kill a process by PID.
///
/// Goes through the OS directly (Unix `kill(2)` / Windows `TerminateProcess`),
/// no sysinfo full-system inventory. Default signal is 15 (SIGTERM); pass
/// `?signal=9` for SIGKILL on stuck processes. The signal arg is ignored on
/// Windows (no POSIX signals there).
pub async fn delete_process(
    _claims: Claims,
    Path(pid): Path<u32>,
    Query(q): Query<KillProcessQuery>,
) -> AppResult<StatusCode> {
    let signal = q.signal.unwrap_or(15);
    if !matches!(signal, 9 | 15) {
        return Err(AppError::BadRequest(format!(
            "signal must be 9 (SIGKILL) or 15 (SIGTERM); got {}",
            signal
        )));
    }
    process::kill_process(pid, signal).map_err(AppError::ProcessKillFailed)?;

    debug!("process {} killed with signal {}", pid, signal);
    Ok(StatusCode::NO_CONTENT)
}
