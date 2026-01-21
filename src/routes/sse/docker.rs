use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;

use crate::{
    api::extractors::Claims,
    monitor::docker_actions::{self, is_docker_available, DockerActionError},
};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseBody {
    Error(String),
}

/// Query params for stream logs
#[derive(Debug, Deserialize, Serialize)]
pub struct StreamLogsQuery {
    pub tail: Option<usize>,
}

/// GET /sse/docker/containers/{id}/logs/stream - Stream container logs via SSE
pub async fn stream_logs(
    _claims: Claims,
    Path(container_id): Path<String>,
    Query(params): Query<StreamLogsQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, Json<ResponseBody>)> {
    use futures_util::StreamExt;

    if !is_docker_available().await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ResponseBody::Error("Docker is not available".to_string())),
        ));
    }

    let log_stream = docker_actions::stream_container_logs(container_id, params.tail)
        .await
        .map_err(|e| match e {
            DockerActionError::ContainerNotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(ResponseBody::Error(e.to_string())),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ResponseBody::Error(e.to_string())),
            ),
        })?;

    // Map log stream to SSE events
    let sse_stream = log_stream.map(|result| {
        match result {
            Ok(log_line) => Ok(Event::default().data(log_line)),
            Err(e) => {
                // On error, send an error event and continue
                Ok(Event::default()
                    .event("error")
                    .data(format!("Error: {}", e)))
            }
        }
    });

    Ok(Sse::new(sse_stream).keep_alive(KeepAlive::default()))
}
