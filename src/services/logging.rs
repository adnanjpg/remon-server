//! Application logger that mirrors stdout output into a `logs` table for
//! later retrieval by the UI.
//!
//! Wiring (in `main.rs`):
//! 1. `LogService::new().set_level(filter).build()` → installs the
//!    env_logger pipeline and returns the receiver end of the in-memory
//!    channel that buffers log records.
//! 2. After the DB is connected, call `start_db_writer(rx, pool)` to spawn
//!    the consumer task that drains the channel into `LogRepository`.
//!
//! The consumer task **must not** call any `log::*!` macro itself — that
//! would feed records back through the pipe and risk an unbounded loop
//! when DB writes fail. Errors go straight to stderr.

use chrono::Local;
use colored::Colorize;
use log::error;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

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
    fn from_string(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "error" => LogLevel::Error,
            "warn" => LogLevel::Warn,
            "info" => LogLevel::Info,
            "debug" => LogLevel::Debug,
            "trace" => LogLevel::Trace,
            _ => LogLevel::Info,
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

pub struct LogService {
    channel: (mpsc::Sender<AppLog>, mpsc::Receiver<AppLog>),
    builder: env_logger::Builder,
}

struct CustomPipe {
    buffer_tx: Arc<Mutex<mpsc::Sender<AppLog>>>,
}

impl CustomPipe {
    fn new(tx: Arc<Mutex<mpsc::Sender<AppLog>>>) -> Self {
        CustomPipe { buffer_tx: tx }
    }
}

impl Write for CustomPipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let app_log = match serde_json::from_slice::<AppLog>(buf) {
            Ok(log) => log,
            Err(e) => {
                return Err(io::Error::new(io::ErrorKind::Other, e.to_string()));
            }
        };

        match self.buffer_tx.lock() {
            Ok(buffer_tx) => {
                if let Err(e) = buffer_tx.try_send(app_log) {
                    // Use stderr — log::error! here would create a feedback loop.
                    eprintln!("Failed to send log to buffer: {}", e);
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "Failed to send log to buffer",
                    ));
                }
            }
            Err(e) => {
                eprintln!("Failed to acquire lock on buffer_tx: {}", e);
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Failed to acquire lock on buffer_tx",
                ));
            }
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl LogService {
    pub fn new() -> Self {
        let mut this = LogService {
            channel: mpsc::channel::<AppLog>(100),
            builder: env_logger::builder(),
        };

        this.builder
            .target(env_logger::Target::Pipe(Box::new(CustomPipe::new(
                Arc::new(Mutex::new(this.channel.0.clone())),
            ))));

        this.builder.format(|buf, record| {
            fn log_level_to_colored(level: log::Level) -> colored::ColoredString {
                match level {
                    log::Level::Error => "ERROR".red(),
                    log::Level::Warn => "WARN".yellow(),
                    log::Level::Info => "INFO".bright_blue(),
                    log::Level::Debug => "DEBUG".bright_cyan(),
                    log::Level::Trace => "TRACE".white(),
                }
            }

            let dt = Local::now();
            let timestamp = dt.format("%Y-%m-%d %H:%M:%S").to_string();
            let level = format!("{:<5}", log_level_to_colored(record.level()));
            let target = record.target().bright_green();
            let msg = record.args();

            let app_log_threshold = get_log_insertion_level();
            if record.level() <= app_log_threshold {
                let app_log = AppLog {
                    id: -1,
                    log_level: LogLevel::from_string(record.level().as_str()),
                    app_id: get_app_name(),
                    logged_at: dt.timestamp(),
                    message: msg.to_string(),
                    target: record.target().to_owned(),
                };

                if let Err(e) = serde_json::to_writer(buf, &app_log) {
                    eprintln!("Failed to serialize log: {}", e);
                }
            }

            writeln!(io::stdout(), "{} {} [{}] {}", timestamp, level, target, msg)
        });

        this
    }

    pub fn set_level(mut self, level: log::LevelFilter) -> Self {
        self.builder.filter_level(level);
        self
    }

    /// Install the env_logger pipeline and return the receiver end of the
    /// in-memory channel. Callers should pass that receiver to
    /// `start_db_writer` once the database is ready.
    pub fn build(mut self) -> mpsc::Receiver<AppLog> {
        let _ = self.builder.try_init();
        self.channel.1
    }
}

/// Drain the log channel into the `logs` table. Spawned as a tokio task
/// after the DB pool is available. The task survives DB write failures —
/// it only dies when the channel closes (which happens when the program
/// drops the last sender, i.e. at shutdown).
pub fn start_db_writer(mut rx: mpsc::Receiver<AppLog>, pool: SqlitePool) {
    let repo = LogRepository::new(pool);
    tokio::spawn(async move {
        while let Some(app_log) = rx.recv().await {
            let level = app_log.log_level.as_i32();
            if let Err(e) = repo
                .insert(level, &app_log.app_id, &app_log.target, &app_log.message)
                .await
            {
                // stderr only — never `log::error!` here, that would loop.
                eprintln!("LogService: failed to persist log entry: {:?}", e);
            }
        }
    });
}

/// Get the app name from config, falls back to "remon" if unavailable.
fn get_app_name() -> String {
    match crate::config::Config::new() {
        Ok(config) => config.monitoring.app_name,
        Err(_) => "remon".to_string(),
    }
}

/// Get the log insertion level from config, falls back to Warn if unavailable.
///
/// Reading `Config::new()` here on every log line is wasteful but it can't
/// be replaced with `OnceLock` until the formatter has access to AppState
/// — tracked under a separate cleanup task.
fn get_log_insertion_level() -> log::Level {
    match crate::config::Config::new() {
        Ok(config) => match config
            .monitoring
            .log_insertion_level
            .to_lowercase()
            .as_str()
        {
            "error" => log::Level::Error,
            "warn" => log::Level::Warn,
            "info" => log::Level::Info,
            "debug" => log::Level::Debug,
            "trace" => log::Level::Trace,
            _ => log::Level::Warn,
        },
        Err(_) => log::Level::Warn,
    }
}

// avoid `unused_imports` for `error` after switching to eprintln in this module
#[allow(dead_code)]
fn _silence_unused_import_lint() {
    error!("never called");
}
