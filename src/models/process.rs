use serde::{Deserialize, Serialize};

/// Process information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    /// PID of the spawning process. `None` for PID 1 (init/systemd) and a
    /// few kernel-managed roots; always set otherwise. Clients use this to
    /// build the process tree without a second round-trip.
    pub parent_pid: Option<u32>,
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
