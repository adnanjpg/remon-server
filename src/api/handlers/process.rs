use axum::{extract::Query, http::StatusCode, Json};
use log::debug;
use serde::{Deserialize, Serialize};

use crate::{api::extractors::Claims, monitor::{get_process_list, models::ProcessInfo}};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseBody {
    Success(bool),
    Error(String),
}

#[derive(Serialize)]
pub struct GetProcessesResponse {
    pub processes: Vec<ProcessInfo>,
}

#[derive(Deserialize)]
pub struct KillProcessQuery {
    pub pid: u32,
}

pub async fn get_processes(_claims: Claims) -> Json<GetProcessesResponse> {
    let start = std::time::Instant::now();
    let processes = get_process_list().await;
    debug!(
        "get_processes[{}] took: {:?}",
        processes.len(),
        start.elapsed()
    );

    Json(GetProcessesResponse { processes })
}

pub async fn kill_process(
    _claims: Claims,
    Query(params): Query<KillProcessQuery>,
) -> Result<Json<ResponseBody>, (StatusCode, Json<ResponseBody>)> {
    crate::monitor::kill_process(params.pid)
        .await
        .map_err(|e| {
            debug!("Error killing process: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            )
        })?;

    debug!("Process {} killed successfully", params.pid);
    Ok(Json(ResponseBody::Success(true)))
}
