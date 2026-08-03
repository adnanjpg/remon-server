use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod factory;

#[cfg(target_os = "linux")]
mod openrc;
#[cfg(target_os = "linux")]
mod systemd;
mod unsupported;
#[cfg(target_os = "windows")]
mod windows_scm;

// ===== Types =====

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Running,
    Stopped,
    Starting,
    Stopping,
    /// Windows SCM: SERVICE_PAUSED
    Paused,
    /// systemd "failed", OpenRC "crashed"
    Failed,
    Reloading,
    Unknown,
}

impl ServiceState {
    pub fn as_str(&self) -> &'static str {
        match self {
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceBackend {
    Systemd,
    /// Not `open_rc`: the tool spells itself OpenRC, and this is the value
    /// clients have always been served.
    #[serde(rename = "openrc")]
    OpenRc,
    WindowsScm,
    Unknown,
}

impl ServiceBackend {
    /// Must stay identical to what the `snake_case` rename above produces:
    /// `ServiceDto` builds `backend` from this rather than serialising the
    /// enum, so a disagreement here means the wire value depends on which
    /// path a response happened to take. `windows_scm` read `windowsscm`
    /// until the web client's declared union caught it.
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceBackend::Systemd => "systemd",
            ServiceBackend::OpenRc => "openrc",
            ServiceBackend::WindowsScm => "windows_scm",
            ServiceBackend::Unknown => "unknown",
        }
    }
}

#[cfg(test)]
mod wire_value_tests {
    use super::*;

    /// The DTO path and the serde path have to name the same thing. Both of
    /// these enums reach clients through `as_str` (via `ServiceDto`) while
    /// still deriving `Serialize`, so nothing but a test stops the two from
    /// drifting — which is exactly how `windowsscm` shipped.
    #[test]
    fn backend_as_str_matches_serde() {
        for backend in [
            ServiceBackend::Systemd,
            ServiceBackend::OpenRc,
            ServiceBackend::WindowsScm,
            ServiceBackend::Unknown,
        ] {
            let serialised = serde_json::to_string(&backend).expect("enum serialises");
            assert_eq!(
                serialised.trim_matches('"'),
                backend.as_str(),
                "{backend:?} disagrees between as_str and serde"
            );
        }
    }

    #[test]
    fn state_as_str_matches_serde() {
        for state in [
            ServiceState::Running,
            ServiceState::Stopped,
            ServiceState::Starting,
            ServiceState::Stopping,
            ServiceState::Paused,
            ServiceState::Failed,
            ServiceState::Reloading,
            ServiceState::Unknown,
        ] {
            let serialised = serde_json::to_string(&state).expect("enum serialises");
            assert_eq!(
                serialised.trim_matches('"'),
                state.as_str(),
                "{state:?} disagrees between as_str and serde"
            );
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    /// Full unit name, e.g. "nginx.service"
    pub name: String,
    pub description: Option<String>,
    pub state: ServiceState,
    /// Platform-specific detail, e.g. "active/running" or "RUNNING"
    pub raw_state: String,
    /// None when enablement state cannot be determined (transient units, etc.)
    pub enabled_at_boot: Option<bool>,
    pub backend: ServiceBackend,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimerUnit {
    /// Full unit name, e.g. "logrotate.timer"
    pub name: String,
    /// Associated service unit name, e.g. "logrotate.service"
    pub service: Option<String>,
    pub description: Option<String>,
    pub state: ServiceState,
    pub raw_state: String,
    pub enabled_at_boot: Option<bool>,
    /// Unix timestamp (seconds) of the next scheduled trigger. None if inactive or unknown.
    pub next_run: Option<i64>,
    /// Unix timestamp (seconds) of the last trigger. None if never triggered or unknown.
    pub last_run: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct ServiceFilter {
    /// If set, only services matching this state are returned.
    pub state: Option<ServiceState>,
}

// ===== Error =====

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("Service '{0}' not found")]
    NotFound(String),

    #[error("Operation not supported on this platform")]
    NotSupported,

    /// The remon process lacks the required privileges.
    /// On Linux with systemd, running as root or having the appropriate
    /// PolicyKit rules allows service management.
    #[error("Permission denied — remon may need root or PolicyKit rules")]
    PermissionDenied,

    #[error("Init system error: {0}")]
    BackendError(String),
}

// ===== Trait =====

#[async_trait]
pub trait ServiceManager: Send + Sync {
    async fn list(&self, filter: ServiceFilter) -> Result<Vec<Service>, ServiceError>;
    async fn get(&self, name: &str) -> Result<Service, ServiceError>;
    async fn start(&self, name: &str) -> Result<(), ServiceError>;
    async fn stop(&self, name: &str) -> Result<(), ServiceError>;
    async fn restart(&self, name: &str) -> Result<(), ServiceError>;
    async fn enable_at_boot(&self, name: &str) -> Result<(), ServiceError>;
    async fn disable_at_boot(&self, name: &str) -> Result<(), ServiceError>;

    /// Send a reload signal (SIGHUP / systemctl reload). Returns NotSupported
    /// on platforms that have no concept of in-process config reload.
    async fn reload(&self, name: &str) -> Result<(), ServiceError> {
        let _ = name;
        Err(ServiceError::NotSupported)
    }

    /// List scheduled timer units. Returns NotSupported on platforms without
    /// a native timer concept (OpenRC, Windows SCM — use cron/Task Scheduler
    /// instead, covered by a separate ScheduledTaskManager in Phase 2).
    async fn list_timers(&self) -> Result<Vec<TimerUnit>, ServiceError> {
        Err(ServiceError::NotSupported)
    }

    /// Enable a timer unit at boot. Distinct from `enable_at_boot`: the unit
    /// must be normalised with the `.timer` suffix, not `.service`, or a bare
    /// name silently targets the wrong unit. NotSupported where there are no
    /// timers.
    async fn enable_timer(&self, name: &str) -> Result<(), ServiceError> {
        let _ = name;
        Err(ServiceError::NotSupported)
    }

    /// Disable a timer unit at boot. See [`ServiceManager::enable_timer`].
    async fn disable_timer(&self, name: &str) -> Result<(), ServiceError> {
        let _ = name;
        Err(ServiceError::NotSupported)
    }
}

// ===== Helpers =====

/// Normalize a user-supplied service name to a full systemd unit name.
/// Accepts "nginx" or "nginx.service"; always returns "nginx.service".
#[allow(dead_code)]
pub fn normalize_unit_name(name: &str, suffix: &str) -> String {
    if name.contains('.') {
        name.to_string()
    } else {
        format!("{}.{}", name, suffix)
    }
}
