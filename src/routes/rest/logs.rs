//! Application-log read endpoint — serves what `DbLayer` persisted into
//! the `logs` table, so the daemon's own behaviour is inspectable from a
//! client without shell access to journald/stdout.

use axum::{
    Json,
    extract::{Query, State},
};
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::routes::dtos::logs::{LogEntry, LogsQuery, LogsResponse};
use crate::routes::extractors::Claims;
use crate::state::AppState;
use crate::storage::repositories::LogRepository;

/// Default span when client omits start/end: last 24 hours. Logs are far
/// sparser than metrics, so a day is a sane first page.
const DEFAULT_SPAN_SECS: i64 = 86_400;
const DEFAULT_LIMIT: u32 = 500;
const MAX_LIMIT: u32 = 5000;

fn level_to_i32(s: &str) -> Option<i32> {
    match s {
        "error" => Some(1),
        "warn" => Some(2),
        "info" => Some(3),
        "debug" => Some(4),
        "trace" => Some(5),
        _ => None,
    }
}

fn level_to_str(level: i32) -> &'static str {
    match level {
        1 => "error",
        2 => "warn",
        3 => "info",
        4 => "debug",
        _ => "trace",
    }
}

/// GET /logs — newest-first application log entries.
pub async fn list_logs(
    _claims: Claims,
    State(state): State<Arc<AppState>>,
    Query(q): Query<LogsQuery>,
) -> AppResult<Json<LogsResponse>> {
    let now = chrono::Utc::now().timestamp();
    let end = q.end.unwrap_or(now);
    let start = q.start.unwrap_or(end - DEFAULT_SPAN_SECS);
    if end < start {
        return Err(AppError::BadRequest("end must be >= start".to_string()));
    }

    let max_level = match q.level.as_deref() {
        None => 5,
        Some(s) => level_to_i32(s).ok_or_else(|| {
            AppError::BadRequest(format!(
                "unknown level '{}'; expected error|warn|info|debug|trace",
                s
            ))
        })?,
    };

    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);

    let repo = LogRepository::new(state.db.clone());
    let rows = repo.list(max_level, start, end, limit).await?;

    let entries = rows
        .into_iter()
        .map(|r| LogEntry {
            id: r.id,
            timestamp: r.timestamp,
            level: level_to_str(r.level).to_string(),
            source: r.source,
            target: r.target,
            message: r.message,
        })
        .collect();

    Ok(Json(LogsResponse { entries }))
}
