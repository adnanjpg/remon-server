use crate::monitor::models::ProcessInfo;
use serde::Serialize;

#[derive(Serialize)]
pub struct GetProcessesResponse {
    pub processes: Vec<ProcessInfo>,
}