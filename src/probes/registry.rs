//! In-memory registry of currently-loaded probes plus their last result.
//!
//! Two things this owns:
//! 1. The set of probes the scheduler is driving (manifest + cancel
//!    handle for the per-probe task).
//! 2. The latest `ProbeRun` per probe, served to `GET /probes` and
//!    `GET /probes/{name}` in O(1) without hitting the DB.
//!
//! History (`/probes/{name}/history`) goes through the DB; the registry
//! only caches the latest. This is the same split as `processes_latest`
//! vs the time-series tables.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::models::probe::{ProbeMetric, ProbeRun};

use super::manifest::Manifest;

/// One probe's place in the running scheduler.
pub struct ProbeEntry {
    pub manifest: Manifest,
    /// Last run-meta we observed. `None` until the first run finishes.
    pub last_run: Option<ProbeRun>,
    /// Metrics emitted by the most recent run. Empty vec until the
    /// first run, or when the probe ran but emitted nothing.
    pub last_metrics: Vec<ProbeMetric>,
    /// Handle to the per-probe task. `abort()`ed on hot-reload when the
    /// manifest's hash changes or the file is removed.
    pub task: Option<JoinHandle<()>>,
}

#[derive(Default)]
pub struct ProbeRegistryInner {
    pub probes: HashMap<String, ProbeEntry>,
}

/// Shared handle. Wrapped in `Arc<RwLock<...>>` because:
/// - reads (REST handlers) are frequent and short
/// - writes (loader hot-reload, scheduler "post-run update") are rare
/// - tokio::sync::RwLock is async-aware, won't block runtime threads
pub type ProbeRegistry = Arc<RwLock<ProbeRegistryInner>>;

pub fn new_registry() -> ProbeRegistry {
    Arc::new(RwLock::new(ProbeRegistryInner::default()))
}
