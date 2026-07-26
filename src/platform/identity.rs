//! What this process is, from the point of view of the things it can control.
//!
//! The control endpoints are generic: `DELETE /processes/{pid}` takes any pid,
//! `POST /services/{name}/stop` takes any unit. Both can name this server, and
//! when they do it is almost always a mistake — an operator scanning the
//! process list they are being shown, killing the one at the top. Answering
//! "is that me?" is what lets those endpoints refuse.
//!
//! Deliberately stopping the server is a separate, explicit endpoint. This
//! module exists so the accidental path and the intentional one stay distinct.

use std::sync::OnceLock;

/// The unit / service name this process is running under, if any. Resolved
/// once — it cannot change for the life of the process.
static SERVICE_NAME: OnceLock<Option<String>> = OnceLock::new();

/// Config override, applied before the first resolution.
static CONFIGURED_NAME: OnceLock<String> = OnceLock::new();

/// Record the operator's explicit answer. The installer knows the unit name it
/// wrote, so it can say so and skip the guessing below entirely. Call before
/// anything reads `service_name`.
pub fn set_service_name(name: &str) {
    let trimmed = name.trim();
    if !trimmed.is_empty() {
        let _ = CONFIGURED_NAME.set(trimmed.to_string());
    }
}

/// This process's own pid.
pub fn pid() -> u32 {
    std::process::id()
}

/// The service unit that supervises this process, lowercased and stripped of
/// its `.service` suffix so it compares cleanly against a caller-supplied
/// name. `None` when running outside a supervisor — a bare `./remon-server`,
/// or a platform we cannot ask.
pub fn service_name() -> Option<&'static str> {
    SERVICE_NAME.get_or_init(resolve_service_name).as_deref()
}

fn resolve_service_name() -> Option<String> {
    if let Some(configured) = CONFIGURED_NAME.get() {
        return Some(normalize(configured));
    }
    detect_service_name().map(|n| normalize(&n))
}

/// Compare a caller-supplied service name against our own. Both sides are
/// normalised, so `Remon-Server.service` matches `remon-server`.
pub fn is_own_service(name: &str) -> bool {
    matches_service(service_name(), name)
}

/// The comparison itself, with our identity passed in rather than read from
/// the process. Keeps the decision testable — the resolved name is a
/// `OnceLock` that the first caller fixes for the life of the process, which
/// is not something a parallel test suite can set up per case.
///
/// An unknown identity matches nothing: running outside a supervisor, there is
/// no unit that could name us, so no request can be self-targeting.
fn matches_service(own: Option<&str>, requested: &str) -> bool {
    match own {
        Some(own) => normalize(requested) == own,
        None => false,
    }
}

/// True when the pid is this process. Under a supervisor the agent would come
/// back, but the request still drops monitoring for the restart window and
/// writes a misleading audit row, and the caller almost never meant it.
pub fn is_own_pid(pid_to_check: u32) -> bool {
    pid_to_check == pid()
}

/// Unit names are case-insensitive in practice and the `.service` suffix is
/// optional everywhere it appears, so fold both away before comparing.
fn normalize(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    lower.strip_suffix(".service").unwrap_or(&lower).to_string()
}

/// OpenRC exports the service name into the daemon's environment, and systemd
/// records the unit in the process's cgroup path. Neither is available when
/// the binary is run by hand, which is the case where there is nothing to
/// protect anyway.
#[cfg(target_os = "linux")]
fn detect_service_name() -> Option<String> {
    // OpenRC: set for every service it starts.
    if let Ok(name) = std::env::var("RC_SVCNAME")
        && !name.trim().is_empty()
    {
        return Some(name);
    }

    // systemd: /proc/self/cgroup ends with the unit under cgroup v2, e.g.
    //   0::/system.slice/remon-server.service
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    cgroup.lines().find_map(|line| {
        let path = line.rsplit(':').next()?;
        path.rsplit('/')
            .find(|segment| segment.ends_with(".service"))
            .map(str::to_string)
    })
}

#[cfg(not(target_os = "linux"))]
fn detect_service_name() -> Option<String> {
    // The Windows install is a scheduled task, not an SCM service, so there is
    // no unit for /services/{name} to name in the first place.
    std::env::var("RC_SVCNAME")
        .ok()
        .filter(|n| !n.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_and_case_do_not_defeat_the_comparison() {
        assert_eq!(normalize("remon-server.service"), "remon-server");
        assert_eq!(normalize("Remon-Server.SERVICE"), "remon-server");
        assert_eq!(normalize("  remon-server  "), "remon-server");
    }

    #[test]
    fn a_request_naming_our_unit_is_self_targeting() {
        let own = Some("remon-server");
        assert!(matches_service(own, "remon-server"));
        assert!(matches_service(own, "remon-server.service"));
        assert!(matches_service(own, "Remon-Server.SERVICE"));
    }

    #[test]
    fn a_request_naming_another_unit_passes_through() {
        let own = Some("remon-server");
        assert!(!matches_service(own, "sshd"));
        assert!(!matches_service(own, "sshd.service"));
        // Not a prefix match: a real unit that merely starts the same way
        // must still be controllable.
        assert!(!matches_service(own, "remon-server-exporter"));
    }

    #[test]
    fn nothing_is_self_targeting_when_we_have_no_unit() {
        // A bare `./remon-server` is not supervised, so no unit name can
        // refer to it and every request is about something else.
        assert!(!matches_service(None, "remon-server"));
    }

    #[test]
    fn our_own_pid_is_recognised() {
        assert!(is_own_pid(std::process::id()));
        // pid 0 is never a real process; whatever we are, we are not it.
        assert!(!is_own_pid(0));
    }

    /// The systemd cgroup line is the only detection path with real parsing in
    /// it, so exercise the shape it actually produces.
    #[test]
    fn the_unit_is_read_out_of_a_cgroup_v2_line() {
        let extract = |cgroup: &str| -> Option<String> {
            cgroup.lines().find_map(|line| {
                let path = line.rsplit(':').next()?;
                path.rsplit('/')
                    .find(|segment| segment.ends_with(".service"))
                    .map(str::to_string)
            })
        };

        assert_eq!(
            extract("0::/system.slice/remon-server.service"),
            Some("remon-server.service".to_string())
        );
        // cgroup v1 emits several lines; the unit is still in there.
        assert_eq!(
            extract(
                "12:pids:/system.slice/remon-server.service\n0::/system.slice/remon-server.service"
            ),
            Some("remon-server.service".to_string())
        );
        // A process started by hand sits in a user slice with no unit.
        assert_eq!(
            extract("0::/user.slice/user-1000.slice/session-3.scope"),
            None
        );
    }
}
