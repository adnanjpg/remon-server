use chrono::Local;
use env_logger::fmt::Color;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use std::io::Write;

use crate::persistence::app_logs::{insert_app_log, AppLog, LogLevel};

async fn log_to_db_receiver(mut receiver: mpsc::Receiver<AppLog>) {
    while let Some(app_log) = receiver.recv().await {
        let app_log_rec = insert_app_log(&app_log)
            .await
            .expect("Failed to insert app log");
    }
}
#[derive(Debug)]
struct MyValue {
    data: String,
    // Add any other fields you need
}

lazy_static! {
    static ref GLOBAL_VALUE: Mutex<Option<MyValue>> = Mutex::new(None);
}

async fn set_global_value(value: MyValue) {
    let mut global_value = GLOBAL_VALUE.lock().await.unwrap();
    *global_value = Some(value);
}

// TODO(adnanjpg): make this configurable
const DEF_APP_NAME: &str = "remon";

pub fn init_logger(test_assertions: bool) {
    let (sender, receiver) = mpsc::channel::<AppLog>(100);

    let sender_arc = Arc::new(sender);

    let receiver_task = log_to_db_receiver(receiver);
    tokio::spawn(receiver_task);

    // make sender have a &'static lifetime
    let sender_static = to_static_fr(&sender_arc);

    let mut bui = env_logger::builder();
    let bui = bui.format(|buf, record| {
        let dt = Local::now();

        let lvl = record.level();
        let targ = record.target();
        let msg = record.args();

        let app_log = AppLog {
            id: -1,
            log_level: LogLevel::from_log_crate_level(&lvl),
            app_id: DEF_APP_NAME.to_owned(),
            logged_at: dt.timestamp(),
            message: msg.to_string(),
            target: targ.to_owned(),
        };
        // let senn = Arc::clone(&sender_arc_for_closure);
        tokio::spawn(async move {
            sender_static
                .send(app_log)
                .await
                .expect("Failed to send message");
        });

        let mut level_style = buf.style();
        level_style
            .set_color(match record.level() {
                log::Level::Error => Color::Red,
                log::Level::Warn => Color::Yellow,
                log::Level::Info => Color::Green,
                log::Level::Debug => Color::Blue,
                log::Level::Trace => Color::Magenta,
            })
            .set_bold(true);

        let mut date_style = buf.style();
        date_style
            .set_color(Color::Rgb(91, 24, 128))
            .set_bold(true)
            .set_bg(Color::Rgb(255, 255, 255));

        let mut target_style = buf.style();
        target_style
            .set_color(Color::Rgb(128, 24, 60))
            .set_bold(true);

        writeln!(
            buf,
            "{} {} {}: {}",
            date_style.value(dt.format("%Y-%m-%d %H:%M:%S")),
            level_style.value(lvl),
            target_style.value(targ),
            msg
        )
    });

    if cfg!(debug_assertions) {
        bui.filter_level(log::LevelFilter::Debug).init();
    } else if test_assertions {
        bui.filter_level(log::LevelFilter::Info).init();
    } else {
        bui.filter_level(log::LevelFilter::Info).init();
    }
}
