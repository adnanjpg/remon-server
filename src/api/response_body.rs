use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub enum ResponseBody {
    Success(bool),
    Error(String),
    Token(String),
}
