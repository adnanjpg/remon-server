use serde::{Deserialize, Serialize};

/// Public device summary (no sensitive fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub last_ip: Option<String>,
    pub last_seen: i64,
    pub created_at: i64,
    pub is_active: bool,
}

/// Internal device record (includes credential hashes).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredDevice {
    pub id: String,
    pub name: String,
    pub token_hash: String,
    pub last_ip: Option<String>,
    pub last_seen: i64,
    pub created_at: i64,
    pub is_active: bool,
}

impl From<StoredDevice> for Device {
    fn from(d: StoredDevice) -> Self {
        Device {
            id: d.id,
            name: d.name,
            last_ip: d.last_ip,
            last_seen: d.last_seen,
            created_at: d.created_at,
            is_active: d.is_active,
        }
    }
}
