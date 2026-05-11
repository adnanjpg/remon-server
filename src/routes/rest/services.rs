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
use crate::state::AppState;

/// Defense-in-depth: validate the path-param name BEFORE handing it to a
/// shell-out backend. Both systemd unit names and Windows service names
/// fit comfortably in `[A-Za-z0-9._@:-]`; anything outside that window is
/// almost certainly an injection probe (`;`, `\``, `$()`, quotes, …) and
/// rejecting it here means our shell-quoting bugs (if any) can't be reached.
/// Empty names and >256-char names are also rejected.
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
        state: params.into_filter_state(),
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
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    state.service_manager.start(&name).await?;
    Ok(Json(ServiceActionResponse::ok(format!(
        "Service '{}' started",
        name
    ))))
}

/// POST /services/{name}/stop
pub async fn stop_service(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    state.service_manager.stop(&name).await?;
    Ok(Json(ServiceActionResponse::ok(format!(
        "Service '{}' stopped",
        name
    ))))
}

/// POST /services/{name}/restart
pub async fn restart_service(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    state.service_manager.restart(&name).await?;
    Ok(Json(ServiceActionResponse::ok(format!(
        "Service '{}' restarted",
        name
    ))))
}

/// POST /services/{name}/reload
pub async fn reload_service(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    state.service_manager.reload(&name).await?;
    Ok(Json(ServiceActionResponse::ok(format!(
        "Service '{}' reloaded",
        name
    ))))
}

/// PUT /services/{name}/enable — enable service at boot.
pub async fn enable_service(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    state.service_manager.enable_at_boot(&name).await?;
    Ok(Json(ServiceActionResponse::ok(format!(
        "Service '{}' enabled at boot",
        name
    ))))
}

/// PUT /services/{name}/disable — disable service at boot.
pub async fn disable_service(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    state.service_manager.disable_at_boot(&name).await?;
    Ok(Json(ServiceActionResponse::ok(format!(
        "Service '{}' disabled at boot",
        name
    ))))
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
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    state.service_manager.enable_at_boot(&name).await?;
    Ok(Json(ServiceActionResponse::ok(format!(
        "Timer '{}' enabled",
        name
    ))))
}

/// PUT /timers/{name}/disable
pub async fn disable_timer(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> AppResult<Json<ServiceActionResponse>> {
    validate_name(&name)?;
    state.service_manager.disable_at_boot(&name).await?;
    Ok(Json(ServiceActionResponse::ok(format!(
        "Timer '{}' disabled",
        name
    ))))
}
