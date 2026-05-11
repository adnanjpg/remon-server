use serde::{Deserialize, Serialize};

use crate::platform::services::{Service, ServiceState, TimerUnit};

// ===== Service DTOs =====

#[derive(Debug, Serialize)]
pub struct ServiceDto {
    pub name: String,
    pub description: Option<String>,
    pub state: String,
    pub raw_state: String,
    pub enabled_at_boot: Option<bool>,
    pub backend: String,
}

impl From<Service> for ServiceDto {
    fn from(s: Service) -> Self {
        Self {
            name: s.name,
            description: s.description,
            state: state_to_str(&s.state).to_string(),
            raw_state: s.raw_state,
            enabled_at_boot: s.enabled_at_boot,
            backend: format!("{:?}", s.backend).to_lowercase(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListServicesResponse {
    pub services: Vec<ServiceDto>,
}

#[derive(Debug, Deserialize)]
pub struct ListServicesQuery {
    /// Filter by state: "running" | "stopped" | "failed" | "starting" | "stopping" | "unknown"
    pub state: Option<String>,
}

impl ListServicesQuery {
    pub fn into_filter_state(&self) -> Option<ServiceState> {
        self.state.as_deref().and_then(parse_state_param)
    }
}

#[derive(Debug, Serialize)]
pub struct ServiceActionResponse {
    pub success: bool,
    pub message: String,
}

impl ServiceActionResponse {
    pub fn ok(msg: impl Into<String>) -> Self {
        Self {
            success: true,
            message: msg.into(),
        }
    }
}

// ===== Timer DTOs =====

#[derive(Debug, Serialize)]
pub struct TimerDto {
    pub name: String,
    pub service: Option<String>,
    pub description: Option<String>,
    pub state: String,
    pub raw_state: String,
    pub enabled_at_boot: Option<bool>,
    /// Unix timestamp (seconds) of the next scheduled trigger. Null if inactive or unknown.
    pub next_run: Option<i64>,
    /// Unix timestamp (seconds) of the last trigger. Null if never triggered or unknown.
    pub last_run: Option<i64>,
}

impl From<TimerUnit> for TimerDto {
    fn from(t: TimerUnit) -> Self {
        Self {
            name: t.name,
            service: t.service,
            description: t.description,
            state: state_to_str(&t.state).to_string(),
            raw_state: t.raw_state,
            enabled_at_boot: t.enabled_at_boot,
            next_run: t.next_run,
            last_run: t.last_run,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListTimersResponse {
    pub timers: Vec<TimerDto>,
}

// ===== Helpers =====

fn state_to_str(s: &ServiceState) -> &'static str {
    match s {
        ServiceState::Running => "running",
        ServiceState::Stopped => "stopped",
        ServiceState::Starting => "starting",
        ServiceState::Stopping => "stopping",
        ServiceState::Paused => "paused",
        ServiceState::Failed => "failed",
        ServiceState::Reloading => "reloading",
        ServiceState::Unknown => "unknown",
    }
}

fn parse_state_param(s: &str) -> Option<ServiceState> {
    match s {
        "running" => Some(ServiceState::Running),
        "stopped" => Some(ServiceState::Stopped),
        "starting" => Some(ServiceState::Starting),
        "stopping" => Some(ServiceState::Stopping),
        "paused" => Some(ServiceState::Paused),
        "failed" => Some(ServiceState::Failed),
        "reloading" => Some(ServiceState::Reloading),
        "unknown" => Some(ServiceState::Unknown),
        _ => None,
    }
}
