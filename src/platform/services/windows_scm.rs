/// Windows Service Control Manager — PowerShell shell-out implementation.
///
/// Uses `Get-Service` / `Start-Service` / `Stop-Service` etc. via PowerShell
/// so no unsafe Win32 bindings are required in Phase 2.
/// Phase 3 can swap this for direct SCM API calls using the `windows` crate
/// (Win32_System_Services) for lower latency and richer error codes.
use async_trait::async_trait;
use serde::Deserialize;
use tokio::process::Command;

use super::{Service, ServiceBackend, ServiceError, ServiceFilter, ServiceManager, ServiceState};

pub struct WindowsScmManager;

// ===== PowerShell record shapes =====

#[derive(Deserialize)]
struct ScmRecord {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "DisplayName")]
    display_name: String,
    /// ServiceControllerStatus enum value:
    /// 1=Stopped 2=StartPending 3=StopPending 4=Running
    /// 5=ContinuePending 6=PausePending 7=Paused
    #[serde(rename = "Status")]
    status: u32,
    /// ServiceStartMode enum value:
    /// 0=Boot 1=System 2=Automatic 3=Manual 4=Disabled
    #[serde(rename = "StartType")]
    start_type: u32,
}

// ===== Internal helpers =====

/// Phrases PowerShell uses for permission failures across `Start-Service`,
/// `Stop-Service`, `Set-Service`, etc. The wording varies per cmdlet:
/// - "Access is denied" / "access denied" — explicit ACL refusal
/// - "Cannot open ... service on computer" — `Start/Stop-Service` reports
///   this when the SCM handle open is denied; the literal word "denied"
///   does not appear in the message
/// - "requires elevation" / "ElevationRequired" — UAC prompt rejected
/// - "Unauthorized" — generic auth fall-through
fn is_permission_denied(stderr: &str) -> bool {
    let s = stderr;
    s.contains("Access")
        || s.contains("denied")
        || s.contains("Unauthorized")
        || s.contains("Cannot open")
        || s.contains("requires elevation")
        || s.contains("ElevationRequired")
}

async fn run_ps(cmd: &str) -> Result<String, ServiceError> {
    let out = Command::new("powershell")
        .args(["-NonInteractive", "-Command", cmd])
        .output()
        .await
        .map_err(|e| ServiceError::BackendError(format!("PowerShell exec: {}", e)))?;

    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    if is_permission_denied(&stderr) {
        return Err(ServiceError::PermissionDenied);
    }
    Err(ServiceError::BackendError(stderr.trim().to_string()))
}

async fn run_ps_action(cmd: &str, unit: &str) -> Result<(), ServiceError> {
    let out = Command::new("powershell")
        .args(["-NonInteractive", "-Command", cmd])
        .output()
        .await
        .map_err(|e| ServiceError::BackendError(e.to_string()))?;

    if out.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("Cannot find any service") || stderr.contains("not found") {
        return Err(ServiceError::NotFound(unit.to_string()));
    }
    if is_permission_denied(&stderr) {
        return Err(ServiceError::PermissionDenied);
    }
    Err(ServiceError::BackendError(stderr.trim().to_string()))
}

fn parse_status(status: u32) -> ServiceState {
    match status {
        1 => ServiceState::Stopped,
        2 => ServiceState::Starting,
        3 => ServiceState::Stopping,
        4 => ServiceState::Running,
        5 => ServiceState::Starting, // ContinuePending
        6 => ServiceState::Stopping, // PausePending
        7 => ServiceState::Paused,
        _ => ServiceState::Unknown,
    }
}

fn raw_status(status: u32) -> &'static str {
    match status {
        1 => "Stopped",
        2 => "StartPending",
        3 => "StopPending",
        4 => "Running",
        5 => "ContinuePending",
        6 => "PausePending",
        7 => "Paused",
        _ => "Unknown",
    }
}

fn is_enabled(start_type: u32) -> bool {
    // Automatic (2) or Boot (0) or System (1) → enabled at boot
    matches!(start_type, 0..=2)
}

fn record_to_service(r: ScmRecord) -> Service {
    let state = parse_status(r.status);
    Service {
        name: r.name,
        description: Some(r.display_name),
        state,
        raw_state: raw_status(r.status).to_string(),
        enabled_at_boot: Some(is_enabled(r.start_type)),
        backend: ServiceBackend::WindowsScm,
    }
}

// ===== ServiceManager impl =====

#[async_trait]
impl ServiceManager for WindowsScmManager {
    async fn list(&self, filter: ServiceFilter) -> Result<Vec<Service>, ServiceError> {
        // @(...) wrapper forces ConvertTo-Json to always output an array.
        let raw = run_ps(
            "@(Get-Service | Select-Object Name,DisplayName,Status,StartType) | ConvertTo-Json -Compress",
        )
        .await?;

        let records: Vec<ScmRecord> = serde_json::from_str(&raw)
            .map_err(|e| ServiceError::BackendError(format!("parse error: {}", e)))?;

        let services: Vec<Service> = records
            .into_iter()
            .map(record_to_service)
            .filter(|s| match &filter.state {
                Some(f) => &s.state == f,
                None => true,
            })
            .collect();

        Ok(services)
    }

    async fn get(&self, name: &str) -> Result<Service, ServiceError> {
        let cmd = format!(
            "@(Get-Service -Name '{}' | Select-Object Name,DisplayName,Status,StartType) | ConvertTo-Json -Compress",
            name.replace('\'', "''")
        );
        let raw = run_ps(&cmd).await?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ServiceError::NotFound(name.to_string()));
        }

        // PowerShell quirk: even with `@(...)` wrapping, `ConvertTo-Json`
        // collapses a single-element array to a bare object. We try the
        // array shape first (multi-match path) then fall back to a single
        // object decode. `-AsArray` would be cleaner but requires
        // PowerShell 7+, which is not universal on Windows Server / older
        // desktop installs.
        if let Ok(arr) = serde_json::from_str::<Vec<ScmRecord>>(trimmed) {
            return arr
                .into_iter()
                .next()
                .map(record_to_service)
                .ok_or_else(|| ServiceError::NotFound(name.to_string()));
        }
        let single: ScmRecord =
            serde_json::from_str(trimmed).map_err(|_| ServiceError::NotFound(name.to_string()))?;
        Ok(record_to_service(single))
    }

    async fn start(&self, name: &str) -> Result<(), ServiceError> {
        let cmd = format!("Start-Service -Name '{}'", name.replace('\'', "''"));
        run_ps_action(&cmd, name).await
    }

    async fn stop(&self, name: &str) -> Result<(), ServiceError> {
        let cmd = format!("Stop-Service -Name '{}'", name.replace('\'', "''"));
        run_ps_action(&cmd, name).await
    }

    async fn restart(&self, name: &str) -> Result<(), ServiceError> {
        let cmd = format!("Restart-Service -Name '{}'", name.replace('\'', "''"));
        run_ps_action(&cmd, name).await
    }

    async fn enable_at_boot(&self, name: &str) -> Result<(), ServiceError> {
        let cmd = format!(
            "Set-Service -Name '{}' -StartupType Automatic",
            name.replace('\'', "''")
        );
        run_ps_action(&cmd, name).await
    }

    async fn disable_at_boot(&self, name: &str) -> Result<(), ServiceError> {
        let cmd = format!(
            "Set-Service -Name '{}' -StartupType Disabled",
            name.replace('\'', "''")
        );
        run_ps_action(&cmd, name).await
    }

    async fn reload(&self, name: &str) -> Result<(), ServiceError> {
        // Windows SCM has no reload signal concept; return NotSupported
        // so callers can fall back to restart if needed.
        let _ = name;
        Err(ServiceError::NotSupported)
    }
}
