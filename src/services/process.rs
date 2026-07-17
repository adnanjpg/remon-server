use sysinfo::System;

use crate::models::process::{ProcessInfo, ProcessList, ProcessState};

/// Get list of all processes
pub fn get_processes(sys: &System) -> ProcessList {
    let timestamp = chrono::Utc::now().timestamp();
    let total_memory = sys.total_memory();

    let processes: Vec<ProcessInfo> = sys
        .processes()
        .iter()
        // sysinfo lists threads (tasks) as processes on Linux: a many-threaded
        // program (Java, node, ...) shows one row per thread, each reporting
        // the whole process's shared RSS — which inflates the list and breaks
        // the memory ranking. Keep only real processes (thread group leaders);
        // `thread_kind()` is `Some(_)` for a thread, `None` for a process.
        .filter(|(_, process)| process.thread_kind().is_none())
        .map(|(pid, process)| {
            let memory_bytes = process.memory();
            let memory_percent = if total_memory > 0 {
                (memory_bytes as f64 / total_memory as f64) * 100.0
            } else {
                0.0
            };

            ProcessInfo {
                pid: pid.as_u32(),
                parent_pid: process.parent().map(|p| p.as_u32()),
                name: process.name().to_string_lossy().to_string(),
                cmd: process
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy().to_string())
                    .collect(),
                exe: process.exe().map(|p| p.to_string_lossy().to_string()),
                cwd: process.cwd().map(|p| p.to_string_lossy().to_string()),
                user: process.user_id().map(|u| u.to_string()),
                cpu_percent: process.cpu_usage() as f64,
                memory_bytes,
                memory_percent,
                state: process_state(process.status()),
                started_at: Some(process.start_time() as i64),
                // Now that threads are filtered out of the list, report how
                // many each real process has: its own task set plus itself.
                threads: process.tasks().map(|t| t.len() as u32 + 1),
            }
        })
        .collect();

    let total_count = processes.len();

    ProcessList {
        processes,
        total_count,
        timestamp,
    }
}

/// Kill a process by PID, cross-platform.
///
/// Hits the OS directly instead of going through sysinfo — the prior path
/// did `System::new_all() + refresh_all()` (10–50 ms inventorying every
/// process on the box) just to obtain a `Process` handle whose `kill_with`
/// translates to the same syscall we issue here.
///
/// Platform behavior:
/// - **Unix** (Linux/macOS): `kill(2)`. The `signal` argument is honored;
///   1=SIGHUP, 2=SIGINT, 9=SIGKILL, 15=SIGTERM, anything else = SIGTERM.
/// - **Windows**: no POSIX signals exist; we open the target with
///   `PROCESS_TERMINATE` and call `TerminateProcess`, the closest
///   equivalent to SIGKILL. The `signal` argument is ignored. Same
///   semantics sysinfo had on this platform.
///
/// `pid == 0` is rejected explicitly — on Unix that targets the caller's
/// entire process group; on Windows that's the System Idle Process,
/// which can't be terminated. Either way it isn't something we want a
/// request handler to do.
pub fn kill_process(pid: u32, signal: i32) -> Result<(), String> {
    if pid == 0 {
        return Err("refusing to signal pid 0".to_string());
    }
    kill_process_native(pid, signal)
}

#[cfg(unix)]
fn kill_process_native(pid: u32, signal: i32) -> Result<(), String> {
    let sig = match signal {
        1 => libc::SIGHUP,
        2 => libc::SIGINT,
        9 => libc::SIGKILL,
        15 => libc::SIGTERM,
        _ => libc::SIGTERM,
    };
    // SAFETY: `pid` is non-zero (checked by caller) and `sig` is one of
    // the values above. `kill(2)` is well-defined for those.
    let r = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if r == 0 {
        Ok(())
    } else {
        Err(format!(
            "kill({}, {}) failed: {}",
            pid,
            signal,
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(windows)]
fn kill_process_native(pid: u32, _signal: i32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    // SAFETY: `OpenProcess` returns a null handle on failure, which we
    // check before any further use. `TerminateProcess`/`CloseHandle` are
    // called on a handle we just verified is non-null. Exit code 1 is a
    // conventional "killed externally" marker.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return Err(format!(
            "OpenProcess({}) failed: {}",
            pid,
            std::io::Error::last_os_error()
        ));
    }

    let ok = unsafe { TerminateProcess(handle, 1) } != 0;
    let term_err = if !ok {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    unsafe { CloseHandle(handle) };

    match term_err {
        None => Ok(()),
        Some(e) => Err(format!("TerminateProcess({}) failed: {}", pid, e)),
    }
}

fn process_state(status: sysinfo::ProcessStatus) -> ProcessState {
    match status {
        sysinfo::ProcessStatus::Run => ProcessState::Running,
        sysinfo::ProcessStatus::Sleep => ProcessState::Sleeping,
        sysinfo::ProcessStatus::Stop => ProcessState::Stopped,
        sysinfo::ProcessStatus::Zombie => ProcessState::Zombie,
        sysinfo::ProcessStatus::Idle => ProcessState::Idle,
        _ => ProcessState::Unknown,
    }
}
