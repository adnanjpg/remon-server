use axum::{extract::State, http::StatusCode, Json, extract::Path};
use log::debug;
use std::sync::Arc;
use sysinfo::System;

use crate::{
    routes::{
        dtos::{common::ResponseBody, process::GetProcessesResponse},
        extractors::Claims,
    },
    services::process,
    state::AppState,
};

/// GET /processes - Get all running processes
pub async fn get_processes(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> Json<GetProcessesResponse> {
    let start = std::time::Instant::now();

    // Subscribe to the processes broadcast to get the latest snapshot
    let mut rx = state.processes_tx.subscribe();

    // Try to receive the latest process list, or fetch it directly if channel is empty
    let process_list = match rx.try_recv() {
        Ok(processes) => processes,
        Err(_) => {
            // Fallback: fetch processes directly
            let mut sys = System::new_all();
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

/// DELETE /processes/:pid - Kill a process
pub async fn delete_process(
    _claims: Claims,
    Path(pid): Path<u32>,
) -> Result<Json<ResponseBody>, (StatusCode, Json<ResponseBody>)> {
    let mut sys = System::new_all();
    sys.refresh_all();

    process::kill_process(&sys, pid, 15) // SIGTERM
        .map_err(|e| {
            debug!("Error killing process: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e)),
            )
        })?;

    debug!("Process {} killed successfully", pid);
    Ok(Json(ResponseBody::Success(true)))
}
