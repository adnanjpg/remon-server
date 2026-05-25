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

use std::process::Stdio;
use std::time::Instant;

use log::warn;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::models::probe::{ProbeMetric, ProbeOutput, ProbeRun};

use super::manifest::Manifest;

/// Configure a `tokio::process::Command` for a probe — common to
/// oneshot and stream paths. Sets piped stdio, env, kill-on-drop, and
/// (on Unix) installs the pre_exec hook for setpgid/setrlimit/setuid.
fn configure_command(probe: &Manifest, kill_on_drop: bool) -> Result<Command, String> {
    let exe = probe
        .command
        .first()
        .ok_or_else(|| "command argv is empty".to_string())?;
    let args = &probe.command[1..];

    let mut cmd = Command::new(exe);
    cmd.args(args);
    cmd.envs(&probe.env);
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
        let closure = unix_pre_exec_closure(probe).map_err(|m| format!("pre_exec setup: {}", m))?;
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

/// Run one probe to completion (or timeout). Never panics; failure
/// modes land as `ProbeRun { parse_ok: false, exit_code: ... }` plus
/// an empty metric vector.
pub async fn execute(probe: &Manifest) -> (ProbeRun, Vec<ProbeMetric>) {
    let now_ts = chrono::Utc::now().timestamp();
    let start = Instant::now();

    let mut cmd = match configure_command(probe, false) {
        Ok(c) => c,
        Err(msg) => {
            return (
                synth_run(probe, now_ts, 0, None, Some(&msg), false),
                Vec::new(),
            );
        }
    };

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return (
                synth_run(
                    probe,
                    now_ts,
                    start.elapsed().as_millis() as i64,
                    None,
                    Some(&format!("spawn failed: {}", e)),
                    false,
                ),
                Vec::new(),
            );
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let _pid = child.id();

    let wait_result = timeout(probe.timeout, child.wait()).await;
    let dur_ms = start.elapsed().as_millis() as i64;

    let (exit_status, timed_out) = match wait_result {
        Ok(Ok(s)) => (Some(s), false),
        Ok(Err(e)) => {
            warn!("probe '{}' wait failed: {}", probe.name, e);
            return (
                synth_run(
                    probe,
                    now_ts,
                    dur_ms,
                    None,
                    Some(&format!("wait error: {}", e)),
                    false,
                ),
                Vec::new(),
            );
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

    let stdout_text = match stdout {
        Some(s) => read_to_string_capped(s, 64 * 1024).await,
        None => String::new(),
    };
    let stderr_text = match stderr {
        Some(s) => read_to_string_capped(s, 8 * 1024).await,
        None => String::new(),
    };

    if timed_out {
        return (
            synth_run(
                probe,
                now_ts,
                dur_ms,
                None,
                Some(&format!(
                    "timed out after {}ms; stderr tail: {}",
                    probe.timeout.as_millis(),
                    truncate(&stderr_text, 200)
                )),
                false,
            ),
            Vec::new(),
        );
    }

    let exit_code = exit_status.as_ref().and_then(|s| s.code());
    let parsed = parse_last_json_line(&stdout_text);

    match parsed {
        Some(out) => {
            let ts = out.timestamp.filter(|t| *t > 0).unwrap_or(now_ts);
            let run = ProbeRun {
                probe_name: probe.name.clone(),
                timestamp: ts,
                duration_ms: dur_ms,
                exit_code,
                message: out.message,
                parse_ok: true,
            };
            (run, out.metrics)
        }
        None => {
            // No JSON object on the last stdout line. Surface the failure
            // so operators can spot a misbehaving probe from `parse_ok=0`
            // or the message text alone.
            let msg = if exit_code.unwrap_or(0) == 0 {
                "exit 0 but no parseable JSON object on the last line".to_string()
            } else {
                format!(
                    "exit {} with no parseable JSON; stderr tail: {}",
                    exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".into()),
                    truncate(&stderr_text, 200)
                )
            };
            (
                synth_run(probe, now_ts, dur_ms, exit_code, Some(&msg), false),
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
        .filter(|l| l.starts_with('{') && l.ends_with('}'))
        .last()
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
        if buf.len() >= cap {
            break;
        }
        let to_read = (cap - buf.len()).min(tmp.len());
        match reader.read(&mut tmp[..to_read]).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
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

    let mut cmd = match configure_command(probe, true) {
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
    probe: &Manifest,
) -> Result<impl FnMut() -> std::io::Result<()> + Send + Sync + 'static, String> {
    // Resolve the target uid before fork — getpwnam allocates and is
    // documented as not async-signal-safe.
    let uid: Option<libc::uid_t> = match probe.run_as_user.as_deref() {
        Some(name) => {
            let cname = std::ffi::CString::new(name)
                .map_err(|e| format!("run_as_user '{}' has nul byte: {}", name, e))?;
            // SAFETY: getpwnam returns a pointer into thread-local static
            // storage; we read pw_uid before any other call could
            // overwrite it. Null result = user not in /etc/passwd.
            let pw = unsafe { libc::getpwnam(cname.as_ptr()) };
            if pw.is_null() {
                return Err(format!("run_as_user '{}' not found in /etc/passwd", name));
            }
            Some(unsafe { (*pw).pw_uid })
        }
        None => None,
    };

    let limit_bytes: Option<libc::rlim_t> = probe
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

        // Step 3: drop privileges last so the prior steps run with
        // whatever rights the server inherited. setuid(2) on Linux drops
        // both effective and real uid; non-root callers may only switch
        // to their own ruid (this fails loud on misconfig instead of
        // silently leaving the script as root).
        if let Some(uid) = uid {
            // SAFETY: setuid is async-signal-safe.
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
}
