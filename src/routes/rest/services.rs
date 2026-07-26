use axum::{
    Json,
    extract::{Path, Query, State},
};
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::platform::services::ServiceFilter;
use crate::routes::dtos::services::{
    ListServicesQuery, ListServicesResponse, ListTimersResponse, ServiceActionResponse, ServiceDto,
    TimerDto,
};
use crate::routes::extractors::Claims;
use crate::services::events;
use crate::state::AppState;

/// Ledger entry for a completed unit action — one audit row per mutating
/// endpoint, attributed to the calling device. The response message doubles
/// as the event message.
fn audit(
    state: &Arc<AppState>,
    claims: &Claims,
    ref_type: &'static str,
    name: &str,
    action: &'static str,
    message: &str,
) {
    events::record_operator(
        state,
        &claims.device_id,
        "service_action",
        message.to_string(),
        Some(ref_type),
        Some(name.to_string()),
        Some(serde_json::json!({ "action": action })),
    );
}

/// Defense-in-depth: validate the path-param name BEFORE handing it to a
/// shell-out backend. Both systemd unit names and Windows service names
/// fit comfortably in `[A-Za-z0-9._@:-]`; anything outside that window is
/// almost certainly an injection probe (`;`, `\``, `$()`, quotes, …) and
/// rejecting it here means our shell-quoting bugs (if any) can't be reached.
/// Empty names and >256-char names are also rejected.
/// Refuse an action that would take this server down through the generic unit
/// endpoint. `systemctl stop` is not something the supervisor undoes, so a
/// self-stop here is permanent and silent — and the caller reaching for
/// `/services/{name}/stop` is working from a unit list, not deciding to switch
/// monitoring off. The deliberate versions live at `/system/restart` and
/// `/system/shutdown`, which say what they do and record it as such.
///
/// `start`, `enable` and `reload` are left alone: none of them can end the
/// process, and enabling ourselves at boot is a reasonable thing to ask for.
fn reject_self(name: &str, alternative: &str) -> AppResult<()> {
    if crate::platform::identity::is_own_service(name) {
        return Err(AppError::Conflict(format!(
            "'{name}' is remon-server itself; use {alternative}"
        )));
    }
    Ok(())
}

fn validate_name(name: &str) -> AppResult<()> {
    if name.is_empty() || name.len() > 256 {
        return Err(AppError::BadRequest(
            "service name must be 1–256 characters".to_string(),
        ));
    }
    let ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | ':'));
    if !ok {
        return Err(AppError::BadRequest(
            "service name may only contain alphanumerics and `._-@:`".to_string(),
        ));
    }
    Ok(())
}

// ===== Services =====

/// GET /services?state=<filter> — list all service units.
pub async fn list_services(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListServicesQuery>,
) -> AppResult<Json<ListServicesResponse>> {
    let filter = ServiceFilter {
        state: params.filter_state(),
    };
    let services = state.service_manager.list(filter).await?;
    Ok(Json(ListServicesResponse {
        services: services.into_iter().map(ServiceDto::from).collect(),
    }))
}

/// GET /services/{name} — single service status.
pub async fn get_service(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceDto>> {
    validate_name(&name)?;
    let service = state.service_manager.get(&name).await?;
    Ok(Json(ServiceDto::from(service)))
}

/// POST /services/{name}/start
pub async fn start_service(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    state.service_manager.start(&name).await?;
    let msg = format!("Service '{}' started", name);
    audit(&state, &claims, "service", &name, "start", &msg);
    Ok(Json(ServiceActionResponse::ok(msg)))
}

/// POST /services/{name}/stop
pub async fn stop_service(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    reject_self(&name, "POST /system/shutdown")?;
    state.service_manager.stop(&name).await?;
    let msg = format!("Service '{}' stopped", name);
    audit(&state, &claims, "service", &name, "stop", &msg);
    Ok(Json(ServiceActionResponse::ok(msg)))
}

/// POST /services/{name}/restart
pub async fn restart_service(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    reject_self(&name, "POST /system/restart")?;
    state.service_manager.restart(&name).await?;
    let msg = format!("Service '{}' restarted", name);
    audit(&state, &claims, "service", &name, "restart", &msg);
    Ok(Json(ServiceActionResponse::ok(msg)))
}

/// POST /services/{name}/reload
pub async fn reload_service(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    state.service_manager.reload(&name).await?;
    let msg = format!("Service '{}' reloaded", name);
    audit(&state, &claims, "service", &name, "reload", &msg);
    Ok(Json(ServiceActionResponse::ok(msg)))
}

/// PUT /services/{name}/enable — enable service at boot.
pub async fn enable_service(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    state.service_manager.enable_at_boot(&name).await?;
    let msg = format!("Service '{}' enabled at boot", name);
    audit(&state, &claims, "service", &name, "enable", &msg);
    Ok(Json(ServiceActionResponse::ok(msg)))
}

/// PUT /services/{name}/disable — disable service at boot.
pub async fn disable_service(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    // Survives a reboot, so the damage outlives the request: the host comes
    // back with no agent and nothing to report that it is missing.
    reject_self(&name, "POST /system/shutdown to stop monitoring this host")?;
    state.service_manager.disable_at_boot(&name).await?;
    let msg = format!("Service '{}' disabled at boot", name);
    audit(&state, &claims, "service", &name, "disable", &msg);
    Ok(Json(ServiceActionResponse::ok(msg)))
}

// ===== Timers =====

/// GET /timers — list all systemd timer units.
pub async fn list_timers(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ListTimersResponse>> {
    let timers = state.service_manager.list_timers().await?;
    Ok(Json(ListTimersResponse {
        timers: timers.into_iter().map(TimerDto::from).collect(),
    }))
}

/// PUT /timers/{name}/enable
pub async fn enable_timer(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    state.service_manager.enable_timer(&name).await?;
    let msg = format!("Timer '{}' enabled", name);
    audit(&state, &claims, "timer", &name, "enable", &msg);
    Ok(Json(ServiceActionResponse::ok(msg)))
}

/// PUT /timers/{name}/disable
pub async fn disable_timer(
    claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    state.service_manager.disable_timer(&name).await?;
    let msg = format!("Timer '{}' disabled", name);
    audit(&state, &claims, "timer", &name, "disable", &msg);
    Ok(Json(ServiceActionResponse::ok(msg)))
}
