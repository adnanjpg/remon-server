//! Where the server reads config from and writes state to.
//!
//! Everything used to be resolved relative to the working directory, which
//! only holds when the process is started from a repo checkout. An installed
//! daemon has no meaningful working directory, so the layout is resolved once
//! at startup and read from here afterwards.
//!
//! Three layouts, picked in this order:
//!
//! 1. **Explicit** — `--config-dir` / `--data-dir`, or `REMON_CONFIG_DIR` /
//!    `REMON_DATA_DIR`. Always wins.
//! 2. **Checkout** — the working directory contains `config/default.toml`.
//!    Resolves to `./config`, `./db`, `./probes` exactly as before, so
//!    `cargo run` in the repo is unchanged.
//! 3. **Installed** — `/etc/remon` + `/var/lib/remon` for a privileged
//!    process, per-user XDG directories otherwise, `%ProgramData%\remon` on
//!    Windows.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Resolved filesystem layout. Immutable for the life of the process.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Holds `config.toml` / `<RUN_ENV>.toml` overrides. May not exist —
    /// the defaults are compiled in, so every config file is optional.
    pub config_dir: PathBuf,
    /// Root for anything the server writes: database, WAL, generated keys.
    pub data_dir: PathBuf,
    /// Custom probe scripts.
    pub probes_dir: PathBuf,
}

static PATHS: OnceLock<Paths> = OnceLock::new();

/// Marker that identifies a repo checkout. Also the file whose contents are
/// compiled into the binary, so its presence means "this working directory
/// is the source of truth, prefer it over system locations".
const CHECKOUT_MARKER: &str = "config/default.toml";

/// Resolve the layout and publish it. Call once, before anything reads a
/// path. Later calls are ignored, so the first resolution wins.
pub fn init(
    config_dir_override: Option<PathBuf>,
    data_dir_override: Option<PathBuf>,
) -> &'static Paths {
    let resolved = resolve(config_dir_override, data_dir_override);
    let _ = PATHS.set(resolved);
    get()
}

/// The resolved layout. Falls back to resolving with no overrides if `init`
/// was never called, which keeps unit tests and `#[tokio::test]` harnesses
/// working without a startup hook.
pub fn get() -> &'static Paths {
    PATHS.get_or_init(|| resolve(None, None))
}

fn resolve(config_dir_override: Option<PathBuf>, data_dir_override: Option<PathBuf>) -> Paths {
    let checkout = is_checkout();

    let explicit_config_dir = config_dir_override.or_else(|| env_dir("REMON_CONFIG_DIR"));
    let config_dir = explicit_config_dir.clone().unwrap_or_else(|| {
        if checkout {
            PathBuf::from("config")
        } else {
            system_config_dir()
        }
    });

    let data_dir = data_dir_override
        .or_else(|| env_dir("REMON_DATA_DIR"))
        .unwrap_or_else(|| {
            if checkout {
                // The checkout's data root is the checkout itself, so the
                // relative `database.path` default lands on `./db` as always.
                PathBuf::from(".")
            } else {
                system_data_dir()
            }
        });

    // Probes are operator-authored scripts — configuration, not state — so
    // they live beside the config, and follow it when it is pointed
    // elsewhere. Only an unredirected checkout keeps them at the repo root.
    let probes_dir = env_dir("REMON_PROBES_DIR").unwrap_or_else(|| {
        if checkout && explicit_config_dir.is_none() {
            PathBuf::from("probes")
        } else {
            config_dir.join("probes")
        }
    });

    Paths {
        config_dir,
        data_dir,
        probes_dir,
    }
}

/// True when the working directory looks like a repo checkout.
fn is_checkout() -> bool {
    Path::new(CHECKOUT_MARKER).is_file()
}

/// Read a directory override from the environment, ignoring empty values so
/// that `REMON_DATA_DIR=` behaves like "unset" rather than "the root".
fn env_dir(key: &str) -> Option<PathBuf> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

#[cfg(windows)]
fn system_config_dir() -> PathBuf {
    program_data().join("remon")
}

#[cfg(windows)]
fn system_data_dir() -> PathBuf {
    program_data().join("remon")
}

#[cfg(windows)]
fn program_data() -> PathBuf {
    std::env::var("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\ProgramData"))
}

#[cfg(unix)]
fn system_config_dir() -> PathBuf {
    if is_privileged() {
        PathBuf::from("/etc/remon")
    } else {
        xdg_dir("XDG_CONFIG_HOME", ".config")
    }
}

#[cfg(unix)]
fn system_data_dir() -> PathBuf {
    if is_privileged() {
        PathBuf::from("/var/lib/remon")
    } else {
        xdg_dir("XDG_DATA_HOME", ".local/share")
    }
}

/// Running as root. The service endpoints, process control and smartctl all
/// need it, so this is the normal case for an installed daemon — but an
/// unprivileged run must not try to write to `/var/lib`.
#[cfg(unix)]
fn is_privileged() -> bool {
    // SAFETY: geteuid is always safe — no arguments, no side effects.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(unix)]
fn xdg_dir(env_key: &str, home_relative: &str) -> PathBuf {
    if let Some(base) = env_dir(env_key) {
        return base.join("remon");
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => PathBuf::from(home).join(home_relative).join("remon"),
        // No HOME and not root: nothing better than the working directory.
        _ => PathBuf::from("."),
    }
}

impl Paths {
    /// Resolve a configured path against the data directory. Absolute paths
    /// are honoured as given, so an operator can point the database at a
    /// separate volume without touching the rest of the layout.
    pub fn resolve_data<P: AsRef<Path>>(&self, configured: P) -> PathBuf {
        let p = configured.as_ref();
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            tidy(&self.data_dir.join(p))
        }
    }
}

/// Drop redundant `.` components so a joined path reads like something a
/// person would have typed. Purely cosmetic — these paths end up in log lines
/// and `config check` output, and `.\./db/monitor.sqlite3` invites a bug
/// report. Nothing is resolved against the filesystem, so symlinks and `..`
/// are left exactly as configured.
fn tidy(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_configured_paths_ignore_the_data_dir() {
        let paths = Paths {
            config_dir: PathBuf::from("/etc/remon"),
            data_dir: PathBuf::from("/var/lib/remon"),
            probes_dir: PathBuf::from("/etc/remon/probes"),
        };

        #[cfg(unix)]
        let absolute = "/srv/data/monitor.sqlite3";
        #[cfg(windows)]
        let absolute = r"D:\data\monitor.sqlite3";

        assert_eq!(paths.resolve_data(absolute), PathBuf::from(absolute));
    }

    #[test]
    fn relative_configured_paths_land_under_the_data_dir() {
        let paths = Paths {
            config_dir: PathBuf::from("/etc/remon"),
            data_dir: PathBuf::from("/var/lib/remon"),
            probes_dir: PathBuf::from("/etc/remon/probes"),
        };

        assert_eq!(
            paths.resolve_data("./db/monitor.sqlite3"),
            PathBuf::from("/var/lib/remon/db/monitor.sqlite3")
        );
    }

    #[test]
    fn joined_paths_drop_redundant_current_dir_components() {
        let paths = Paths {
            config_dir: PathBuf::from("config"),
            data_dir: PathBuf::from("."),
            probes_dir: PathBuf::from("probes"),
        };

        assert_eq!(
            paths.resolve_data("./db/monitor.sqlite3"),
            PathBuf::from("db").join("monitor.sqlite3")
        );
    }

    #[test]
    fn a_path_that_tidies_away_entirely_stays_usable() {
        assert_eq!(tidy(Path::new("./")), PathBuf::from("."));
    }

    #[test]
    fn probes_follow_an_explicitly_pointed_config_dir() {
        // Redirecting the config dir has to take the probes with it, or a
        // checkout-shaped working directory silently keeps serving its own
        // `./probes` to an install that was pointed somewhere else.
        let redirected = resolve(Some(PathBuf::from("/etc/remon")), None);
        assert_eq!(
            redirected.probes_dir,
            PathBuf::from("/etc/remon").join("probes")
        );
    }

    #[test]
    fn empty_env_override_is_treated_as_unset() {
        // SAFETY: single-threaded test, no other thread reads the env here.
        unsafe { std::env::set_var("REMON_TEST_EMPTY_DIR", "   ") };
        assert!(env_dir("REMON_TEST_EMPTY_DIR").is_none());
        unsafe { std::env::remove_var("REMON_TEST_EMPTY_DIR") };
    }
}
