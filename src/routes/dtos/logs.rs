//! REST DTOs for the application-log read endpoint.

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    pub start: Option<i64>,
    pub end: Option<i64>,
    /// Minimum severity to include: "error" | "warn" | "info" | "debug" |
    /// "trace". Defaults to "trace" (everything persisted). Note the table
    /// only ever holds what `monitoring.log_insertion_level` let through.
    pub level: Option<String>,
    /// Hard cap on entries returned. Defaults to 500, max 5000.
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct LogEntry {
    pub id: i64,
    pub timestamp: i64,
    /// "error" | "warn" | "info" | "debug" | "trace".
    pub level: String,
    /// Emitting app id (this daemon's identifier).
    pub source: String,
    /// Rust module path of the emitting call site.
    pub target: String,
    pub message: String,
}

/// Entries are sorted newest-first.
#[derive(Debug, Serialize)]
pub struct LogsResponse {
    pub entries: Vec<LogEntry>,
}
