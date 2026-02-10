use serde::{Deserialize, Serialize};

/// Process information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub cmd: Vec<String>,
    pub exe: Option<String>,
    pub user: Option<String>,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub memory_percent: f64,
    pub state: ProcessState,
    pub started_at: Option<i64>,
    pub threads: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessState {
    Running,
    Sleeping,
    Stopped,
    Zombie,
    Idle,
    Unknown,
}

impl From<&str> for ProcessState {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "running" | "run" => ProcessState::Running,
            "sleeping" | "sleep" => ProcessState::Sleeping,
            "stopped" | "stop" => ProcessState::Stopped,
            "zombie" => ProcessState::Zombie,
            "idle" => ProcessState::Idle,
            _ => ProcessState::Unknown,
        }
    }
}

/// List of processes with metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessList {
    pub processes: Vec<ProcessInfo>,
    pub total_count: usize,
    pub timestamp: i64,
}

/// Request to kill a process
#[derive(Debug, Deserialize)]
pub struct KillProcessRequest {
    /// Signal to send (default: 15 = SIGTERM)
    #[serde(default = "default_signal")]
    pub signal: i32,
}

fn default_signal() -> i32 {
    15 // SIGTERM
}
