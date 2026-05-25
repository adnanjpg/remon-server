use serde::{Deserialize, Serialize};

const VALID_TYPES: &[&str] = &["fcm", "telegram", "ntfy", "webhook", "web-push"];
const VALID_SEVERITIES: &[&str] = &["warn", "crit"];

#[derive(Debug, Deserialize)]
pub struct CreateChannelRequest {
    pub name: String,
    pub r#type: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub config: serde_json::Value,
    pub min_severity: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateChannelRequest {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub config: serde_json::Value,
    pub min_severity: Option<String>,
}

fn default_true() -> bool {
    true
}

impl CreateChannelRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("name is required".to_string());
        }
        if !VALID_TYPES.contains(&self.r#type.as_str()) {
            return Err(format!("type must be one of: {}", VALID_TYPES.join(", ")));
        }
        if let Some(ref sev) = self.min_severity
            && !VALID_SEVERITIES.contains(&sev.as_str())
        {
            return Err(format!(
                "min_severity must be one of: {}",
                VALID_SEVERITIES.join(", ")
            ));
        }
        Ok(())
    }
}

impl UpdateChannelRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("name is required".to_string());
        }
        if let Some(ref sev) = self.min_severity
            && !VALID_SEVERITIES.contains(&sev.as_str())
        {
            return Err(format!(
                "min_severity must be one of: {}",
                VALID_SEVERITIES.join(", ")
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct ChannelResponse {
    pub id: i64,
    pub name: String,
    pub r#type: String,
    pub enabled: bool,
    pub config: serde_json::Value,
    pub min_severity: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Serialize)]
pub struct ListChannelsResponse {
    pub channels: Vec<ChannelResponse>,
}

#[derive(Debug, Serialize)]
pub struct TestChannelResponse {
    pub delivered: usize,
}
