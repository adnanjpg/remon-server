use std::collections::HashSet;

use async_trait::async_trait;
use tokio::process::Command;

use super::{Service, ServiceBackend, ServiceError, ServiceFilter, ServiceManager, ServiceState};

pub struct OpenRcManager;

// ===== Internal helpers =====

async fn run_rc(args: &[&str]) -> Result<String, ServiceError> {
    let out = Command::new(args[0])
        .args(&args[1..])
        .output()
        .await
        .map_err(|e| ServiceError::BackendError(format!("exec failed: {}", e)))?;

    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("ermission") || out.status.code() == Some(126) {
        return Err(ServiceError::PermissionDenied);
    }
    // rc-service exits 1 when service is stopped; that is not an error for status checks.
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn run_rc_action(args: &[&str], unit: &str) -> Result<(), ServiceError> {
    let out = Command::new(args[0])
        .args(&args[1..])
        .output()
        .await
        .map_err(|e| ServiceError::BackendError(e.to_string()))?;

    if out.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{}{}", stderr, stdout);

    if combined.contains("does not exist") || combined.contains("not found") {
        return Err(ServiceError::NotFound(unit.to_string()));
    }
    if combined.contains("ermission") || out.status.code() == Some(126) {
        return Err(ServiceError::PermissionDenied);
    }
    Err(ServiceError::BackendError(combined.trim().to_string()))
}

/// Parse `rc-status --all --nocolor` into a list of (name, state) pairs.
/// Lines look like:
///   " nginx                                                    [  started  ]"
fn parse_rc_status(output: &str) -> Vec<(String, ServiceState)> {
    output
        .lines()
        .filter_map(|line| {
            // Skip runlevel headers and empty lines.
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with("Runlevel:")
                || trimmed.starts_with("Dynamic Runlevel:")
            {
                return None;
            }
            // Expect a "[" separator.
            let bracket = line.find('[')?;
            let name = line[..bracket].trim().to_string();
            if name.is_empty() {
                return None;
            }
            let rest = &line[bracket..];
            let state_str = rest
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_string();
            let state = parse_openrc_state(&state_str);
            Some((name, state))
        })
        .collect()
}

fn parse_openrc_state(s: &str) -> ServiceState {
    match s {
        "started" => ServiceState::Running,
        "stopped" => ServiceState::Stopped,
        "starting" => ServiceState::Starting,
        "stopping" => ServiceState::Stopping,
        "crashed" => ServiceState::Failed,
        "inactive" => ServiceState::Stopped,
        _ => ServiceState::Unknown,
    }
}

/// Parse `rc-update show` to get the set of services enabled in any runlevel.
/// Output lines: " nginx | default "
async fn enabled_services() -> HashSet<String> {
    let Ok(raw) = run_rc(&["rc-update", "show"]).await else {
        return HashSet::new();
    };
    raw.lines()
        .filter_map(|line| {
            let pipe = line.find('|')?;
            Some(line[..pipe].trim().to_string())
        })
        .collect()
}

// ===== ServiceManager impl =====

#[async_trait]
impl ServiceManager for OpenRcManager {
    async fn list(&self, filter: ServiceFilter) -> Result<Vec<Service>, ServiceError> {
        let raw = run_rc(&["rc-status", "--all", "--nocolor"]).await?;
        let enabled = enabled_services().await;
        let pairs = parse_rc_status(&raw);

        let services: Vec<Service> = pairs
            .into_iter()
            .map(|(name, state)| {
                let enabled_at_boot = Some(enabled.contains(&name));
                Service {
                    name: name.clone(),
                    description: None,
                    state: state.clone(),
                    raw_state: format!("{:?}", state).to_lowercase(),
                    enabled_at_boot,
                    backend: ServiceBackend::OpenRc,
                }
            })
            .filter(|s| match &filter.state {
                Some(f) => &s.state == f,
                None => true,
            })
            .collect();

        Ok(services)
    }

    async fn get(&self, name: &str) -> Result<Service, ServiceError> {
        let raw = run_rc(&["rc-service", name, "status"]).await?;

        // rc-service status prints " * status: started" or " * status: stopped"
        let state_str = raw
            .lines()
            .find_map(|l| l.trim().strip_prefix("* status:").map(|s| s.trim().to_string()))
            .unwrap_or_else(|| "unknown".to_string());

        if state_str.contains("does not exist") || state_str.contains("not found") {
            return Err(ServiceError::NotFound(name.to_string()));
        }

        let state = parse_openrc_state(&state_str);
        let enabled = enabled_services().await;

        Ok(Service {
            name: name.to_string(),
            description: None,
            state: state.clone(),
            raw_state: state_str,
            enabled_at_boot: Some(enabled.contains(name)),
            backend: ServiceBackend::OpenRc,
        })
    }

    async fn start(&self, name: &str) -> Result<(), ServiceError> {
        run_rc_action(&["rc-service", name, "start"], name).await
    }

    async fn stop(&self, name: &str) -> Result<(), ServiceError> {
        run_rc_action(&["rc-service", name, "stop"], name).await
    }

    async fn restart(&self, name: &str) -> Result<(), ServiceError> {
        run_rc_action(&["rc-service", name, "restart"], name).await
    }

    async fn enable_at_boot(&self, name: &str) -> Result<(), ServiceError> {
        run_rc_action(&["rc-update", "add", name, "default"], name).await
    }

    async fn disable_at_boot(&self, name: &str) -> Result<(), ServiceError> {
        run_rc_action(&["rc-update", "del", name, "default"], name).await
    }
}
