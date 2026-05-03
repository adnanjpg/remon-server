//! REST DTOs for `/probes`, `/metrics/probe/{...}`, and the loader-reload
//! admin endpoint.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::models::probe::{ProbeMetric, ProbeRun};

#[derive(Debug, Serialize)]
pub struct ProbeListEntry {
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub schedule: String,
    pub timeout_ms: u64,
    pub last_run_at: Option<i64>,
    pub last_message: Option<String>,
    /// `true`/`false` after the first run; `null` until then. `false`
    /// means contract violation — the runner couldn't parse the script's
    /// stdout. Watch this from a dashboard to catch broken probes early.
    pub last_parse_ok: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct ListProbesResponse {
    pub probes: Vec<ProbeListEntry>,
}

#[derive(Debug, Serialize)]
pub struct ProbeDetail {
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub schedule: String,
    pub timeout_ms: u64,
    pub command: Vec<String>,
    pub platforms: Vec<String>,
    pub last_run: Option<ProbeRunDto>,
    /// Metric values from the most recent run. Empty until the probe
    /// has fired once (or the probe ran but emitted nothing).
    pub last_metrics: Vec<ProbeMetric>,
}

#[derive(Debug, Serialize)]
pub struct ProbeRunDto {
    pub timestamp: i64,
    pub duration_ms: i64,
    pub exit_code: Option<i32>,
    pub message: Option<String>,
    pub parse_ok: bool,
}

impl From<ProbeRun> for ProbeRunDto {
    fn from(r: ProbeRun) -> Self {
        Self {
            timestamp: r.timestamp,
            duration_ms: r.duration_ms,
            exit_code: r.exit_code,
            message: r.message,
            parse_ok: r.parse_ok,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ProbeHistoryResponse {
    pub probe_name: String,
    pub runs: Vec<ProbeRunDto>,
}

#[derive(Debug, Serialize)]
pub struct ReloadProbesResponse {
    pub loaded: Vec<String>,
    pub skipped_disabled: Vec<String>,
    pub skipped_platform: Vec<String>,
    pub failed: Vec<ReloadFailure>,
}

#[derive(Debug, Serialize)]
pub struct ReloadFailure {
    pub path: String,
    pub error: String,
}

impl ReloadFailure {
    pub fn from_pair((path, error): (PathBuf, String)) -> Self {
        Self {
            path: path.display().to_string(),
            error,
        }
    }
}

// ===== /metrics/probe/{probe_name}/{metric_name} =====

#[derive(Debug, Deserialize)]
pub struct ProbeMetricRangeQuery {
    pub start: Option<i64>,
    pub end: Option<i64>,
    /// Optional resolution override. Only `"raw"` is populated today —
    /// rollup pipeline for probe metrics is not in MVP.
    pub resolution: Option<String>,
    pub limit: Option<u32>,
    /// Canonical labels JSON (sorted keys, no whitespace). Filters to a
    /// single labelled stream when set; omit to return all label
    /// combinations and let the client group by label.
    pub labels: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProbeMetricPoint {
    pub timestamp: i64,
    pub labels: serde_json::Value,
    pub value: f64,
}

#[derive(Debug, Serialize)]
pub struct ProbeMetricHistoryResponse {
    pub probe_name: String,
    pub metric_name: String,
    pub resolution: String,
    pub points: Vec<ProbeMetricPoint>,
}
