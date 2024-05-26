use chrono::Local;
use colored::Colorize;
use log::error;
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

use crate::logs::persistence::{insert_app_log, AppLog, LogLevel};

pub struct LogService {
    #[allow(dead_code)]
    channel: (mpsc::Sender<AppLog>, mpsc::Receiver<AppLog>),
    builder: env_logger::Builder,
}

pub struct CustomPipe {
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
        // Create a buffer channel for asynchronous processing
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

            // TODO(@isaidsari): AppLog instertion level should be configurable
            const APP_LOG_THRESHOLD: log::Level = log::Level::Warn;
            if record.level() <= APP_LOG_THRESHOLD {
                let app_log = AppLog {
                    id: -1,
                    log_level: LogLevel::from_string(record.level().as_str()),
                    app_id: DEF_APP_NAME.to_owned(),
                    logged_at: dt.timestamp(),
                    message: msg.to_string(),
                    target: record.target().to_owned(),
                };

                // binary serialization would be better
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
        self.builder.init();

        let _ = tokio::spawn(async move {
            while let Some(app_log) = self.channel.1.recv().await {
                println!("app log received '{}'", app_log.message);
                insert_app_log(&app_log)
                    .await
                    .expect("Failed to insert app log");
            }
        });
    }
}

// TODO(adnanjpg): make this configurable
const DEF_APP_NAME: &str = "remon";
