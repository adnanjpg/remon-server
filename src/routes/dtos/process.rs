use crate::models::process::ProcessInfo;
use serde::Serialize;

#[derive(Serialize)]
pub struct GetProcessesResponse {
    pub processes: Vec<ProcessInfo>,
}
