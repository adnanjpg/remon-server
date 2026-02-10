use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct AppLog {
    pub id: i32,
    pub log_level: String,
    pub app_id: String,
    pub logged_at: i64,
    pub message: String,
    pub target: String,
}

#[derive(Serialize)]
pub struct GetAppIdsResponse {
    pub ids: Vec<String>,
}

#[derive(Serialize)]
pub struct GetAppLogsResponse {
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
    pub logs: Vec<AppLog>,
}

#[derive(Deserialize)]
pub struct GetAppIdsQueryParams {
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
}

#[derive(Deserialize)]
pub struct GetAppLogsQueryParams {
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    #[serde(default)]
    pub include_app_ids: Option<String>,
    pub order_by: Option<String>,
    pub order_by_direction: Option<String>,
    pub filter_by_word: Option<String>,
    #[serde(default)]
    pub levels: Option<String>,
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_page_size")]
    pub page_size: i64,
}

fn default_page() -> i64 {
    1
}

fn default_page_size() -> i64 {
    10
}