//! `remon-server doctor` — one screen answering "will this host run me, and
//! where did I decide to put things?".
//!
//! The questions it answers are the ones that otherwise turn into a support
//! round trip: which config file actually got read, whether the port is free,
//! whether the data directory is writable, whether the optional external
//! tools are present. It never mutates anything — no database is created, no
//! collector runs — so it is safe against a live install.

use std::fmt;
use std::net::TcpListener;
use std::path::Path;

use colored::Colorize;

use crate::config::Config;
use crate::paths::Paths;

/// Verdict for a single check. `Warn` never fails the run: a missing
/// `smartctl` or an unreachable Docker socket degrades a feature, it does not
/// stop the server.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Verdict {
    Ok,
    Warn,
    Fail,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Verdict::Ok => "  ok  ".green(),
            Verdict::Warn => " warn ".yellow(),
            Verdict::Fail => " fail ".red(),
        };
        write!(f, "[{}]", s)
    }
}

struct Report {
    failed: bool,
}

impl Report {
    fn new() -> Self {
        Self { failed: false }
    }

    fn line(&mut self, verdict: Verdict, label: &str, detail: impl AsRef<str>) {
        if verdict == Verdict::Fail {
            self.failed = true;
        }
        println!("{} {:<22} {}", verdict, label, detail.as_ref());
    }

    fn section(&self, title: &str) {
        println!("\n{}", title.bold());
    }
}

/// Run every check and return whether the install looks serviceable.
/// `Err` means a hard failure was found; the message has already been
/// printed, so the caller only needs the exit code.
pub fn run() -> bool {
    let paths = crate::paths::get();
    let mut report = Report::new();

    println!(
        "{} {}",
        "remon-server".bold(),
        env!("CARGO_PKG_VERSION").dimmed()
    );

    report.section("paths");
    check_dir(&mut report, "config dir", &paths.config_dir, false);
    check_dir(&mut report, "data dir", &paths.data_dir, true);
    check_dir(&mut report, "probes dir", &paths.probes_dir, false);

    report.section("configuration");
    let config = match Config::load(&paths.config_dir) {
        Ok(cfg) => {
            report.line(Verdict::Ok, "loads", "configuration parsed");
            Some(cfg)
        }
        Err(e) => {
            report.line(Verdict::Fail, "loads", e.to_string());
            None
        }
    };

    if let Some(cfg) = &config {
        check_config(&mut report, cfg, paths);
    }

    report.section("host");
    check_privileges(&mut report);
    report.line(
        Verdict::Ok,
        "init system",
        format!("{:?}", crate::platform::init::detect()),
    );
    check_smartctl(&mut report, config.as_ref());
    check_docker(&mut report, config.as_ref());

    if report.failed {
        println!("\n{}", "one or more checks failed".red().bold());
    } else {
        println!("\n{}", "no blocking problems found".green().bold());
    }
    !report.failed
}

/// A config directory that does not exist is fine — the defaults are compiled
/// in. A data directory that cannot be written to is fatal.
fn check_dir(report: &mut Report, label: &str, dir: &Path, must_write: bool) {
    let shown = dir.display().to_string();

    if !dir.exists() {
        let verdict = if must_write {
            // Created at boot, so only the parent has to be reachable.
            match dir.parent() {
                Some(parent) if parent.exists() || parent.as_os_str().is_empty() => Verdict::Warn,
                _ => Verdict::Fail,
            }
        } else {
            Verdict::Warn
        };
        report.line(verdict, label, format!("{shown} (does not exist yet)"));
        return;
    }

    if must_write {
        match write_probe(dir) {
            Ok(()) => report.line(Verdict::Ok, label, format!("{shown} (writable)")),
            Err(e) => report.line(Verdict::Fail, label, format!("{shown} — not writable: {e}")),
        }
    } else {
        report.line(Verdict::Ok, label, shown);
    }
}

/// Prove writability by actually writing, since permission bits alone lie
/// about read-only mounts and full filesystems.
fn write_probe(dir: &Path) -> std::io::Result<()> {
    let probe = dir.join(format!(".remon-doctor-{}", std::process::id()));
    std::fs::write(&probe, b"")?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

fn check_config(report: &mut Report, cfg: &Config, paths: &Paths) {
    match cfg.server.bind_addr() {
        Ok(addr) => {
            report.line(Verdict::Ok, "bind address", addr.to_string());
            match TcpListener::bind(addr) {
                Ok(listener) => {
                    drop(listener);
                    report.line(Verdict::Ok, "port", format!("{} is free", addr.port()));
                }
                Err(e) => report.line(
                    Verdict::Fail,
                    "port",
                    format!("cannot bind {addr}: {e} (already running?)"),
                ),
            }
        }
        Err(e) => report.line(Verdict::Fail, "bind address", e.to_string()),
    }

    let db_path = paths.resolve_data(&cfg.database.path);
    let db_state = if db_path.exists() {
        "existing"
    } else {
        "will be created"
    };
    report.line(
        Verdict::Ok,
        "database",
        format!("{} ({db_state})", db_path.display()),
    );

    // The placeholder means "generate and persist one on first boot", which
    // is the recommended setup, not a problem.
    if cfg.auth.jwt_secret.trim().len() >= 32 {
        report.line(Verdict::Ok, "jwt secret", "explicit secret configured");
    } else {
        report.line(
            Verdict::Ok,
            "jwt secret",
            "per-install secret (generated on first boot)",
        );
    }

    if cfg.cors.allow_any_origin {
        report.line(
            Verdict::Warn,
            "cors",
            "any origin allowed — intended for local development only",
        );
    } else if cfg.cors.allowed_origins.is_empty() {
        report.line(
            Verdict::Warn,
            "cors",
            "no browser origin allowed; native clients unaffected",
        );
    } else {
        report.line(
            Verdict::Ok,
            "cors",
            format!("{} origin(s) allowed", cfg.cors.allowed_origins.len()),
        );
    }

    report.line(
        Verdict::Ok,
        "logging",
        format!("{} / {}", cfg.logging.level, cfg.logging.format),
    );
}

#[cfg(unix)]
fn check_privileges(report: &mut Report) {
    // SAFETY: geteuid takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        report.line(Verdict::Ok, "privileges", "running as root");
    } else {
        report.line(
            Verdict::Warn,
            "privileges",
            format!(
                "running as uid {euid} — service control, process kill and SMART reads need root"
            ),
        );
    }
}

#[cfg(windows)]
fn check_privileges(report: &mut Report) {
    // Elevation is awkward to probe without extra API surface, and the
    // failure mode is a clear per-endpoint error rather than a broken boot.
    report.line(
        Verdict::Ok,
        "privileges",
        "service control and SMART reads need an elevated process",
    );
}

fn check_smartctl(report: &mut Report, cfg: Option<&Config>) {
    let configured = cfg.map(|c| c.smart.smartctl_path.trim()).unwrap_or("");
    let enabled = cfg.map(|c| c.smart.enabled).unwrap_or(true);

    if !enabled {
        report.line(Verdict::Ok, "smartctl", "disabled by config");
        return;
    }

    let bin = if configured.is_empty() {
        "smartctl"
    } else {
        configured
    };

    match std::process::Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) if status.success() => {
            report.line(Verdict::Ok, "smartctl", format!("{bin} available"))
        }
        _ => report.line(
            Verdict::Warn,
            "smartctl",
            "not found — disk health off (install smartmontools)",
        ),
    }
}

#[cfg(feature = "docker")]
fn check_docker(report: &mut Report, cfg: Option<&Config>) {
    let configured = cfg.map(|c| c.docker.socket_path.trim()).unwrap_or("");
    if !configured.is_empty() {
        let exists = Path::new(configured).exists();
        let verdict = if exists { Verdict::Ok } else { Verdict::Warn };
        let state = if exists { "present" } else { "missing" };
        report.line(verdict, "docker socket", format!("{configured} ({state})"));
        return;
    }

    // Platform defaults, in the order bollard would try them.
    let candidates = ["/var/run/docker.sock", "/run/podman/podman.sock"];
    match candidates.iter().find(|p| Path::new(p).exists()) {
        Some(found) => report.line(Verdict::Ok, "docker socket", *found),
        None => report.line(
            Verdict::Warn,
            "docker socket",
            "none found — container endpoints will be unavailable",
        ),
    }
}

#[cfg(not(feature = "docker"))]
fn check_docker(report: &mut Report, _cfg: Option<&Config>) {
    report.line(Verdict::Ok, "docker", "compiled out");
}
