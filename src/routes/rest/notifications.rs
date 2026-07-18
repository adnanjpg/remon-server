use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::notify::url_policy;
use crate::routes::dtos::notifications::{
    ChannelResponse, CreateChannelRequest, ListChannelsResponse, TestChannelResponse,
    UpdateChannelRequest,
};
use crate::routes::extractors::Claims;
use crate::state::AppState;
use crate::storage::repositories::NotificationChannelRepository;

/// SSRF validation for channels with an operator-supplied outbound URL
/// (webhook + ntfy). Run before DB insert / update so a channel that would be
/// blocked at send time never gets persisted. Channels with no operator URL
/// (fcm, web-push, telegram) are a no-op here.
async fn validate_channel_url(
    state: &AppState,
    channel_type: &str,
    config: &serde_json::Value,
) -> AppResult<()> {
    let Some(url) = url_policy::channel_check_url(channel_type, config) else {
        return Ok(());
    };
    if channel_type == "webhook" && url.is_empty() {
        return Err(AppError::BadRequest(
            "webhook channel config requires non-empty 'url'".to_string(),
        ));
    }
    let policy = state.notify.webhook_policy();
    url_policy::check_url(&url, &policy)
        .await
        .map_err(AppError::BadRequest)
}

fn to_response(row: crate::storage::repositories::NotificationChannelRow) -> ChannelResponse {
    ChannelResponse {
        id: row.id,
        name: row.name,
        r#type: row.r#type,
        enabled: row.enabled,
        config: serde_json::from_str(&row.config).unwrap_or_default(),
        min_severity: row.min_severity,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

pub async fn list_channels(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<ListChannelsResponse>> {
    let repo = NotificationChannelRepository::new(state.db.clone());
    let rows = repo.list_all().await?;
    Ok(Json(ListChannelsResponse {
        channels: rows.into_iter().map(to_response).collect(),
    }))
}

pub async fn create_channel(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateChannelRequest>,
) -> AppResult<(StatusCode, Json<ChannelResponse>)> {
    body.validate().map_err(AppError::BadRequest)?;
    validate_channel_url(&state, &body.r#type, &body.config).await?;

    let config_str =
        serde_json::to_string(&body.config).map_err(|e| AppError::BadRequest(e.to_string()))?;

    let repo = NotificationChannelRepository::new(state.db.clone());
    let id = repo
        .insert(
            &body.name,
            &body.r#type,
            body.enabled,
            &config_str,
            body.min_severity.as_deref(),
        )
        .await?;

    state.notify.reload().await;

    let row = repo
        .get(id)
        .await?
        .ok_or(AppError::NotFound("notification channel".to_string()))?;
    Ok((StatusCode::CREATED, Json(to_response(row))))
}

pub async fn update_channel(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateChannelRequest>,
) -> AppResult<Json<ChannelResponse>> {
    body.validate().map_err(AppError::BadRequest)?;

    let repo = NotificationChannelRepository::new(state.db.clone());
    let existing = repo
        .get(id)
        .await?
        .ok_or_else(|| AppError::NotFound("notification channel".to_string()))?;

    // Type is immutable on update — re-validate against the stored type.
    validate_channel_url(&state, &existing.r#type, &body.config).await?;

    let config_str =
        serde_json::to_string(&body.config).map_err(|e| AppError::BadRequest(e.to_string()))?;

    let found = repo
        .update(
            id,
            &body.name,
            body.enabled,
            &config_str,
            body.min_severity.as_deref(),
        )
        .await?;

    if !found {
        return Err(AppError::NotFound("notification channel".to_string()));
    }

    state.notify.reload().await;

    let row = repo
        .get(id)
        .await?
        .ok_or(AppError::NotFound("notification channel".to_string()))?;
    Ok(Json(to_response(row)))
}

pub async fn delete_channel(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let repo = NotificationChannelRepository::new(state.db.clone());
    let found = repo.delete(id).await?;

    if !found {
        return Err(AppError::NotFound("notification channel".to_string()));
    }

    state.notify.reload().await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn test_channel(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> AppResult<Json<TestChannelResponse>> {
    // Verify the channel exists in DB first.
    let repo = NotificationChannelRepository::new(state.db.clone());
    repo.get(id)
        .await?
        .ok_or(AppError::NotFound("notification channel".to_string()))?;

    let server_name = state.effective_config.read().await.server_name.clone();
    let delivered = state
        .notify
        .test_channel(id, &server_name)
        .await
        .map_err(AppError::BadRequest)?;

    Ok(Json(TestChannelResponse { delivered }))
}
