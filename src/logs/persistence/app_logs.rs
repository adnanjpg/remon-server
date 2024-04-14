use super::{get_default_sql_connection, SQLConnection};

use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Serialize, Deserialize, sqlx::Type, Clone, PartialEq, PartialOrd)]
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
    pub fn from_string(level: &str) -> LogLevel {
        match level.to_lowercase().as_str() {
            "error" => LogLevel::Error,
            "warn" => LogLevel::Warning,
            "warning" => LogLevel::Warning,
            "info" => LogLevel::Info,
            "debug" => LogLevel::Debug,
            "trace" => LogLevel::Trace,
            _ => LogLevel::Info,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
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

pub async fn get_app_ids(
    start_date: Option<i64>,
    end_date: Option<i64>,
) -> Result<Vec<String>, sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    let app_ids_query = match (start_date, end_date) {
        (Some(_), Some(_)) => {
            let app_ids_statement = format!(
                "SELECT DISTINCT app_id FROM {} WHERE logged_at BETWEEN ? AND ?",
                APP_LOGS_TABLE_NAME
            );

            sqlx::query_scalar::<_, String>(&app_ids_statement)
                .bind(&start_date)
                .bind(&end_date)
                .fetch_all(&conn)
                .await?
        }
        _ => {
            let app_ids_statement = format!("SELECT DISTINCT app_id FROM {}", APP_LOGS_TABLE_NAME);

            sqlx::query_scalar::<_, String>(&app_ids_statement)
                .fetch_all(&conn)
                .await?
        }
    };

    let app_ids = app_ids_query;

    Ok(app_ids)
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
