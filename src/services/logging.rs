use chrono::Local;
use colored::Colorize;
use log::{debug, error};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

#[derive(Clone, Serialize, Deserialize)]
struct AppLog {
    id: i32,
    log_level: LogLevel,
    app_id: String,
    logged_at: i64,
    message: String,
    target: String,
}

#[derive(Clone, Serialize, Deserialize)]
enum LogLevel {
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
}

pub struct LogService {
    #[allow(dead_code)]
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
                    error!("Failed to send log to buffer: {}", e);
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "Failed to send log to buffer",
                    ));
                }
            }
            Err(e) => {
                error!("Failed to acquire lock on buffer_tx: {}", e);
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

                match serde_json::to_writer(buf, &app_log) {
                    Ok(_) => {}
                    Err(e) => {
                        error!("Failed to serialize log: {}", e);
                    }
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

    pub fn build(mut self) {
        let _ = self.builder.try_init();

        let _ = tokio::spawn(async move {
            while let Some(app_log) = self.channel.1.recv().await {
                debug!("AppLog received '{}'", app_log.message);
            }
        });
    }
}

/// Get the app name from config, falls back to "remon" if unavailable.
fn get_app_name() -> String {
    match crate::config::Config::new() {
        Ok(config) => config.monitoring.app_name,
        Err(_) => "remon".to_string(),
    }
}

/// Get the log insertion level from config, falls back to Warn if unavailable.
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