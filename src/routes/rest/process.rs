use axum::{Json, extract::Path, extract::State, http::StatusCode};
use log::debug;
use std::sync::Arc;
use sysinfo::{MINIMUM_CPU_UPDATE_INTERVAL, System};

use crate::error::{AppError, AppResult};
use crate::routes::{dtos::process::GetProcessesResponse, extractors::Claims};
use crate::services::process;
use crate::state::AppState;

/// GET /processes — return the latest process snapshot.
///
/// Reads from `state.processes_latest`, populated by the processes
/// collector after each refresh. The collector's first publish is gated
/// behind a `MINIMUM_CPU_UPDATE_INTERVAL` warm-up so the cache only ever
/// holds frames with valid `cpu_percent` deltas.
///
/// Cold-start fallback (cache empty — request landed in the first ~200 ms
/// of server life): pay the warm-up here too. Two refreshes spaced one
/// `MINIMUM_CPU_UPDATE_INTERVAL` apart give sysinfo enough delta to
/// compute meaningful CPU%; doing them back-to-back would zero everything
/// out — that was the prior bug.
pub async fn get_processes(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> Json<GetProcessesResponse> {
    let start = std::time::Instant::now();

    let cached = state.processes_latest.read().await.clone();
    let process_list = match cached {
        Some(p) => p,
        None => {
            let mut sys = System::new_all();
            tokio::time::sleep(MINIMUM_CPU_UPDATE_INTERVAL).await;
            sys.refresh_all();
            process::get_processes(&sys)
        }
    };

    debug!(
        "get_processes[{}] took: {:?}",
        process_list.processes.len(),
        start.elapsed()
    );

    Json(GetProcessesResponse {
        processes: process_list.processes,
    })
}

/// DELETE /processes/{pid} — kill a process by PID.
///
/// Goes through the OS directly (Unix `kill(2)` / Windows `TerminateProcess`),
/// no sysinfo full-system inventory. SIGTERM on Unix; signal arg is ignored
/// on Windows (no POSIX signals there).
pub async fn delete_process(_claims: Claims, Path(pid): Path<u32>) -> AppResult<StatusCode> {
    process::kill_process(pid, 15).map_err(AppError::ProcessKillFailed)?;

    debug!("Process {} killed successfully", pid);
    Ok(StatusCode::NO_CONTENT)
}
