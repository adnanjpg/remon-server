use crate::models::process::ProcessInfo;
use serde::Serialize;

#[derive(Serialize)]
pub struct GetProcessesResponse {
    /// Collection time in Unix seconds.
    pub timestamp: i64,
    pub processes: Vec<ProcessInfo>,
    /// Total number of processes on the system (before filtering).
    pub total: usize,
    /// Number of processes after applying search/state filters (before pagination).
    pub filtered_total: usize,
}
