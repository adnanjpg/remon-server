use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct GetAppIdsRequest {
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
}
