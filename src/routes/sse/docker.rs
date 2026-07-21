#![cfg(feature = "docker")]

use axum::{
    extract::{Path, Query, State},
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::stream::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::routes::extractors::Claims;
use crate::routes::sse::until_shutdown;
use crate::services::docker;
use crate::state::AppState;

/// Query params for stream logs
#[derive(Debug, Deserialize, Serialize)]
pub struct StreamLogsQuery {
    pub tail: Option<usize>,
}

/// GET /sse/docker/containers/{id}/logs/stream — stream container logs via SSE.
pub async fn stream_logs(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Path(container_id): Path<String>,
    Query(params): Query<StreamLogsQuery>,
) -> AppResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    if !docker::is_docker_available().await {
        return Err(AppError::DockerUnavailable(
            "daemon unreachable".to_string(),
        ));
    }

    let log_stream = docker::stream_container_logs(container_id, params.tail).await?;

    let sse_stream = log_stream.map(|result| match result {
        Ok(log_line) => Ok(Event::default().data(log_line)),
        Err(e) => Ok(Event::default()
            .event("error")
            .data(format!("Error: {}", e))),
    });

    let sse_stream = until_shutdown(sse_stream, state.shutdown.subscribe());
    Ok(Sse::new(sse_stream).keep_alive(KeepAlive::default()))
}
