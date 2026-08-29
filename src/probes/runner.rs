//! Runner — spawns a probe's command and consumes its stdout.
//!
//! Two execution shapes:
//!
//! - `execute(...)` — oneshot. Wait for the child to finish (or the
//!   wall-clock `timeout_ms` to fire), then parse the LAST JSON line of
//!   stdout. Returns `(ProbeRun, Vec<ProbeMetric>)`.
//!
//! - `execute_stream(..., tx)` — long-running. The child stays alive
//!   and emits newline-delimited JSON. Each line becomes its own
//!   `(ProbeRun, Vec<ProbeMetric>)` pushed onto `tx`. `timeout_ms` is
//!   reinterpreted as a per-line idle deadline: if the script goes
//!   silent for that long, we kill it and let the schedule restart.
//!
//! Severity is never computed here. "Warn / crit" is the alert engine's
//! job — see `services/alerts.rs` and `alert_rules.metric_type='probe'`.
//!
//! Underneath both sits `execute_capture(&ExecSpec)` — spawn, drain, wait,
//! kill-on-timeout, with no opinion about what the output means. Remediation
//! actions (`services/actions.rs`) run their scripts through it too, so the
//! process sandbox has one implementation rather than two that drift.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::{Duration, Instant};

use log::warn;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::models::probe::{ProbeMetric, ProbeOutput, ProbeRun};

use super::manifest::Manifest;

/// How much of a child's stdout we keep. Probes shouldn't print megabytes,
/// but the drain never stops early regardless — see `read_to_string_capped`.
pub const STDOUT_CAP: usize = 64 * 1024;
/// Same for stderr, which we only ever quote a tail of.
pub const STDERR_CAP: usize = 8 * 1024;

/// The subset of a manifest the process layer actually needs: what to run and
/// under what limits. Probes and remediation actions both hand one of these
/// down, so the sandbox (new process group, address-space cap, privilege
/// drop, kill-on-timeout) is written once and both inherit every fix to it.
pub struct ExecSpec<'a> {
    /// Only used in log lines — the manifest's name.
    pub name: &'a str,
    pub command: &'a [String],
    pub env: &'a HashMap<String, String>,
    pub timeout: Duration,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub run_as_user: Option<&'a str>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub memory_limit_mb: Option<u64>,
}

/// Raw outcome of running one child to completion — no interpretation.
/// The probe path turns this into a `ProbeRun` + metrics; the action path
/// turns it into an `ExecutionResult`.
pub struct Capture {
    pub duration_ms: i64,
    /// `None` when the child was killed (timeout) or never started.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    /// Set when the child never started at all — empty argv, a `run_as_user`
    /// that isn't on this host, or a failed `spawn()`. Distinct from "ran and
    /// failed": there is no exit code to report and nothing was executed.
    pub spawn_error: Option<String>,
}

impl Capture {
    fn not_started(duration_ms: i64, message: String) -> Self {
        Self {
            duration_ms,
            exit_code: None,
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
            spawn_error: Some(message),
        }
    }

    /// Best one-line explanation of a non-success, for a run row's `message`.
    /// Prefers the spawn error, then the timeout, then the stderr tail.
    pub fn failure_message(&self) -> String {
        if let Some(e) = &self.spawn_error {
            return e.clone();
        }
        if self.timed_out {
            return format!("timed out after {}ms", self.duration_ms);
        }
        let code = self
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into());
        let tail = self.stderr.trim();
        if tail.is_empty() {
            format!("exit {}", code)
        } else {
            format!("exit {}: {}", code, truncate(tail, 200))
        }
    }

    /// Bounded tail of whatever the child said, stderr preferred — that is
    /// where a failing script explains itself.
    pub fn output_tail(&self, max: usize) -> Option<String> {
        let text = if self.stderr.trim().is_empty() {
            self.stdout.trim()
        } else {
            self.stderr.trim()
        };
        (!text.is_empty()).then(|| truncate(text, max))
    }
}

/// Configure a `tokio::process::Command` — common to oneshot, stream and
/// action paths. Sets piped stdio, env, kill-on-drop, and (on Unix) installs
/// the pre_exec hook for setpgid/setrlimit/setuid.
fn configure_command(spec: &ExecSpec<'_>, kill_on_drop: bool) -> Result<Command, String> {
    let exe = spec
        .command
        .first()
        .ok_or_else(|| "command argv is empty".to_string())?;
    let args = &spec.command[1..];

    let mut cmd = Command::new(exe);
    cmd.args(args);
    cmd.envs(spec.env);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(kill_on_drop);

    // Unix-only: install a pre_exec hook that runs in the child after
    // fork but before exec. We pack three things in there:
    //   1. setpgid(0, 0) — new process group so kill-on-timeout signals
    //      the entire subtree, not just the top.
    //   2. setrlimit(RLIMIT_AS, ...) — caps the child's address space
    //      when the manifest sets `memory_limit_mb`.
    //   3. setuid(uid) — drops privileges when the manifest sets
    //      `run_as_user`. Username → uid is resolved BEFORE fork (here
    //      in the parent) because getpwnam(3) is not async-signal-safe.
    #[cfg(unix)]
    {
        let closure = unix_pre_exec_closure(spec).map_err(|m| format!("pre_exec setup: {}", m))?;
        use std::os::unix::process::CommandExt;
        // SAFETY: every libc call in the closure is documented as
        // async-signal-safe (setpgid, setrlimit, setuid). No allocator,
        // no locale, no Rust-level locks. The closure owns captured
        // uid/limit copies; nothing is borrowed across the fork.
        unsafe {
            cmd.as_std_mut().pre_exec(closure);
        }
    }

    Ok(cmd)
}

/// Run one child to completion (or timeout) and hand back everything it
/// produced, uninterpreted. Never panics: a child that could not start comes
/// back as a `Capture` with `spawn_error` set rather than an `Err`, so every
/// caller has exactly one shape to handle.
pub async fn execute_capture(spec: &ExecSpec<'_>) -> Capture {
    let start = Instant::now();

    let mut cmd = match configure_command(spec, false) {
        Ok(c) => c,
        Err(msg) => return Capture::not_started(0, msg),
    };

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Capture::not_started(
                start.elapsed().as_millis() as i64,
                format!("spawn failed: {}", e),
            );
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    #[cfg_attr(not(unix), allow(unused_variables))]
    let pid = child.id();

    // Drain stdout/stderr concurrently with `wait()`. Reading only *after*
    // the child exits deadlocks once the child writes more than the OS pipe
    // buffer (~64 KiB): it blocks on write, never exits, and we'd always hit
    // the timeout instead of parsing its output. The drain tasks complete on
    // EOF — when the child exits normally or we kill it on timeout below.
    let out_task = tokio::spawn(async move {
        match stdout {
            Some(s) => read_to_string_capped(s, STDOUT_CAP).await,
            None => String::new(),
        }
    });
    let err_task = tokio::spawn(async move {
        match stderr {
            Some(s) => read_to_string_capped(s, STDERR_CAP).await,
            None => String::new(),
        }
    });

    let wait_result = timeout(spec.timeout, child.wait()).await;
    let dur_ms = start.elapsed().as_millis() as i64;

    let (exit_status, timed_out) = match wait_result {
        Ok(Ok(s)) => (Some(s), false),
        Ok(Err(e)) => {
            warn!("'{}' wait failed: {}", spec.name, e);
            // Reap so the drain tasks see EOF and don't linger detached.
            let _ = child.kill().await;
            return Capture::not_started(dur_ms, format!("wait error: {}", e));
        }
        Err(_) => {
            // SIGKILL the child (and group on Unix) so it can't hang
            // around eating CPU after we've moved on.
            let _ = child.kill().await;
            #[cfg(unix)]
            if let Some(pid) = pid {
                // SAFETY: kill(-pid, SIGKILL) signals the negative pid's
                // entire group; we created a new group with process_group(0)
                // above so blast radius is bounded to the child + its kids.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            (None, true)
        }
    };

    // Child is gone (exited or killed) → pipes hit EOF → drains complete.
    Capture {
        duration_ms: dur_ms,
        exit_code: exit_status.as_ref().and_then(|s| s.code()),
        timed_out,
        stdout: out_task.await.unwrap_or_default(),
        stderr: err_task.await.unwrap_or_default(),
        spawn_error: None,
    }
}

/// Run one probe to completion (or timeout) and interpret its stdout as the
/// probe contract. Failure modes land as `ProbeRun { parse_ok: false,
/// exit_code: ... }` plus an empty metric vector.
pub async fn execute(probe: &Manifest) -> (ProbeRun, Vec<ProbeMetric>) {
    let now_ts = chrono::Utc::now().timestamp();
    let cap = execute_capture(&probe.exec_spec()).await;

    if let Some(msg) = &cap.spawn_error {
        return (
            synth_run(probe, now_ts, cap.duration_ms, None, Some(msg), false),
            Vec::new(),
        );
    }

    if cap.timed_out {
        return (
            synth_run(
                probe,
                now_ts,
                cap.duration_ms,
                None,
                Some(&format!(
                    "timed out after {}ms; stderr tail: {}",
                    probe.timeout.as_millis(),
                    truncate(&cap.stderr, 200)
                )),
                false,
            ),
            Vec::new(),
        );
    }

    match parse_last_json_line(&cap.stdout) {
        Some(out) => {
            let ts = out.timestamp.filter(|t| *t > 0).unwrap_or(now_ts);
            let run = ProbeRun {
                probe_name: probe.name.clone(),
                timestamp: ts,
                duration_ms: cap.duration_ms,
                exit_code: cap.exit_code,
                message: out.message,
                parse_ok: true,
            };
            (run, out.metrics)
        }
        None => {
            // No JSON object on the last stdout line. Surface the failure
            // so operators can spot a misbehaving probe from `parse_ok=0`
            // or the message text alone.
            let msg = if cap.exit_code.unwrap_or(0) == 0 {
                "exit 0 but no parseable JSON object on the last line".to_string()
            } else {
                format!(
                    "exit {} with no parseable JSON; stderr tail: {}",
                    cap.exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".into()),
                    truncate(&cap.stderr, 200)
                )
            };
            (
                synth_run(
                    probe,
                    now_ts,
                    cap.duration_ms,
                    cap.exit_code,
                    Some(&msg),
                    false,
                ),
                Vec::new(),
            )
        }
    }
}

fn synth_run(
    probe: &Manifest,
    now_ts: i64,
    dur_ms: i64,
    exit_code: Option<i32>,
    message: Option<&str>,
    parse_ok: bool,
) -> ProbeRun {
    ProbeRun {
        probe_name: probe.name.clone(),
        timestamp: now_ts,
        duration_ms: dur_ms,
        exit_code,
        message: message.map(|s| s.to_string()),
        parse_ok,
    }
}

/// Take the last stdout line that looks like a JSON object and try to
/// parse it as a `ProbeOutput`. Lines that aren't JSON objects (script
/// logging, banners, etc.) earlier in stdout are silently skipped.
fn parse_last_json_line(stdout: &str) -> Option<ProbeOutput> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .rfind(|l| l.starts_with('{') && l.ends_with('}'))
        .and_then(|line| serde_json::from_str::<ProbeOutput>(line).ok())
}

/// Read up to `cap` bytes from a child stdio handle into a UTF-8 String,
/// silently truncating the rest. Probes shouldn't print megabytes — but
/// when they do, we don't want to OOM.
async fn read_to_string_capped<R>(mut reader: R, cap: usize) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(cap.min(4096));
    let mut tmp = [0u8; 4096];
    loop {
        match reader.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                // Keep at most `cap` bytes but never stop reading early. A
                // child that fills its stdout pipe (~64 KiB on Linux) blocks
                // on write; if we stopped draining here it would deadlock
                // against `child.wait()` and always trip the timeout. Drain
                // the overflow and discard it.
                if buf.len() < cap {
                    let take = (cap - buf.len()).min(n);
                    buf.extend_from_slice(&tmp[..take]);
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Slice on a char boundary, never through a multi-byte UTF-8
        // sequence. Probe stdout/stderr is operator- and target-controlled
        // (e.g. a cert subject or log line with non-ASCII bytes); with
        // `panic = "abort"` a mid-codepoint `&s[..max]` would take down the
        // whole server, not just the probe task.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Drive a stream-mode probe: keep the child alive, parse each
/// newline-delimited JSON line, push `(ProbeRun, Vec<ProbeMetric>)`
/// per line onto `tx`. Returns when the child exits, the receiver
/// drops, or the per-line idle deadline (`probe.timeout`) elapses.
///
/// One emitted ProbeRun per line. `duration_ms` on a stream-mode run is
/// `0` because there's no fresh-spawn boundary — duration would be
/// "time since last line" which we don't track today (and rarely
/// matters when the alert engine fires off the value column).
pub async fn execute_stream(probe: &Manifest, tx: mpsc::Sender<(ProbeRun, Vec<ProbeMetric>)>) {
    let now_ts = chrono::Utc::now().timestamp();
    let probe_name = probe.name.clone();

    let mut cmd = match configure_command(&probe.exec_spec(), true) {
        Ok(c) => c,
        Err(msg) => {
            let _ = tx
                .send((
                    synth_run(probe, now_ts, 0, None, Some(&msg), false),
                    Vec::new(),
                ))
                .await;
            return;
        }
    };

    let mut child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx
                .send((
                    synth_run(
                        probe,
                        now_ts,
                        0,
                        None,
                        Some(&format!("spawn failed: {}", e)),
                        false,
                    ),
                    Vec::new(),
                ))
                .await;
            return;
        }
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            // Should never happen — we set Stdio::piped above.
            let _ = tx
                .send((
                    synth_run(probe, now_ts, 0, None, Some("stdout pipe missing"), false),
                    Vec::new(),
                ))
                .await;
            let _ = child.kill().await;
            return;
        }
    };

    let mut lines = BufReader::new(stdout).lines();
    let idle_deadline = probe.timeout;

    loop {
        match timeout(idle_deadline, lines.next_line()).await {
            // Got a line within the idle window.
            Ok(Ok(Some(line))) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let line_ts = chrono::Utc::now().timestamp();
                if !(trimmed.starts_with('{') && trimmed.ends_with('}')) {
                    // Not a JSON object — could be log noise the script
                    // also writes. Silently skip; only surface a
                    // parse_ok=false run when we see something that
                    // LOOKS like JSON but doesn't deserialize.
                    continue;
                }
                match serde_json::from_str::<ProbeOutput>(trimmed) {
                    Ok(out) => {
                        let ts = out.timestamp.filter(|t| *t > 0).unwrap_or(line_ts);
                        let run = ProbeRun {
                            probe_name: probe_name.clone(),
                            timestamp: ts,
                            duration_ms: 0,
                            exit_code: None,
                            message: out.message,
                            parse_ok: true,
                        };
                        if tx.send((run, out.metrics)).await.is_err() {
                            // Receiver gone — kill child and bail.
                            let _ = child.kill().await;
                            return;
                        }
                    }
                    Err(e) => {
                        let run = synth_run(
                            probe,
                            line_ts,
                            0,
                            None,
                            Some(&format!(
                                "JSON parse error on line: {} (err: {})",
                                truncate(trimmed, 200),
                                e
                            )),
                            false,
                        );
                        if tx.send((run, Vec::new())).await.is_err() {
                            let _ = child.kill().await;
                            return;
                        }
                    }
                }
            }
            // Child closed stdout normally — wait for exit and report
            // the final state, then return.
            Ok(Ok(None)) => {
                let exit = child.wait().await.ok();
                let run = ProbeRun {
                    probe_name: probe_name.clone(),
                    timestamp: chrono::Utc::now().timestamp(),
                    duration_ms: 0,
                    exit_code: exit.and_then(|s| s.code()),
                    message: Some("stdout closed; child exited".into()),
                    parse_ok: false,
                };
                let _ = tx.send((run, Vec::new())).await;
                return;
            }
            // Read I/O error.
            Ok(Err(e)) => {
                warn!("probe '{}' stream read error: {}", probe_name, e);
                let _ = child.kill().await;
                let run = synth_run(
                    probe,
                    chrono::Utc::now().timestamp(),
                    0,
                    None,
                    Some(&format!("stream read error: {}", e)),
                    false,
                );
                let _ = tx.send((run, Vec::new())).await;
                return;
            }
            // Idle timeout — script hasn't emitted anything for the
            // configured window. Likely stuck. Kill it and let the
            // schedule restart it on the next fire.
            Err(_) => {
                warn!(
                    "probe '{}' idle timeout ({}ms with no output); killing",
                    probe_name,
                    idle_deadline.as_millis()
                );
                let _ = child.kill().await;
                let run = synth_run(
                    probe,
                    chrono::Utc::now().timestamp(),
                    0,
                    None,
                    Some(&format!(
                        "stream idle for {}ms; killed and will restart on schedule",
                        idle_deadline.as_millis()
                    )),
                    false,
                );
                let _ = tx.send((run, Vec::new())).await;
                return;
            }
        }
    }
}

/// Build the pre_exec closure that runs in the child between fork() and
/// exec(). Username → uid resolution happens HERE (in the parent) — the
/// closure body must not call getpwnam(3) or anything else
/// non-async-signal-safe. Returns `Err(msg)` when the manifest references
/// a user that isn't on this host; the runner surfaces that as a
/// `parse_ok=false` run-meta with a helpful message.
#[cfg(unix)]
fn unix_pre_exec_closure(
    spec: &ExecSpec<'_>,
) -> Result<impl FnMut() -> std::io::Result<()> + Send + Sync + 'static, String> {
    // Resolve the target uid + primary gid before fork — getpwnam allocates
    // and is documented as not async-signal-safe.
    let creds: Option<(libc::uid_t, libc::gid_t)> = match spec.run_as_user {
        Some(name) => {
            let cname = std::ffi::CString::new(name)
                .map_err(|e| format!("run_as_user '{}' has nul byte: {}", name, e))?;
            // SAFETY: getpwnam returns a pointer into thread-local static
            // storage; we read pw_uid/pw_gid before any other call could
            // overwrite it. Null result = user not in /etc/passwd.
            let pw = unsafe { libc::getpwnam(cname.as_ptr()) };
            if pw.is_null() {
                return Err(format!("run_as_user '{}' not found in /etc/passwd", name));
            }
            Some(unsafe { ((*pw).pw_uid, (*pw).pw_gid) })
        }
        None => None,
    };

    // Only drop gid + supplementary groups when we're root and actually
    // switching uid. A non-root server can only setuid to its own ruid and
    // lacks CAP_SETGID, so setgroups/setgid would fail with EPERM — keep the
    // prior setuid-only behavior there so non-root setups don't regress.
    // SAFETY: geteuid is async-signal-safe and infallible; we call it in the
    // parent so the post-fork closure stays minimal.
    let is_root = unsafe { libc::geteuid() } == 0;

    let limit_bytes: Option<libc::rlim_t> = spec
        .memory_limit_mb
        .map(|mb| (mb as libc::rlim_t).saturating_mul(1024 * 1024));

    Ok(move || {
        // Step 1: new process group so kill-on-timeout reaches the full
        // child subtree (any descendants the script spawned).
        // SAFETY: setpgid is async-signal-safe; the (0,0) form moves us
        // into a new group with our own pid as group leader.
        let r = unsafe { libc::setpgid(0, 0) };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // Step 2: address-space cap before exec, so the script's first
        // mmap counts against the limit.
        if let Some(bytes) = limit_bytes {
            let rl = libc::rlimit {
                rlim_cur: bytes,
                rlim_max: bytes,
            };
            // SAFETY: rl is fully initialised on the stack;
            // setrlimit(RLIMIT_AS, ...) is async-signal-safe.
            let r = unsafe { libc::setrlimit(libc::RLIMIT_AS, &rl) };
            if r != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }

        // Step 3: drop privileges last, in the correct order — supplementary
        // groups and gid BEFORE uid, while still privileged. setuid(2) alone
        // left the child with the server's gid 0 and *all* of root's
        // supplementary groups (docker, sudo, …), so a probe dropped to an
        // unprivileged user could still reach group-gated resources.
        if let Some((uid, gid)) = creds {
            if is_root {
                // Clear the server's supplementary group set, then take the
                // target user's primary gid. Both need CAP_SETGID — hence the
                // is_root gate above.
                // SAFETY: setgroups is a thin syscall with no allocation;
                // (0, NULL) clears the supplementary set. Must precede setuid,
                // which drops the capability.
                let r = unsafe { libc::setgroups(0, std::ptr::null::<libc::gid_t>()) };
                if r != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // SAFETY: setgid is async-signal-safe.
                let r = unsafe { libc::setgid(gid) };
                if r != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            // SAFETY: setuid is async-signal-safe. On Linux this drops both
            // effective and real uid; a non-root caller may only switch to
            // its own ruid (fails loud on misconfig instead of silently
            // leaving the script as root).
            let r = unsafe { libc::setuid(uid) };
            if r != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }

        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_line() {
        let s = r#"{"message":"hi","metrics":[{"name":"x","value":1}]}"#;
        let out = parse_last_json_line(s).expect("must parse");
        assert_eq!(out.message.as_deref(), Some("hi"));
        assert_eq!(out.metrics.len(), 1);
        assert_eq!(out.metrics[0].name, "x");
    }

    #[test]
    fn skips_log_lines_before_json() {
        let s = "starting...\nworking\n{\"metrics\":[{\"name\":\"a\",\"value\":2}]}\n";
        let out = parse_last_json_line(s).expect("must parse the last line");
        assert_eq!(out.metrics[0].value, 2.0);
    }

    #[test]
    fn last_line_wins() {
        let s = "{\"metrics\":[{\"name\":\"a\",\"value\":1}]}\n{\"metrics\":[{\"name\":\"a\",\"value\":2}]}\n";
        let out = parse_last_json_line(s).expect("must parse last");
        assert_eq!(out.metrics[0].value, 2.0);
    }

    #[test]
    fn rejects_non_json_garbage() {
        assert!(parse_last_json_line("not json at all").is_none());
        assert!(parse_last_json_line("").is_none());
    }

    #[test]
    fn empty_metrics_array_is_valid() {
        let out = parse_last_json_line(r#"{"metrics":[]}"#).expect("parse");
        assert!(out.metrics.is_empty());
    }

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hi there", 200), "hi there");
    }

    #[test]
    fn truncate_never_panics_on_multibyte_boundary() {
        // Each 'é' is two UTF-8 bytes, so every odd `max` lands mid-codepoint.
        // The byte-slice version would panic here (and, with panic=abort,
        // crash the server). We must back off to a char boundary instead.
        let s = "éééééééééé hello world";
        for max in 1..s.len() {
            let out = truncate(s, max); // must not panic for any cut point
            if s.len() > max {
                assert!(out.ends_with('…'));
            }
        }
    }

    #[test]
    fn truncate_backs_off_to_char_boundary() {
        // "é€" = 2 + 3 = 5 bytes. Cutting at byte 3 (mid-'€') must back off
        // to byte 2, keeping just "é".
        assert_eq!(truncate("é€", 3), "é…");
    }

    #[tokio::test]
    async fn read_capped_keeps_at_most_cap_and_drains_rest() {
        // Input far larger than the cap. We keep exactly `cap` bytes but must
        // still consume the whole reader — the production path must never stop
        // early or a chatty child's full stdout pipe would deadlock wait().
        let data = vec![b'x'; 200_000];
        let out = read_to_string_capped(&data[..], 64 * 1024).await;
        assert_eq!(out.len(), 64 * 1024);
    }
}
