use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use chrono::Local;
use log::error;
use tokio::sync::mpsc;

use crate::logs::persistence::{insert_app_log, AppLog, LogLevel};

pub struct LogService {
    sender: mpsc::Sender<AppLog>,
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
        let log_str = String::from_utf8_lossy(buf);
        let log_str = log_str.trim_end();

        // extract log level, target, and message format is
        // TODO(isaidsari): this isnt the way
        let log_parts: Vec<&str> = log_str.splitn(4, " ").collect();
        let log_level = log_parts[2];

        io::stdout()
            .write_all(format!("{:?}\n", log_level).as_bytes())
            .unwrap();

        let app_log = AppLog {
            id: -1,
            log_level: LogLevel::from_string(log_level),
            app_id: DEF_APP_NAME.to_owned(),
            logged_at: Local::now().timestamp(),
            message: log_str.to_string(),
            target: "stdout".to_owned(),
        };

        let buffer_tx = self.buffer_tx.clone();
        if app_log.log_level >= LogLevel::Warning {
            if let Err(e) = buffer_tx.lock().unwrap().try_send(app_log) {
                error!("Failed to send log to buffer: {}", e);
            }
        }

        //io::stdout().write_all(buf).unwrap();
        //writeln!(io::stdout(), "{}", log_str).unwrap();

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl LogService {
    pub fn new() -> Self {
        // Create a buffer channel for asynchronous processing
        let (buffer_tx, mut buffer_rx) = mpsc::channel::<AppLog>(100);
        let this = LogService {
            sender: mpsc::Sender::clone(&buffer_tx),
        };

        let mut builder = env_logger::builder();

        builder.target(env_logger::Target::Pipe(Box::new(CustomPipe::new(
            Arc::new(Mutex::new(buffer_tx)),
        ))));

        builder.format(|buf, record| {
            let dt = Local::now();

            // TODO(isaidsari): use library for this
            let lvl = match record.level() {
                log::Level::Error => "\x1b[1;31mERROR\x1b[0m",
                log::Level::Warn => "\x1b[1;33mWARN\x1b[0m",
                log::Level::Info => "\x1b[1;34mINFO\x1b[0m",
                log::Level::Debug => "\x1b[1;36mDEBUG\x1b[0m",
                log::Level::Trace => "\x1b[1;37mTRACE\x1b[0m",
            };
            let targ = format!("\x1b[1;32m{}\x1b[0m", record.target());
            let msg = record.args();

            writeln!(
                buf,
                "{} {} [{}] {}",
                dt.format("%Y-%m-%d %H:%M:%S").to_string(),
                record.level(),
                record.target(),
                record.args()
            )
            .unwrap();

            writeln!(
                io::stdout(),
                "{} {} [{}] {}",
                dt.format("%Y-%m-%d %H:%M:%S").to_string(),
                lvl,
                targ,
                msg
            )
        });

        builder.filter_level(log::LevelFilter::Debug);

        builder.init();

        // Spawn the task for processing the buffer asynchronously
        tokio::spawn(async move {
            while let Some(app_log) = buffer_rx.recv().await {
                println!("app log received at {}", app_log.message);
                let app_log_rec = insert_app_log(&app_log)
                    .await
                    .expect("Failed to insert app log");
            }
        });

        this
    }
}

// TODO(adnanjpg): make this configurable
const DEF_APP_NAME: &str = "remon";
