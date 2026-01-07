use serde::{Deserialize, Serialize};

/// Request for network status - if no params, returns latest frame
#[derive(Debug, Deserialize, Serialize)]
pub struct GetNetworkStatusRequest {
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
}

/// Single network interface info
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct SingleNetworkInfo {
    pub id: i64,
    pub frame_id: i64,
    /// Interface name (e.g., "eth0", "wlan0")
    pub interface_name: String,
    /// Total bytes received (cumulative)
    pub rx_bytes: i64,
    /// Total bytes transmitted (cumulative)
    pub tx_bytes: i64,
    /// Total packets received (cumulative)
    pub rx_packets: i64,
    /// Total packets transmitted (cumulative)
    pub tx_packets: i64,
}

/// Network status frame
#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkFrameStatus {
    pub id: i64,
    pub last_check: i64,
    pub interfaces: Vec<SingleNetworkInfo>,
}

/// Network status data (multiple frames)
#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkStatusData {
    pub frames: Vec<NetworkFrameStatus>,
}
