//! Action manifest — the on-disk definition of a remediation script.
//!
//! Deliberately the probe manifest minus the schedule: an action has no
//! cadence of its own, it runs when an alert transition (or an operator)
//! says so. Everything else — argv-only commands, the timeout bracket, the
//! platform filter, `run_as_user`, `memory_limit_mb` — is the same shape and
//! shares the probe module's validators, so the two headers cannot drift.
//!
//! Same two formats as probes: a `.yaml` file, or any script carrying
//! `# @action key=value` header lines. The inline form is the one operators
//! reach for — code and config in one file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::fs;

use crate::probes::manifest::{
    DEFAULT_TIMEOUT_MS, INLINE_HEADER_PROBE_BYTES, MAX_NAME_LEN, MAX_TIMEOUT_MS, MIN_TIMEOUT_MS,
    ManifestError, current_platform, extract_tagged_kv, is_valid_name,
};
use crate::probes::runner::ExecSpec;

/// Header marker. `@probe` and `@action` are distinct so a file can never be
/// picked up by both loaders — a script that measures and a script that
/// changes things are different responsibilities.
const HEADER_TAG: &str = "@action";

/// Raw shape off YAML / the inline header, before validation.
#[derive(Debug, Deserialize)]
struct RawActionManifest {
    name: String,
    #[serde(default)]
    description: Option<String>,
    /// Unlike probes, actions default to **enabled**: an action file does
    /// nothing until a binding points at it, so the dangerous default here is
    /// the opposite one — a loaded-but-disabled action that silently makes a
    /// binding a no-op.
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// argv array — first element is the executable. String-form commands are
    /// rejected so there is never a `sh -c` expansion path.
    command: Vec<String>,
    #[serde(default)]
    platforms: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    run_as_user: Option<String>,
    #[serde(default)]
    memory_limit_mb: Option<u64>,
}

fn default_true() -> bool {
    true
}

/// Validated, ready-to-run action definition.
#[derive(Debug, Clone)]
pub struct ActionManifest {
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub timeout: Duration,
    pub command: Vec<String>,
    pub platforms: Vec<String>,
    pub env: HashMap<String, String>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub run_as_user: Option<String>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub memory_limit_mb: Option<u64>,
    pub source_path: PathBuf,
}

impl ActionManifest {
    /// Read + validate one action file.
    pub async fn load(path: &Path) -> Result<Self, ManifestError> {
        let bytes = fs::read(path).await?;

        let is_yaml = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yaml") | Some("yml")
        );

        let raw = if is_yaml {
            serde_yaml_ng::from_slice::<RawActionManifest>(&bytes)
                .map_err(|e| ManifestError::Parse(format!("{}: {}", path.display(), e)))?
        } else {
            parse_inline_header(&bytes, path)?
        };
        Self::validate(raw, path.to_path_buf())
    }

    /// Scan a directory for action definitions. Errors are returned per-entry
    /// so one malformed file doesn't take down the actions that are fine —
    /// the reload endpoint reports both halves.
    pub async fn load_dir(
        dir: &Path,
    ) -> Result<Vec<Result<Self, (PathBuf, ManifestError)>>, ManifestError> {
        let mut out = Vec::new();
        let mut entries = match fs::read_dir(dir).await {
            Ok(e) => e,
            // A missing directory is not an error — most installs have no
            // actions at all. Caller logs and proceeds.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(ManifestError::Io(e)),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let candidate = match path.extension().and_then(|e| e.to_str()) {
                Some("yaml") | Some("yml") => true,
                _ => has_inline_header(&path).await,
            };
            if !candidate {
                continue;
            }
            match Self::load(&path).await {
                Ok(m) => out.push(Ok(m)),
                Err(e) => out.push(Err((path, e))),
            }
        }
        Ok(out)
    }

    fn validate(raw: RawActionManifest, source_path: PathBuf) -> Result<Self, ManifestError> {
        let name = raw.name.trim().to_string();
        if !is_valid_name(&name) {
            return Err(ManifestError::Validation(format!(
                "name '{}': must match [a-z][a-z0-9_-]{{0,{}}}",
                name,
                MAX_NAME_LEN - 1
            )));
        }

        if raw.command.is_empty() {
            return Err(ManifestError::Validation(
                "command: must be a non-empty array (e.g. [\"/usr/local/bin/drain\"])".into(),
            ));
        }
        if raw.command.iter().any(|s| s.is_empty()) {
            return Err(ManifestError::Validation(
                "command: contains an empty argv element".into(),
            ));
        }

        let timeout_ms = raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(ManifestError::Validation(format!(
                "timeout_ms {} out of range [{}..{}]",
                timeout_ms, MIN_TIMEOUT_MS, MAX_TIMEOUT_MS
            )));
        }

        if let Some(mb) = raw.memory_limit_mb
            && (mb == 0 || mb > 65_536)
        {
            return Err(ManifestError::Validation(format!(
                "memory_limit_mb {} out of range [1..65536]",
                mb
            )));
        }

        Ok(ActionManifest {
            name,
            description: raw.description.filter(|s| !s.is_empty()),
            enabled: raw.enabled,
            timeout: Duration::from_millis(timeout_ms),
            command: raw.command,
            platforms: raw
                .platforms
                .into_iter()
                .map(|s| s.to_lowercase())
                .collect(),
            env: raw.env,
            run_as_user: raw.run_as_user,
            memory_limit_mb: raw.memory_limit_mb,
            source_path,
        })
    }

    /// Runner view. `extra_env` carries the alert context (rule, labels,
    /// value, event) — merged over the manifest's own env, so a binding can
    /// never be shadowed by a stale value the file happens to set.
    pub fn exec_spec<'a>(&'a self, merged_env: &'a HashMap<String, String>) -> ExecSpec<'a> {
        ExecSpec {
            name: &self.name,
            command: &self.command,
            env: merged_env,
            timeout: self.timeout,
            run_as_user: self.run_as_user.as_deref(),
            memory_limit_mb: self.memory_limit_mb,
        }
    }

    /// Manifest env plus the caller's context vars. Context wins on collision.
    pub fn env_with(&self, context: &HashMap<String, String>) -> HashMap<String, String> {
        let mut merged = self.env.clone();
        merged.extend(context.iter().map(|(k, v)| (k.clone(), v.clone())));
        merged
    }

    /// True when this action's `platforms` filter allows the current host.
    pub fn matches_current_platform(&self) -> bool {
        if self.platforms.is_empty() {
            return true;
        }
        let target = current_platform();
        self.platforms.iter().any(|p| p == target)
    }
}

/// Cheap "does this look like an inline-header action?" check for the
/// directory walker. Reads only the head of the file.
async fn has_inline_header(path: &Path) -> bool {
    use tokio::io::AsyncReadExt;
    let mut f = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = vec![0u8; INLINE_HEADER_PROBE_BYTES as usize];
    let n = match f.read(&mut buf).await {
        Ok(n) => n,
        Err(_) => return false,
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    head.lines()
        .any(|l| extract_tagged_kv(l, HEADER_TAG).is_some())
}

/// Parse `# @action key=value` header lines into a `RawActionManifest`.
/// Stops at the first non-blank, non-shebang, non-comment line.
///
/// Recognised keys: `name, description, enabled, timeout_ms, platforms,
/// run_as_user, memory_limit_mb, command`. `command` defaults to the script
/// itself. `env` is not supported inline — the script can export what it
/// needs, and the alert context arrives as environment regardless.
fn parse_inline_header(bytes: &[u8], path: &Path) -> Result<RawActionManifest, ManifestError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| ManifestError::Parse(format!("{}: not valid UTF-8: {}", path.display(), e)))?;

    let mut kv: HashMap<String, String> = HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with("#!") || trimmed.starts_with('#') {
            if let Some((k, v)) = extract_tagged_kv(line, HEADER_TAG) {
                kv.insert(k, v);
            }
            continue;
        }
        break;
    }

    if kv.is_empty() {
        return Err(ManifestError::Parse(format!(
            "{}: no `# @action key=value` header lines found",
            path.display()
        )));
    }

    let name = kv
        .remove("name")
        .ok_or_else(|| ManifestError::Validation("inline header: `name` is required".into()))?;

    let platforms: Vec<String> = kv
        .remove("platforms")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let timeout_ms: Option<u64> = match kv.remove("timeout_ms") {
        Some(v) => Some(v.trim().parse().map_err(|_| {
            ManifestError::Validation(format!("inline header: timeout_ms '{}' is not a number", v))
        })?),
        None => None,
    };
    let memory_limit_mb: Option<u64> = match kv.remove("memory_limit_mb") {
        Some(v) => Some(v.trim().parse().map_err(|_| {
            ManifestError::Validation(format!(
                "inline header: memory_limit_mb '{}' is not a number",
                v
            ))
        })?),
        None => None,
    };

    let command: Vec<String> = match kv.remove("command") {
        Some(c) => shell_words::split(&c).map_err(|e| {
            ManifestError::Validation(format!("inline header: command parse: {}", e))
        })?,
        None => vec![
            path.to_str()
                .ok_or_else(|| {
                    ManifestError::Validation("inline header: file path is not valid UTF-8".into())
                })?
                .to_string(),
        ],
    };

    let raw = RawActionManifest {
        name,
        description: kv.remove("description"),
        enabled: kv
            .remove("enabled")
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "true" | "yes" | "1"))
            .unwrap_or(true),
        timeout_ms,
        command,
        platforms,
        env: Default::default(),
        run_as_user: kv.remove("run_as_user"),
        memory_limit_mb,
    };

    if !kv.is_empty() {
        // Unknown keys are a typo until proven otherwise — a silently
        // ignored `timout_ms` is a 30-second default nobody asked for.
        let mut unknown: Vec<&String> = kv.keys().collect();
        unknown.sort();
        return Err(ManifestError::Validation(format!(
            "inline header: unknown key(s): {}",
            unknown
                .into_iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_str(body: &str, name: &str) -> Result<ActionManifest, ManifestError> {
        let raw = parse_inline_header(body.as_bytes(), Path::new(name))?;
        ActionManifest::validate(raw, PathBuf::from(name))
    }

    #[test]
    fn inline_header_minimal() {
        let m =
            load_str("#!/bin/sh\n# @action name=drain\nexit 0\n", "drain.sh").expect("must parse");
        assert_eq!(m.name, "drain");
        // Actions default enabled — the binding is the opt-in, not the file.
        assert!(m.enabled);
        assert_eq!(m.command, vec!["drain.sh".to_string()]);
        assert_eq!(m.timeout, Duration::from_millis(DEFAULT_TIMEOUT_MS));
    }

    #[test]
    fn probe_header_is_not_an_action() {
        let err = load_str("# @probe name=cpu\n# @probe interval=1m\n", "cpu.sh")
            .expect_err("a probe file must not load as an action");
        assert!(matches!(err, ManifestError::Parse(_)));
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = load_str("# @action name=x\n# @action interval=1m\n", "x.sh")
            .expect_err("interval has no meaning for an action");
        assert!(format!("{err}").contains("interval"));
    }

    #[test]
    fn timeout_bracket_enforced() {
        let err = load_str("# @action name=x\n# @action timeout_ms=1\n", "x.sh")
            .expect_err("below the floor");
        assert!(format!("{err}").contains("out of range"));
    }

    #[test]
    fn context_env_wins_over_manifest_env() {
        let mut m = load_str("# @action name=x\n", "x.sh").expect("must parse");
        m.env.insert("REMON_RULE".into(), "stale".into());
        let ctx = HashMap::from([("REMON_RULE".to_string(), "live".to_string())]);
        assert_eq!(m.env_with(&ctx).get("REMON_RULE").unwrap(), "live");
    }
}
