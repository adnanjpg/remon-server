//! Domain types for the probe engine.
//!
//! Probes are pure metric sources. The script emits zero or more numeric
//! metrics per run; severity is derived from the existing alert rule
//! engine (a rule of `metric_type='probe'` targeting a specific
//! probe_name + metric_field). There is no `status` enum on the wire and
//! no transition tracking in the runner — `alert_rules` already does
//! both better, with cooldown and history.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single numeric measurement reported by a probe.
///
/// `labels` is intentionally a `BTreeMap` (sorted keys) so canonicalising
/// to JSON for the storage primary key is deterministic — two probes
/// emitting `{jail:sshd}` will collide in the PK rather than store dupes
/// just because the JSON encoder shuffled key order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeMetric {
    pub name: String,
    pub value: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Empty map serialises to `{}` — never `null` — so the storage
    /// primary key is well-defined for unlabelled metrics.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl ProbeMetric {
    /// Canonical JSON encoding of `labels` for use in the metrics_probe
    /// PK. `BTreeMap` already iterates in key-sorted order; we just
    /// serialise without whitespace.
    pub fn labels_canonical(&self) -> String {
        serde_json::to_string(&self.labels).unwrap_or_else(|_| "{}".to_string())
    }
}

/// What the runner expects to read from a probe's stdout — one JSON
/// line. `metrics` may be empty (probe ran but had nothing to report);
/// `timestamp` defaults to wall-clock-at-receipt; `message` is an
/// optional one-line summary persisted on the run-meta row.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProbeOutput {
    #[serde(default)]
    pub metrics: Vec<ProbeMetric>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub timestamp: Option<i64>,
}

/// One row's worth of run metadata — did the script execute, how long
/// did it take, did the runner manage to parse anything? Metric values
/// land in `metrics_probe`, not here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeRun {
    pub probe_name: String,
    pub timestamp: i64,
    pub duration_ms: i64,
    /// `None` when the runner killed the child on timeout.
    pub exit_code: Option<i32>,
    pub message: Option<String>,
    /// `true` iff the runner managed to parse at least one JSON line.
    /// `false` means contract violation — bad probe, fix the script.
    pub parse_ok: bool,
}
