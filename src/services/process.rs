use sysinfo::{System, Pid, Signal};
use log::error;

use crate::models::process::{ProcessInfo, ProcessList, ProcessState};

/// Get list of all processes
pub fn get_processes(sys: &System) -> ProcessList {
    let timestamp = chrono::Utc::now().timestamp();
    let total_memory = sys.total_memory();

    let processes: Vec<ProcessInfo> = sys
        .processes()
        .iter()
        .map(|(pid, process)| {
            let memory_bytes = process.memory();
            let memory_percent = if total_memory > 0 {
                (memory_bytes as f64 / total_memory as f64) * 100.0
            } else {
                0.0
            };

            ProcessInfo {
                pid: pid.as_u32(),
                name: process.name().to_string_lossy().to_string(),
                cmd: process.cmd().iter().map(|s| s.to_string_lossy().to_string()).collect(),
                exe: process.exe().map(|p| p.to_string_lossy().to_string()),
                user: process.user_id().map(|u| u.to_string()),
                cpu_percent: process.cpu_usage() as f64,
                memory_bytes,
                memory_percent,
                state: process_state(process.status()),
                started_at: Some(process.start_time() as i64),
                threads: None, // sysinfo doesn't expose this directly
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

/// Get a single process by PID
pub fn get_process(sys: &System, pid: u32) -> Option<ProcessInfo> {
    let total_memory = sys.total_memory();

    sys.process(Pid::from_u32(pid)).map(|process| {
        let memory_bytes = process.memory();
        let memory_percent = if total_memory > 0 {
            (memory_bytes as f64 / total_memory as f64) * 100.0
        } else {
            0.0
        };

        ProcessInfo {
            pid,
            name: process.name().to_string_lossy().to_string(),
            cmd: process.cmd().iter().map(|s| s.to_string_lossy().to_string()).collect(),
            exe: process.exe().map(|p| p.to_string_lossy().to_string()),
            user: process.user_id().map(|u| u.to_string()),
            cpu_percent: process.cpu_usage() as f64,
            memory_bytes,
            memory_percent,
            state: process_state(process.status()),
            started_at: Some(process.start_time() as i64),
            threads: None,
        }
    })
}

/// Kill a process
pub fn kill_process(sys: &System, pid: u32, signal: i32) -> Result<(), String> {
    let process = sys
        .process(Pid::from_u32(pid))
        .ok_or_else(|| format!("Process {} not found", pid))?;

    let sig = match signal {
        1 => Signal::Hangup,
        2 => Signal::Interrupt,
        9 => Signal::Kill,
        15 => Signal::Term,
        _ => Signal::Term,
    };

    if process.kill_with(sig).is_none() {
        error!("Failed to send signal {} to process {}", signal, pid);
        return Err(format!(
            "Failed to send signal {} to process {}",
            signal, pid
        ));
    }

    Ok(())
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
