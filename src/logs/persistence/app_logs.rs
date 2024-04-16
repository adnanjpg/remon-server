use crate::logs::models::get_app_logs::{OrderBy, OrderByDirection, PaginationInfo};

use super::{get_default_sql_connection, SQLConnection};

use chrono::format::format;
use serde::{Deserialize, Serialize};
use sqlx::{Execute, QueryBuilder, Sqlite};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;

#[derive(Debug, Serialize, Deserialize, sqlx::Type, Clone, PartialEq, PartialOrd, EnumIter)]
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

            let binded = sqlx::query_scalar::<_, String>(&app_ids_statement)
                .bind(&start_date)
                .bind(&end_date);

            let res = binded.fetch_all(&conn).await?;

            res
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

pub async fn get_app_logs(
    start_date: Option<i64>,
    end_date: Option<i64>,
    app_ids: Option<Vec<String>>,
    order_by: Option<OrderBy>,
    order_by_direction: Option<OrderByDirection>,
    filter_by_word: Option<String>,
    levels: Option<Vec<LogLevel>>,
    pagination_info: &PaginationInfo,
) -> Result<Vec<AppLog>, sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    // 1 = 1 is a hack to make the query builder work
    let mut query: QueryBuilder<Sqlite> =
        QueryBuilder::new(format!("SELECT * from {} where 1=1 ", APP_LOGS_TABLE_NAME));

    // if let Some(start_date) = start_date {
    //     query.push(" AND logged_at >= ?");
    //     query.push_bind(start_date);
    // }

    // if let Some(end_date) = end_date {
    //     query.push(" AND logged_at <= ?");
    //     query.push_bind(end_date);
    // }

    // if let Some(app_ids) = &app_ids {
    //     query.push(" AND app_id IN (");
    //     let it = app_ids.iter();
    //     for (i, app_id) in it.enumerate() {
    //         query.push("?");
    //         query.push_bind(app_id);

    //         if i < app_ids.len() - 1 {
    //             query.push(", ");
    //         }
    //     }
    //     query.push(")");
    // }

    // if let Some(filter_by_word) = filter_by_word {
    //     query.push(" AND message LIKE ?");
    //     query.push_bind(format!("%{}%", filter_by_word));
    // }

    // if let Some(levels) = &levels {
    //     query.push(" AND log_level IN (");
    //     for (i, level) in levels.iter().enumerate() {
    //         query.push("?");
    //         query.push_bind(level);

    //         if i < levels.len() - 1 {
    //             query.push(", ");
    //         }
    //     }
    //     query.push(")");
    // }

    // if let Some(order_by) = order_by {
    //     query.push(" ORDER BY ");
    //     query.push(&order_by);

    //     if let Some(order_by_direction) = order_by_direction {
    //         query.push(" ");
    //         query.push(&order_by_direction.to_string());
    //     }
    // }

    query.push(format!(
        " LIMIT {} OFFSET {}",
        pagination_info.page_size,
        pagination_info.page * pagination_info.page_size
    ));

    let qub = query.build();
    let query_sql = qub.sql();

    let query_res = sqlx::query_as::<_, AppLog>(&query_sql);
    let app_logs: Vec<AppLog> = query_res.fetch_all(&conn).await?;

    Ok(app_logs)
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
