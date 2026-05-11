//! Application logger — persists app-emitted events into the `logs`
//! table for the in-UI log viewer.
//!
//! Architecture (since 0.7.4 — replaced the env_logger + CustomPipe
//! design):
//! 1. `tracing-subscriber` is the single logging backend. The
//!    `tracing-log` feature bridges every `log::*!` macro call into a
//!    tracing event, so the 27 source files using `log::info!` /
//!    `warn!` / `error!` keep working unchanged.
//! 2. `DbLayer` is a `tracing_subscriber::Layer` registered alongside
//!    the stdout fmt layer. It runs its own per-target filter so that
//!    third-party crate noise (sqlx queries, hyper internals) never
//!    lands in the `logs` table — only `remon_server::*` events do.
//! 3. `start_db_writer(rx, pool)` drains the channel into
//!    `LogRepository`. Spawned after the DB is connected; write
//!    failures go to **stderr only** — emitting a `log::*!` or
//!    `tracing::*!` here would feed back through DbLayer and loop.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

use crate::storage::repositories::LogRepository;

#[derive(Clone, Serialize, Deserialize)]
pub struct AppLog {
    pub id: i32,
    pub log_level: LogLevel,
    pub app_id: String,
    pub logged_at: i64,
    pub message: String,
    pub target: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn from_tracing(level: &Level) -> Self {
        match *level {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            Level::INFO => LogLevel::Info,
            Level::DEBUG => LogLevel::Debug,
            Level::TRACE => LogLevel::Trace,
        }
    }

    fn as_i32(&self) -> i32 {
        match self {
            LogLevel::Error => 1,
            LogLevel::Warn => 2,
            LogLevel::Info => 3,
            LogLevel::Debug => 4,
            LogLevel::Trace => 5,
        }
    }
}

/// tracing-subscriber Layer that persists app-emitted events into the
/// `logs` table.
///
/// The layer's filter is intentionally INDEPENDENT of the global
/// EnvFilter:
/// - it only matches the `remon_server` target prefix (so sqlx / hyper
///   noise never gets stored), and
/// - it only persists events at or above `min_persist_level`, which
///   tracks `monitoring.log_insertion_level` from config.
///
/// The composition lets operators run the stdout layer at `debug` for
/// diagnostics without flooding the database in parallel.
pub struct DbLayer {
    sender: mpsc::Sender<AppLog>,
    /// Lowest-verbosity level we persist (e.g. `WARN` keeps WARN+ERROR).
    /// `tracing::Level` orders by VERBOSITY, so "more verbose than this"
    /// means `level > min`. We persist if `level <= min`.
    min_persist_level: Level,
    app_id: Arc<String>,
}

impl DbLayer {
    pub fn new(sender: mpsc::Sender<AppLog>, min_persist_level: Level, app_id: String) -> Self {
        Self {
            sender,
            min_persist_level,
            app_id: Arc::new(app_id),
        }
    }
}

impl<S: Subscriber> Layer<S> for DbLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();

        if *metadata.level() > self.min_persist_level {
            return;
        }

        // We need both the message and (for bridged `log::*!` events)
        // the real target, which the `tracing-log` bridge stuffs into
        // a `log.target` field rather than the event's metadata —
        // metadata.target() for bridged events is the literal string
        // "log". Collect both up-front.
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);

        // Effective target: prefer the bridged `log.target` field when
        // present, otherwise fall back to the event's own metadata
        // target (used by direct `tracing::*!` call sites).
        let effective_target = visitor
            .log_target
            .as_deref()
            .unwrap_or_else(|| metadata.target());

        // Only persist events from our own crate. Third-party tracing
        // events (sqlx query bodies, hyper retries, etc.) belong in
        // stdout/journald — not the in-app log table.
        if !effective_target.starts_with("remon_server") {
            return;
        }

        let app_log = AppLog {
            id: -1,
            log_level: LogLevel::from_tracing(metadata.level()),
            app_id: (*self.app_id).clone(),
            logged_at: chrono::Utc::now().timestamp(),
            message: visitor.message.unwrap_or_default(),
            target: effective_target.to_owned(),
        };

        // try_send — drop if the buffer is full. We never want a log
        // emission to block the calling task; a saturated channel means
        // the DB writer is stuck and is already complaining to stderr.
        let _ = self.sender.try_send(app_log);
    }
}

/// Pulls the two fields we care about off a tracing Event:
/// - `message`: the formatted log line
/// - `log.target`: present only on events bridged from the `log` crate
///   by `tracing-log` — preserves the original module path that the
///   bridge would otherwise hide behind a literal `"log"` metadata
///   target.
#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    log_target: Option<String>,
}

impl Visit for EventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message = Some(value.to_string()),
            "log.target" => self.log_target = Some(value.to_string()),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => self.message = Some(format!("{:?}", value)),
            "log.target" => {
                self.log_target = Some(format!("{:?}", value).trim_matches('"').to_string())
            }
            _ => {}
        }
    }
}

/// Drain the log channel into the `logs` table. Spawned as a tokio task
/// after the DB pool is available.
pub fn start_db_writer(mut rx: mpsc::Receiver<AppLog>, pool: SqlitePool) {
    let repo = LogRepository::new(pool);
    tokio::spawn(async move {
        while let Some(app_log) = rx.recv().await {
            let level = app_log.log_level.as_i32();
            if let Err(e) = repo
                .insert(level, &app_log.app_id, &app_log.target, &app_log.message)
                .await
            {
                // stderr only — emitting a `log::*!` or `tracing::*!` here
                // would feed back through DbLayer and loop.
                eprintln!("LogService: failed to persist log entry: {:?}", e);
            }
        }
    });
}

/// Map the `monitoring.log_insertion_level` config string to a tracing Level.
/// Falls back to WARN on unrecognized input — the same conservative default
/// the env_logger-based predecessor used.
pub fn parse_persist_level(s: &str) -> Level {
    match s.to_lowercase().as_str() {
        "error" => Level::ERROR,
        "warn" => Level::WARN,
        "info" => Level::INFO,
        "debug" => Level::DEBUG,
        "trace" => Level::TRACE,
        _ => Level::WARN,
    }
}
