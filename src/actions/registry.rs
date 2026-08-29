//! In-memory registry of loaded action scripts, plus the loader that fills it.
//!
//! Deliberately lighter than the probe registry: an action has no schedule, so
//! there is no task to spawn, nothing to abort on reload, and no DB definition
//! table. The registry is the whole runtime state — a name → manifest map that
//! `POST /actions/reload` replaces wholesale.
//!
//! Bindings live in `alert_actions` and reference a script by name. They
//! outlive a reload that drops the script (the run then fails loudly with
//! "no such action" rather than silently doing nothing), which is why the
//! binding API refuses to *create* a reference to a name that isn't loaded.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use log::{info, warn};
use tokio::sync::RwLock;

use crate::probes::manifest::ManifestError;

use super::manifest::ActionManifest;

#[derive(Default)]
pub struct ActionRegistryInner {
    pub actions: HashMap<String, ActionManifest>,
}

/// Shared handle. Reads (execution, REST listings) are frequent and short;
/// writes happen only on reload.
pub type ActionRegistry = Arc<RwLock<ActionRegistryInner>>;

pub fn new_registry() -> ActionRegistry {
    Arc::new(RwLock::new(ActionRegistryInner::default()))
}

/// What one load pass did. Mirrors the probe loader's report so the two
/// reload endpoints read the same way.
#[derive(Debug, Default)]
pub struct LoadReport {
    pub loaded: Vec<String>,
    pub skipped_disabled: Vec<String>,
    pub skipped_platform: Vec<String>,
    pub failed: Vec<(PathBuf, String)>,
}

/// Scan `dir` and replace the registry's contents with what it finds.
///
/// Behaviour:
/// - A file that fails to parse or validate is recorded in `failed`; the rest
///   of the pass continues.
/// - `enabled: false` files are parsed and reported, but not registered — a
///   binding pointing at one fails at run time with a clear message rather
///   than executing something the operator had switched off.
/// - A `platforms` filter excluding this host skips the file entirely.
/// - Two files claiming the same name: first one wins, second is a `failed`
///   entry. Silently shadowing would make which script ran depend on
///   directory order.
pub async fn load(dir: &std::path::Path, registry: &ActionRegistry) -> LoadReport {
    let mut report = LoadReport::default();

    let raw_results = match ActionManifest::load_dir(dir).await {
        Ok(rs) => rs,
        Err(e) => {
            warn!("action loader could not read directory {:?}: {:?}", dir, e);
            return report;
        }
    };

    let mut next: HashMap<String, ActionManifest> = HashMap::new();
    for r in raw_results {
        let m = match r {
            Ok(m) => m,
            Err((path, ManifestError::Validation(msg)))
            | Err((path, ManifestError::Parse(msg))) => {
                warn!("action manifest {:?} rejected: {}", path, msg);
                report.failed.push((path, msg));
                continue;
            }
            Err((path, ManifestError::Io(e))) => {
                warn!("action manifest {:?} read failed: {}", path, e);
                report.failed.push((path, e.to_string()));
                continue;
            }
        };

        if !m.matches_current_platform() {
            report.skipped_platform.push(m.name.clone());
            continue;
        }
        if !m.enabled {
            report.skipped_disabled.push(m.name.clone());
            continue;
        }
        if let Some(existing) = next.get(&m.name) {
            let msg = format!(
                "duplicate action name '{}' (already defined by {:?})",
                m.name, existing.source_path
            );
            warn!("{}", msg);
            report.failed.push((m.source_path.clone(), msg));
            continue;
        }
        report.loaded.push(m.name.clone());
        next.insert(m.name.clone(), m);
    }

    report.loaded.sort();
    report.skipped_disabled.sort();
    report.skipped_platform.sort();

    {
        let mut reg = registry.write().await;
        reg.actions = next;
    }

    info!(
        "actions loaded: {} active, {} disabled, {} off-platform, {} failed",
        report.loaded.len(),
        report.skipped_disabled.len(),
        report.skipped_platform.len(),
        report.failed.len()
    );
    report
}
