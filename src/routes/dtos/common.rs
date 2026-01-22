use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseBody {
    Success(bool),
    Error(String),
    Token(String),
}