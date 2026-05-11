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
use crate::models::process::ProcessList;
use crate::routes::{dtos::process::GetProcessesResponse, extractors::Claims};
use crate::services::process;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct KillProcessQuery {
    /// POSIX signal number. Defaults to 15 (SIGTERM). Only 9 (SIGKILL) and
    /// 15 (SIGTERM) are accepted — anything else is rejected as a bad
    /// request rather than silently coerced. Ignored on Windows, which
    /// has no POSIX signals.
    pub signal: Option<i32>,
}

/// GET /processes — return the latest process snapshot.
///
/// Reads from `state.processes_latest` when fresh; otherwise refreshes the
/// snapshot on demand. The refresh is serialized by
/// `state.processes_refresh_lock` so a burst of callers does not multiply
/// the full process-table scan.
///
/// Cold/stale refresh pays sysinfo's warm-up cost: two process refreshes
/// spaced one `MINIMUM_CPU_UPDATE_INTERVAL` apart give meaningful CPU%;
/// doing them back-to-back would zero everything out.
pub async fn get_processes(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> Json<GetProcessesResponse> {
    let start = std::time::Instant::now();

    let process_list = get_or_refresh_processes(&state).await;

    debug!(
        "get_processes[{}] took: {:?}",
        process_list.processes.len(),
        start.elapsed()
    );

    Json(GetProcessesResponse {
        processes: process_list.processes,
    })
}

async fn get_or_refresh_processes(state: &AppState) -> ProcessList {
    if let Some(cached) = fresh_cached_processes(state).await {
        return cached;
    }

    let _guard = state.processes_refresh_lock.lock().await;

    // Another request may have refreshed while we waited for the lock.
    if let Some(cached) = fresh_cached_processes(state).await {
        return cached;
    }

    let refreshed = refresh_process_snapshot().await;
    *state.processes_latest.write().await = Some(refreshed.clone());
    let _ = state.processes_tx.send(refreshed.clone());
    refreshed
}

async fn fresh_cached_processes(state: &AppState) -> Option<ProcessList> {
    let now = chrono::Utc::now().timestamp();
    let ttl_secs =
        ((state.processes_cache_ttl_ms.load(Ordering::Relaxed) + 999) / 1000).max(1) as i64;
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

    debug!("Process {} killed with signal {}", pid, signal);
    Ok(StatusCode::NO_CONTENT)
}
