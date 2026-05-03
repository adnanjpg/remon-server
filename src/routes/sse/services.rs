use axum::{
    extract::{Path, Query},
    response::Response,
};
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::routes::extractors::Claims;

#[derive(Debug, Deserialize)]
pub struct StreamServiceLogsQuery {
    /// Number of historical lines to include before following. Defaults to 50.
    pub tail: Option<u64>,
}

/// GET /sse/services/{name}/logs — stream a service's journal entries via SSE.
///
/// Spawns `journalctl -fu <unit> -n <tail>` and pipes each line as an SSE
/// data event. Only available on Linux systems with systemd/journald.
///
/// Return type is `Response` rather than the parameterised `Sse<...>` —
/// `Sse::keep_alive` wraps the inner stream type, which makes the
/// concrete return type harder to express, and the cfg-gated 501 branch
/// can't synthesise a stream value to satisfy `impl Stream` inference.
/// `into_response()` collapses both shapes into a single `Response`.
pub async fn stream_service_logs(
    _claims: Claims,
    Path(name): Path<String>,
    Query(params): Query<StreamServiceLogsQuery>,
) -> AppResult<Response> {
    // Same defense-in-depth charset check as the REST handlers — keeps
    // shell-meta out of the journalctl argv before we even spawn the child.
    if name.is_empty()
        || name.len() > 256
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | ':'))
    {
        return Err(AppError::BadRequest(
            "service name may only contain alphanumerics and `._-@:`".to_string(),
        ));
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (name, params);
        return Err(AppError::NotSupported);
    }

    #[cfg(target_os = "linux")]
    {
        use axum::response::IntoResponse;
        use axum::response::sse::{Event, KeepAlive, Sse};
        use crate::platform::services::normalize_unit_name;
        use futures_util::StreamExt;
        use std::process::Stdio;
        use tokio::io::AsyncBufReadExt;
        use tokio_stream::wrappers::LinesStream;

        let unit = normalize_unit_name(&name, "service");
        let tail = params.tail.unwrap_or(50).to_string();

        let mut child = tokio::process::Command::new("journalctl")
            .args(["-fu", &unit, "--output=short-precise", "--no-pager", "-n", &tail])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| AppError::Internal(format!("journalctl spawn failed: {}", e)))?;

        let stdout = child.stdout.take().ok_or_else(|| {
            AppError::Internal("journalctl stdout not captured".to_string())
        })?;
        let reader = tokio::io::BufReader::new(stdout);
        let stream = LinesStream::new(reader.lines()).map(|line| match line {
            Ok(l) => Ok::<_, std::convert::Infallible>(Event::default().data(l)),
            Err(e) => Ok(Event::default().event("error").data(e.to_string())),
        });

        Ok(Sse::new(stream).keep_alive(KeepAlive::default()).into_response())
    }
}
