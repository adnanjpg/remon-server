use super::{get_default_sql_connection, SQLConnection};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, sqlx::Type, Clone)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warning,
    Error,
}

impl LogLevel {
    pub fn from_log_crate_level(s: &log::Level) -> LogLevel {
        match s {
            log::Level::Trace => LogLevel::Trace,
            log::Level::Debug => LogLevel::Debug,
            log::Level::Info => LogLevel::Info,
            log::Level::Warn => LogLevel::Warning,
            log::Level::Error => LogLevel::Error,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct AppLog {
    pub id: i64,
    pub log_level: LogLevel,
    pub app_id: String,
    pub logged_at: i64,
    pub message: String,
    // like function name or crate name
    // e.g. remon_server::monitor::config_exceeds, sqlx::query
    pub target: String,
}

const APP_LOGS_TABLE_NAME: &str = "app_logs";

pub async fn insert_app_log(log: &AppLog) -> Result<(), sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    let statement = format!(
        "INSERT INTO {}
        (log_level, app_id, logged_at, message, target)
        VALUES (?, ?, ?, ?, ?)",
        APP_LOGS_TABLE_NAME
    );

    sqlx::query(&statement)
        .bind(&log.log_level)
        .bind(&log.app_id)
        .bind(&log.logged_at)
        .bind(&log.message)
        .bind(&log.target)
        .execute(&conn)
        .await?;

    Ok(())
}

pub(super) async fn create_app_logs_table(conn: &SQLConnection) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        log_level INTEGER NOT NULL,
        app_id TEXT NOT NULL,
        logged_at INTEGER NOT NULL,
        message TEXT NOT NULL,
        target TEXT NOT NULL
    )",
        APP_LOGS_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}
