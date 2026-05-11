//! Probe manifest — the on-disk YAML loaded from `probes/*.yaml`.
//!
//! Validation philosophy: reject early, reject loud. Bad input here
//! becomes a corrupt scheduler state we'd have to debug at runtime, so
//! every field is range-checked or charset-checked before the manifest
//! ever reaches the registry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use blake3::Hasher;
use serde::Deserialize;
use tokio::fs;

/// Hard caps. The runner allocates fresh state per invocation, so these
/// also bound worst-case memory: 100 ms is the floor (anything below
/// that is almost certainly a misconfig and a busy-loop trap), 10 min
/// the ceiling (anything longer than this is daemon territory and
/// should use a different mechanism).
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Maximum probe name length. 64 is enough for `category-subject` style
/// names ("ssl-cert-example-com") without inviting essay-length identifiers.
const MAX_NAME_LEN: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(String),
    #[error("validation: {0}")]
    Validation(String),
}

/// Raw YAML shape — what serde sees coming off the file. Validated and
/// converted to `Manifest` before reaching any other module.
#[derive(Debug, Deserialize)]
struct RawManifest {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    enabled: bool,
    /// Cron expression. Mutually exclusive with `interval`. At least one
    /// of the two must be set.
    #[serde(default)]
    schedule: Option<String>,
    /// Short-form interval like "60s" / "5m" / "1h". Mutually exclusive
    /// with `schedule`.
    #[serde(default)]
    interval: Option<String>,
    /// Hard wall-clock cap on script runtime. Defaults to 30s when
    /// omitted; both ends bracketed against pathological values.
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// argv array — first element is the executable, rest are args. We
    /// reject string-form commands so there is never a `sh -c` shell
    /// expansion path. Required.
    command: Vec<String>,
    /// Restrict by host platform. Empty / absent = all platforms.
    #[serde(default)]
    platforms: Vec<String>,
    /// Extra env vars to merge into the script's environment.
    #[serde(default)]
    env: HashMap<String, String>,
    /// On Unix only: drop privileges to this user before exec.
    /// Looked up via `getpwnam(3)`. Probe fails to start (parse_ok=false
    /// on the run-meta, no metrics emitted) if the name doesn't resolve
    /// or the server lacks the privilege to switch.
    #[serde(default)]
    run_as_user: Option<String>,
    /// On Unix only: cap the child's address-space size with
    /// `setrlimit(RLIMIT_AS, ...)`. None = no limit. The cap is a
    /// process-wide hard ceiling, not a measure-and-kill — so a probe
    /// that allocates beyond it gets ENOMEM from malloc rather than
    /// going out of band silently.
    #[serde(default)]
    memory_limit_mb: Option<u64>,
    /// Execution model:
    /// - `oneshot` (default): script runs to completion every fire,
    ///   one `ProbeRun` row per fire. Wall-clock `timeout_ms` applies.
    /// - `stream`: script stays running and emits newline-delimited
    ///   JSON. Each line becomes its own `ProbeRun` + metric inserts.
    ///   `timeout_ms` is interpreted as a per-line idle deadline rather
    ///   than total runtime. Schedule (interval/cron) gates *restart*
    ///   on child exit — when the script is alive it keeps producing.
    #[serde(default)]
    mode: Option<String>,
}

/// Validated, ready-to-run probe manifest.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub schedule: Schedule,
    pub timeout: Duration,
    pub command: Vec<String>,
    pub platforms: Vec<String>,
    pub env: HashMap<String, String>,
    pub run_as_user: Option<String>,
    pub memory_limit_mb: Option<u64>,
    pub mode: ProbeMode,
    /// blake3 hash of the manifest file contents. Stored in the DB so
    /// the loader can detect "manifest changed since last load".
    pub manifest_hash: String,
    /// Where the manifest was loaded from, for log messages.
    pub source_path: PathBuf,
}

/// How the runner drives a probe's child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeMode {
    /// Default. Spawn → wait → parse last line → exit. Schedule = fire
    /// cadence. Wall-clock `timeout_ms` enforced.
    Oneshot,
    /// Long-running daemon. Each newline-delimited JSON line on stdout
    /// becomes a separate ProbeRun. `timeout_ms` here is "max seconds
    /// without a line before we give up and let the schedule restart
    /// us"; total runtime is unbounded.
    Stream,
}

impl ProbeMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "oneshot" | "one-shot" | "one_shot" => Some(ProbeMode::Oneshot),
            "stream" | "streaming" => Some(ProbeMode::Stream),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Schedule {
    /// Fire every `Duration`. Drift-correcting: next-fire is computed
    /// from the previous fire, not from "now plus interval".
    Interval(Duration),
    /// Fire when the cron expression next matches. Cron uses 6-field
    /// (with-seconds) form via the `cron` crate. We accept the more
    /// common 5-field by prepending "0" to the user input.
    Cron(cron::Schedule),
}

impl Schedule {
    /// Wire-form back to the storage layer. We persist the original
    /// user-typed string so manifest changes round-trip cleanly.
    pub fn as_db_string(&self) -> String {
        match self {
            Schedule::Interval(d) => format!("{}s", d.as_secs()),
            Schedule::Cron(c) => c.to_string(),
        }
    }
}

impl Manifest {
    /// Read + validate one manifest file. The disk path becomes
    /// `source_path` for logs; `manifest_hash` is taken over the raw
    /// bytes so reformatting (whitespace) does not register as a change.
    ///
    /// Two formats are accepted:
    /// 1. **YAML manifest** (`.yaml` / `.yml`) — the historic shape; the
    ///    `command` field points at a separate script file.
    /// 2. **Inline-header script** — any other extension. Probe metadata
    ///    lives in `# @probe key=value` comment lines at the top of the
    ///    script, and the script itself is the command. One file per
    ///    probe; config and code travel together.
    pub async fn load(path: &Path) -> Result<Self, ManifestError> {
        let bytes = fs::read(path).await?;
        let hash = {
            let mut h = Hasher::new();
            h.update(&bytes);
            h.finalize().to_hex().to_string()
        };

        let is_yaml = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yaml") | Some("yml")
        );

        let raw = if is_yaml {
            serde_yaml_ng::from_slice::<RawManifest>(&bytes)
                .map_err(|e| ManifestError::Parse(format!("{}: {}", path.display(), e)))?
        } else {
            parse_inline_header(&bytes, path)?
        };
        Self::validate(raw, hash, path.to_path_buf())
    }

    /// Scan a directory and return everything that looks like a probe.
    ///
    /// What's "probe-shaped":
    /// - `.yaml` / `.yml` files (always)
    /// - Any other regular file whose first ~100 lines contain at least
    ///   one `# @probe` header. Avoids accidentally trying to parse
    ///   `README.md`, helper-libs, etc.
    ///
    /// Errors on individual files are returned per-entry so one bad
    /// manifest doesn't take down probes that are fine.
    pub async fn load_dir(
        dir: &Path,
    ) -> Result<Vec<Result<Self, (PathBuf, ManifestError)>>, ManifestError> {
        let mut out = Vec::new();
        let mut entries = match fs::read_dir(dir).await {
            Ok(e) => e,
            // A missing directory is not an error — fresh installs may
            // not have one yet. Caller logs and proceeds.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(ManifestError::Io(e)),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let candidate = match path.extension().and_then(|e| e.to_str()) {
                // YAML always counted.
                Some("yaml") | Some("yml") => true,
                // Other extensions only count if we can confirm the file
                // opted in via at least one `# @probe` header line. This
                // avoids parsing README.md / shared.lib.sh etc.
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

    fn validate(
        raw: RawManifest,
        manifest_hash: String,
        source_path: PathBuf,
    ) -> Result<Self, ManifestError> {
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
                "command: must be a non-empty array (e.g. [\"/usr/bin/check_x\"])".into(),
            ));
        }
        if raw.command.iter().any(|s| s.is_empty()) {
            return Err(ManifestError::Validation(
                "command: contains an empty argv element".into(),
            ));
        }

        let schedule = match (raw.schedule.as_deref(), raw.interval.as_deref()) {
            (Some(_), Some(_)) => {
                return Err(ManifestError::Validation(
                    "schedule and interval are mutually exclusive — pick one".into(),
                ));
            }
            (Some(c), None) => parse_cron(c)?,
            (None, Some(i)) => Schedule::Interval(parse_interval(i)?),
            (None, None) => {
                return Err(ManifestError::Validation(
                    "neither schedule nor interval set — probe would never run".into(),
                ));
            }
        };

        let timeout_ms = raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(ManifestError::Validation(format!(
                "timeout_ms {} out of range [{}..{}]",
                timeout_ms, MIN_TIMEOUT_MS, MAX_TIMEOUT_MS
            )));
        }

        if let Some(mb) = raw.memory_limit_mb {
            if mb == 0 || mb > 65_536 {
                return Err(ManifestError::Validation(format!(
                    "memory_limit_mb {} out of range [1..65536]",
                    mb
                )));
            }
        }

        let mode = match raw.mode.as_deref() {
            None => ProbeMode::Oneshot,
            Some(s) => ProbeMode::parse(s).ok_or_else(|| {
                ManifestError::Validation(format!("mode '{}': expected 'oneshot' or 'stream'", s))
            })?,
        };

        Ok(Manifest {
            name,
            description: raw.description.filter(|s| !s.is_empty()),
            enabled: raw.enabled,
            schedule,
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
            mode,
            manifest_hash,
            source_path,
        })
    }

    /// Returns true if this manifest's `platforms` filter allows the
    /// current host. An empty filter means "all platforms".
    pub fn matches_current_platform(&self) -> bool {
        if self.platforms.is_empty() {
            return true;
        }
        let target = current_platform();
        self.platforms.iter().any(|p| p == target)
    }
}

/// Number of bytes (not lines) we read from the head of a candidate
/// inline-header script before deciding it isn't a probe. Comfortably
/// fits the manifest header even with verbose comments, but small
/// enough not to slurp huge files we then throw away.
const INLINE_HEADER_PROBE_BYTES: u64 = 4096;

/// Cheap "does this look like an inline-header probe?" check used by
/// the directory walker to skip helper libraries, README files, etc.
/// Reads only the first `INLINE_HEADER_PROBE_BYTES` of the file.
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
    head.lines().any(|l| extract_header_kv(l).is_some())
}

/// Parse a script's `# @probe key=value` header lines into a
/// `RawManifest`. Stops at the first non-blank, non-shebang, non-comment
/// line — header must live at the top of the file.
///
/// Recognised keys mirror the YAML manifest field names:
///   `name, description, enabled, schedule, interval, timeout_ms,
///    platforms, run_as_user`
///
/// `command` defaults to `[<file_path>]` (the script runs itself). It
/// CAN be overridden in the header (`# @probe command=/usr/bin/python3
/// probes/my_probe.py`) but operators almost never need to.
///
/// Multi-value fields (`platforms`) accept comma-separated values:
///   `# @probe platforms=linux,macos`
///
/// `env` is not supported in inline form — script can `export FOO=bar`
/// itself if it needs custom env. Keep the header narrow.
fn parse_inline_header(bytes: &[u8], path: &Path) -> Result<RawManifest, ManifestError> {
    use std::collections::HashMap as StdMap;
    let text = std::str::from_utf8(bytes)
        .map_err(|e| ManifestError::Parse(format!("{}: not valid UTF-8: {}", path.display(), e)))?;

    let mut kv: StdMap<String, String> = StdMap::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        // Stop at the first non-comment, non-shebang, non-blank line.
        if trimmed.is_empty() || trimmed.starts_with("#!") || trimmed.starts_with('#') {
            if let Some((k, v)) = extract_header_kv(line) {
                kv.insert(k, v);
            }
            continue;
        }
        break;
    }

    if kv.is_empty() {
        return Err(ManifestError::Parse(format!(
            "{}: no `# @probe key=value` header lines found",
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

    // `command` override is rare but supported — single string with
    // shell-words splitting so quoted arguments survive.
    let command_override = kv.remove("command");
    let command: Vec<String> = match command_override {
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

    let raw = RawManifest {
        name,
        description: kv.remove("description"),
        enabled: kv
            .remove("enabled")
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "true" | "yes" | "1"))
            .unwrap_or(false),
        schedule: kv.remove("schedule"),
        interval: kv.remove("interval"),
        timeout_ms,
        command,
        platforms,
        env: Default::default(),
        run_as_user: kv.remove("run_as_user"),
        memory_limit_mb,
        mode: kv.remove("mode"),
    };

    if !kv.is_empty() {
        // Unknown keys: surface as a single error so typos get caught
        // immediately. We list them sorted so the message is stable.
        let mut unknown: Vec<&str> = kv.keys().map(String::as_str).collect();
        unknown.sort();
        return Err(ManifestError::Validation(format!(
            "inline header: unknown key(s): {}",
            unknown.join(", ")
        )));
    }

    Ok(raw)
}

/// Extract the `key=value` payload from a `# @probe ...` line. Returns
/// `None` for lines that don't begin with the marker, so the caller can
/// freely pass arbitrary script lines through this filter.
fn extract_header_kv(line: &str) -> Option<(String, String)> {
    let s = line.trim_start();
    let rest = s.strip_prefix('#')?;
    let rest = rest.trim_start();
    let payload = rest.strip_prefix("@probe")?;
    let payload = payload.trim_start();
    let (k, v) = payload.split_once('=')?;
    let key = k.trim().to_string();
    let val = v.trim().to_string();
    if key.is_empty() {
        return None;
    }
    Some((key, val))
}

/// `[a-z][a-z0-9_-]{0..MAX_NAME_LEN-1}`. First char must be a lowercase
/// letter so probe names sort cleanly and never collide with reserved
/// identifiers (`-` or numeric leading would).
fn is_valid_name(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_NAME_LEN {
        return false;
    }
    let mut chars = s.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Parse `Ns` / `Nm` / `Nh`. Plain integers default to seconds — `60` and
/// `60s` both work. We don't accept fractional values; sub-second probe
/// schedules are not a thing we support.
fn parse_interval(s: &str) -> Result<Duration, ManifestError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ManifestError::Validation("interval: empty string".into()));
    }
    let (num_part, multiplier) = if let Some(rest) = s.strip_suffix('s') {
        (rest, 1u64)
    } else if let Some(rest) = s.strip_suffix('m') {
        (rest, 60u64)
    } else if let Some(rest) = s.strip_suffix('h') {
        (rest, 3_600u64)
    } else {
        (s, 1u64)
    };
    let n: u64 = num_part.trim().parse().map_err(|_| {
        ManifestError::Validation(format!(
            "interval: '{}' is not a number with optional s/m/h suffix",
            s
        ))
    })?;
    if n == 0 {
        return Err(ManifestError::Validation(
            "interval: zero is not a valid period".into(),
        ));
    }
    let secs = n.checked_mul(multiplier).ok_or_else(|| {
        ManifestError::Validation(format!("interval: '{}' overflows u64 seconds", s))
    })?;
    Ok(Duration::from_secs(secs))
}

/// Cron expression. The `cron` crate expects 6 fields (sec min hour dom
/// mon dow). Most users write 5-field "*/5 * * * *" — we accept both by
/// prepending "0" (fire at second 0) when only 5 are supplied.
fn parse_cron(s: &str) -> Result<Schedule, ManifestError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ManifestError::Validation(
            "schedule: empty cron string".into(),
        ));
    }
    let normalized = if s.split_whitespace().count() == 5 {
        format!("0 {}", s)
    } else {
        s.to_string()
    };
    cron::Schedule::from_str(&normalized)
        .map(Schedule::Cron)
        .map_err(|e| {
            ManifestError::Validation(format!("schedule '{}' is not a valid cron: {}", s, e))
        })
}

fn current_platform() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "freebsd") {
        "freebsd"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_charset() {
        assert!(is_valid_name("foo"));
        assert!(is_valid_name("foo-bar_baz"));
        assert!(is_valid_name("a"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("Foo")); // uppercase
        assert!(!is_valid_name("1foo")); // leading digit
        assert!(!is_valid_name("-foo")); // leading dash
        assert!(!is_valid_name("foo bar")); // space
        assert!(!is_valid_name(&"x".repeat(MAX_NAME_LEN + 1)));
    }

    #[test]
    fn interval_parsing() {
        assert_eq!(parse_interval("60").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_interval("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_interval("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_interval("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_interval("0").is_err());
        assert!(parse_interval("").is_err());
        assert!(parse_interval("abc").is_err());
        assert!(parse_interval("60x").is_err());
    }

    #[test]
    fn cron_5field_normalises_to_6() {
        let s = parse_cron("*/5 * * * *").expect("5-field cron should parse");
        match s {
            Schedule::Cron(_) => {}
            _ => panic!("expected cron"),
        }
    }

    #[test]
    fn schedule_or_interval_required() {
        let raw = RawManifest {
            name: "foo".into(),
            description: None,
            enabled: false,
            schedule: None,
            interval: None,
            timeout_ms: None,
            command: vec!["/bin/true".into()],
            platforms: vec![],
            env: Default::default(),
            run_as_user: None,
            memory_limit_mb: None,
            mode: None,
        };
        let err = Manifest::validate(raw, "abc".into(), PathBuf::from("test.yaml")).unwrap_err();
        assert!(matches!(err, ManifestError::Validation(_)));
    }

    #[test]
    fn schedule_and_interval_are_exclusive() {
        let raw = RawManifest {
            name: "foo".into(),
            description: None,
            enabled: false,
            schedule: Some("*/5 * * * *".into()),
            interval: Some("60s".into()),
            timeout_ms: None,
            command: vec!["/bin/true".into()],
            platforms: vec![],
            env: Default::default(),
            run_as_user: None,
            memory_limit_mb: None,
            mode: None,
        };
        let err = Manifest::validate(raw, "abc".into(), PathBuf::from("test.yaml")).unwrap_err();
        assert!(matches!(err, ManifestError::Validation(_)));
    }

    #[test]
    fn empty_command_rejected() {
        let raw = RawManifest {
            name: "foo".into(),
            description: None,
            enabled: false,
            schedule: None,
            interval: Some("60s".into()),
            timeout_ms: None,
            command: vec![],
            platforms: vec![],
            env: Default::default(),
            run_as_user: None,
            memory_limit_mb: None,
            mode: None,
        };
        assert!(Manifest::validate(raw, "abc".into(), PathBuf::from("test.yaml")).is_err());
    }

    #[test]
    fn timeout_bracketed() {
        let mk = |to: u64| RawManifest {
            name: "foo".into(),
            description: None,
            enabled: false,
            schedule: None,
            interval: Some("60s".into()),
            timeout_ms: Some(to),
            command: vec!["/bin/true".into()],
            platforms: vec![],
            env: Default::default(),
            run_as_user: None,
            memory_limit_mb: None,
            mode: None,
        };
        assert!(Manifest::validate(mk(50), "h".into(), PathBuf::from("p")).is_err());
        assert!(
            Manifest::validate(mk(MAX_TIMEOUT_MS + 1), "h".into(), PathBuf::from("p")).is_err()
        );
        assert!(Manifest::validate(mk(5_000), "h".into(), PathBuf::from("p")).is_ok());
    }

    #[test]
    fn probe_mode_parsing() {
        assert_eq!(ProbeMode::parse("oneshot"), Some(ProbeMode::Oneshot));
        assert_eq!(ProbeMode::parse("ONESHOT"), Some(ProbeMode::Oneshot));
        assert_eq!(ProbeMode::parse("one-shot"), Some(ProbeMode::Oneshot));
        assert_eq!(ProbeMode::parse("one_shot"), Some(ProbeMode::Oneshot));
        assert_eq!(ProbeMode::parse("stream"), Some(ProbeMode::Stream));
        assert_eq!(ProbeMode::parse("streaming"), Some(ProbeMode::Stream));
        assert_eq!(ProbeMode::parse("Stream"), Some(ProbeMode::Stream));
        assert!(ProbeMode::parse("unknown").is_none());
        assert!(ProbeMode::parse("").is_none());
    }

    #[test]
    fn mode_default_oneshot_when_unspecified() {
        let raw = RawManifest {
            name: "foo".into(),
            description: None,
            enabled: false,
            schedule: None,
            interval: Some("60s".into()),
            timeout_ms: None,
            command: vec!["/bin/true".into()],
            platforms: vec![],
            env: Default::default(),
            run_as_user: None,
            memory_limit_mb: None,
            mode: None,
        };
        let m = Manifest::validate(raw, "h".into(), PathBuf::from("p")).expect("validate");
        assert_eq!(m.mode, ProbeMode::Oneshot);
    }

    #[test]
    fn mode_stream_validates() {
        let raw = RawManifest {
            name: "foo".into(),
            description: None,
            enabled: false,
            schedule: None,
            interval: Some("60s".into()),
            timeout_ms: None,
            command: vec!["/bin/true".into()],
            platforms: vec![],
            env: Default::default(),
            run_as_user: None,
            memory_limit_mb: None,
            mode: Some("stream".into()),
        };
        let m = Manifest::validate(raw, "h".into(), PathBuf::from("p")).expect("validate");
        assert_eq!(m.mode, ProbeMode::Stream);
    }

    #[test]
    fn mode_invalid_rejected() {
        let raw = RawManifest {
            name: "foo".into(),
            description: None,
            enabled: false,
            schedule: None,
            interval: Some("60s".into()),
            timeout_ms: None,
            command: vec!["/bin/true".into()],
            platforms: vec![],
            env: Default::default(),
            run_as_user: None,
            memory_limit_mb: None,
            mode: Some("garbage".into()),
        };
        assert!(Manifest::validate(raw, "h".into(), PathBuf::from("p")).is_err());
    }

    #[test]
    fn extract_header_kv_basic() {
        assert_eq!(
            extract_header_kv("# @probe name=foo"),
            Some(("name".into(), "foo".into()))
        );
        assert_eq!(
            extract_header_kv("  # @probe interval=60s"),
            Some(("interval".into(), "60s".into()))
        );
        // Quoted spaces in value survive (they're a single value).
        assert_eq!(
            extract_header_kv("# @probe description=hello world"),
            Some(("description".into(), "hello world".into()))
        );
        // Lines that aren't @probe lines are ignored.
        assert_eq!(extract_header_kv("# just a comment"), None);
        assert_eq!(extract_header_kv("echo hi"), None);
        assert_eq!(extract_header_kv(""), None);
        // Missing `=` is invalid.
        assert_eq!(extract_header_kv("# @probe enabled"), None);
    }

    #[test]
    fn inline_header_minimal() {
        let script = b"#!/bin/sh\n# @probe name=disk-free\n# @probe interval=60s\necho hi\n";
        let raw = parse_inline_header(script, &PathBuf::from("/x/disk-free.sh")).expect("parse");
        assert_eq!(raw.name, "disk-free");
        assert_eq!(raw.interval.as_deref(), Some("60s"));
        // Default command points at the script itself.
        assert_eq!(raw.command, vec!["/x/disk-free.sh".to_string()]);
        // Defaults
        assert!(!raw.enabled);
        assert!(raw.platforms.is_empty());
    }

    #[test]
    fn inline_header_full() {
        let script = b"\
#!/usr/bin/env bash
# @probe name=fail2ban-banned
# @probe description=Banned IPs in default jail
# @probe enabled=true
# @probe schedule=*/5 * * * *
# @probe timeout_ms=15000
# @probe platforms=linux,freebsd
# @probe run_as_user=nobody
echo running
";
        let raw = parse_inline_header(script, &PathBuf::from("p.sh")).expect("parse");
        assert_eq!(raw.name, "fail2ban-banned");
        assert!(raw.enabled);
        assert_eq!(raw.schedule.as_deref(), Some("*/5 * * * *"));
        assert_eq!(raw.timeout_ms, Some(15_000));
        assert_eq!(
            raw.platforms,
            vec!["linux".to_string(), "freebsd".to_string()]
        );
        assert_eq!(raw.run_as_user.as_deref(), Some("nobody"));
    }

    #[test]
    fn inline_header_command_override_with_args() {
        let script = b"#!/usr/bin/env python3\n# @probe name=py\n# @probe interval=60s\n# @probe command=/usr/bin/python3 -u /opt/me.py --jail sshd\n";
        let raw = parse_inline_header(script, &PathBuf::from("py.py")).expect("parse");
        assert_eq!(
            raw.command,
            vec!["/usr/bin/python3", "-u", "/opt/me.py", "--jail", "sshd"]
        );
    }

    #[test]
    fn inline_header_unknown_key_rejected() {
        let script = b"# @probe name=foo\n# @probe nonsense=bar\n";
        let err = parse_inline_header(script, &PathBuf::from("x.sh")).unwrap_err();
        assert!(matches!(err, ManifestError::Validation(_)));
    }

    #[test]
    fn inline_header_stops_at_first_code_line() {
        // The `name=second` header sits AFTER an executable line and
        // must not be picked up.
        let script = b"# @probe name=first\necho before-headers\n# @probe enabled=true\n";
        let raw = parse_inline_header(script, &PathBuf::from("x.sh")).expect("parse");
        assert_eq!(raw.name, "first");
        assert!(!raw.enabled); // second header was ignored
    }

    #[test]
    fn inline_header_no_headers_at_all_rejected() {
        let script = b"#!/bin/sh\n# just a normal comment\necho hi\n";
        let err = parse_inline_header(script, &PathBuf::from("x.sh")).unwrap_err();
        assert!(matches!(err, ManifestError::Parse(_)));
    }
}
