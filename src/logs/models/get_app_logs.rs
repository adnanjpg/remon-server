use serde::{Deserialize, Serialize};

use crate::logs::persistence::LogLevel;

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct GetAppLogsRequest {
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    pub include_app_ids: Option<Vec<String>>,
    pub order_by: Option<OrderBy>,
    pub order_by_direction: Option<OrderByDirection>,
    pub filter_by_word: Option<String>,
    pub levels: Option<Vec<LogLevel>>,
    pub pagination_info: PaginationInfo,
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct PaginationInfo {
    pub page: i64,
    pub page_size: i64,
}

#[derive(Debug, Deserialize, Serialize, strum_macros::Display)]
pub enum OrderBy {
    Time,
    AppId,
    Log,
}

#[derive(Debug, Deserialize, Serialize, strum_macros::Display)]
pub enum OrderByDirection {
    Asc,
    Desc,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct AppLogRecord {
    pub id: i64,
    pub level: LogLevel,
    pub app_id: String,
    pub message: String,
    pub logged_at: i64,
    pub target: String,
}
