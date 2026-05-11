use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::routes::dtos::notifications::{
    ChannelResponse, CreateChannelRequest, ListChannelsResponse, TestChannelResponse,
    UpdateChannelRequest,
};
use crate::routes::extractors::Claims;
use crate::state::AppState;
use crate::storage::repositories::NotificationChannelRepository;

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

    let config_str =
        serde_json::to_string(&body.config).map_err(|e| AppError::BadRequest(e.to_string()))?;

    let repo = NotificationChannelRepository::new(state.db.clone());
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

    let delivered = state
        .notify
        .test_channel(id)
        .await
        .map_err(AppError::BadRequest)?;

    Ok(Json(TestChannelResponse { delivered }))
}
