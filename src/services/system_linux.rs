//! Linux-only metric extras the cross-platform `sysinfo` doesn't surface:
//! - `/proc/stat` extended CPU breakdown (steal, iowait, guest) plus
//!   kernel event counters (context switches, process forks)
//! - `/proc/vmstat` page-fault and swap traffic counters
//! - `/proc/pressure/*` PSI saturation signals
//! - filesystem inode usage via `statvfs`
//!
//! Every public function in here returns `Option<T>`; on a kernel without
//! the relevant proc file (e.g. PSI on pre-4.20) or any read/parse failure
//! we silently return `None` so the caller can carry on with the rest of
//! the stats frame.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::fs;
use std::mem::MaybeUninit;
use std::time::Duration;

use crate::models::stats::PressureStats;

/// Snapshot of `/proc/stat`: aggregate `cpu` jiffy counters plus the
/// kernel-event tallies (context switches, process forks). All fields are
/// monotonic counters — turn them into rates with a prior snapshot.
#[derive(Debug, Clone, Copy)]
pub struct ProcStatSnapshot {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
    pub guest: u64,
    /// Total context switches kernel-wide since boot.
    pub ctxt: u64,
    /// Total processes forked since boot. Despite the line name "processes"
    /// in /proc/stat, this counts forks (clone() syscall invocations).
    pub processes: u64,
}

impl ProcStatSnapshot {
    pub fn total(&self) -> u64 {
        self.user
            + self.nice
            + self.system
            + self.idle
            + self.iowait
            + self.irq
            + self.softirq
            + self.steal
            + self.guest
    }
}

/// Read and parse `/proc/stat`. We need the first line (`cpu …`) for the
/// jiffy breakdown plus two single-line entries (`ctxt N`, `processes N`)
/// further down. Returns None on any I/O or parse failure.
pub fn read_proc_stat() -> Option<ProcStatSnapshot> {
    let content = fs::read_to_string("/proc/stat").ok()?;
    let mut user = 0u64; let mut nice = 0u64; let mut system = 0u64;
    let mut idle = 0u64; let mut iowait = 0u64; let mut irq = 0u64;
    let mut softirq = 0u64; let mut steal = 0u64; let mut guest = 0u64;
    let mut ctxt = 0u64; let mut processes = 0u64;
    let mut have_cpu = false;
    for line in content.lines() {
        if !have_cpu && (line.starts_with("cpu ") || line.starts_with("cpu\t")) {
            let mut tokens = line.split_whitespace();
            tokens.next(); // "cpu"
            let nums: Vec<u64> = tokens.filter_map(|s| s.parse().ok()).collect();
            if nums.len() < 9 {
                return None;
            }
            user = nums[0]; nice = nums[1]; system = nums[2]; idle = nums[3];
            iowait = nums[4]; irq = nums[5]; softirq = nums[6];
            steal = nums[7]; guest = nums[8];
            have_cpu = true;
        } else if let Some(rest) = line.strip_prefix("ctxt ") {
            ctxt = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("processes ") {
            processes = rest.trim().parse().unwrap_or(0);
        }
    }
    if !have_cpu {
        return None;
    }
    Some(ProcStatSnapshot {
        user, nice, system, idle, iowait, irq, softirq, steal, guest,
        ctxt, processes,
    })
}

/// Snapshot of the `/proc/vmstat` lines we care about. Each is a monotonic
/// counter (in pages, not bytes) — rate them with a prior snapshot.
#[derive(Debug, Clone, Copy, Default)]
pub struct VmstatSnapshot {
    /// All page faults (major + minor).
    pub pgfault: u64,
    /// Major faults only — those that required disk I/O to resolve.
    pub pgmajfault: u64,
    /// Pages swapped IN from disk to RAM.
    pub pswpin: u64,
    /// Pages swapped OUT from RAM to disk.
    pub pswpout: u64,
}

pub fn read_vmstat() -> Option<VmstatSnapshot> {
    let content = fs::read_to_string("/proc/vmstat").ok()?;
    let mut s = VmstatSnapshot::default();
    let mut any = false;
    for line in content.lines() {
        let mut tokens = line.split_whitespace();
        let key = match tokens.next() { Some(k) => k, None => continue };
        let val: u64 = match tokens.next().and_then(|v| v.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        match key {
            "pgfault" => { s.pgfault = val; any = true; }
            "pgmajfault" => { s.pgmajfault = val; any = true; }
            "pswpin" => { s.pswpin = val; any = true; }
            "pswpout" => { s.pswpout = val; any = true; }
            _ => {}
        }
    }
    if any { Some(s) } else { None }
}

/// Computed deltas, expressed as percent of total CPU time over the
/// interval. Caller is responsible for interpreting; "high" thresholds:
/// - steal_percent: >2-3% sustained on a VPS = host throttling
/// - iowait_percent: >20% sustained = disk-bound workload
#[derive(Debug, Clone, Copy)]
pub struct CpuExtras {
    pub steal_percent: f64,
    pub iowait_percent: f64,
    pub guest_percent: f64,
}

/// Compute per-tick percentages from two consecutive snapshots. Returns
/// None if no time elapsed between samples (possible right after boot or
/// if the same snapshot was passed twice).
pub fn compute_cpu_extras(prev: ProcStatSnapshot, cur: ProcStatSnapshot) -> Option<CpuExtras> {
    let total_d = cur.total().saturating_sub(prev.total());
    if total_d == 0 {
        return None;
    }
    let pct = |a: u64, b: u64| (a.saturating_sub(b) as f64 / total_d as f64) * 100.0;
    Some(CpuExtras {
        steal_percent: pct(cur.steal, prev.steal),
        iowait_percent: pct(cur.iowait, prev.iowait),
        guest_percent: pct(cur.guest, prev.guest),
    })
}

/// Per-second deltas for the kernel-event counters in /proc/stat.
#[derive(Debug, Clone, Copy)]
pub struct KernelEventRates {
    pub context_switches_per_sec: u64,
    pub process_forks_per_sec: u64,
}

pub fn compute_kernel_event_rates(
    prev: ProcStatSnapshot,
    cur: ProcStatSnapshot,
    interval_secs: f64,
) -> Option<KernelEventRates> {
    if interval_secs <= 0.0 {
        return None;
    }
    let rate = |a: u64, b: u64| (a.saturating_sub(b) as f64 / interval_secs) as u64;
    Some(KernelEventRates {
        context_switches_per_sec: rate(cur.ctxt, prev.ctxt),
        process_forks_per_sec: rate(cur.processes, prev.processes),
    })
}

/// Per-second deltas for the /proc/vmstat counters we capture.
#[derive(Debug, Clone, Copy)]
pub struct VmstatRates {
    pub page_faults_minor_per_sec: u64,
    pub page_faults_major_per_sec: u64,
    pub swap_in_pages_per_sec: u64,
    pub swap_out_pages_per_sec: u64,
}

pub fn compute_vmstat_rates(
    prev: VmstatSnapshot,
    cur: VmstatSnapshot,
    interval_secs: f64,
) -> Option<VmstatRates> {
    if interval_secs <= 0.0 {
        return None;
    }
    let pgfault_d = cur.pgfault.saturating_sub(prev.pgfault);
    let pgmaj_d = cur.pgmajfault.saturating_sub(prev.pgmajfault);
    // Minor faults: total minus major. Floor at 0 for the rare race
    // window where major increments observed before total.
    let pgmin_d = pgfault_d.saturating_sub(pgmaj_d);
    let rate = |d: u64| (d as f64 / interval_secs) as u64;
    Some(VmstatRates {
        page_faults_minor_per_sec: rate(pgmin_d),
        page_faults_major_per_sec: rate(pgmaj_d),
        swap_in_pages_per_sec: rate(cur.pswpin.saturating_sub(prev.pswpin)),
        swap_out_pages_per_sec: rate(cur.pswpout.saturating_sub(prev.pswpout)),
    })
}

/// Read `/proc/pressure/{cpu,memory,io}`. Each file looks like:
///
/// ```text
/// some avg10=0.00 avg60=0.00 avg300=0.00 total=...
/// full avg10=0.00 avg60=0.00 avg300=0.00 total=...
/// ```
///
/// `cpu` only emits a `some` line; the `full` averages stay 0.0.
/// Returns None on pre-4.20 kernels (file does not exist) or read failure.
pub fn read_pressure(resource: &str) -> Option<PressureStats> {
    let path = format!("/proc/pressure/{}", resource);
    let content = fs::read_to_string(&path).ok()?;
    let mut some = (0.0_f64, 0.0_f64, 0.0_f64);
    let mut full = (0.0_f64, 0.0_f64, 0.0_f64);
    for line in content.lines() {
        let kind = match line.split_whitespace().next() {
            Some(k) => k,
            None => continue,
        };
        let parsed = parse_pressure_line(line);
        match kind {
            "some" => some = parsed,
            "full" => full = parsed,
            _ => {}
        }
    }
    Some(PressureStats {
        some_avg10: some.0,
        some_avg60: some.1,
        some_avg300: some.2,
        full_avg10: full.0,
        full_avg60: full.1,
        full_avg300: full.2,
    })
}

fn parse_pressure_line(line: &str) -> (f64, f64, f64) {
    let mut a10 = 0.0;
    let mut a60 = 0.0;
    let mut a300 = 0.0;
    for tok in line.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=') {
            let val: f64 = v.parse().unwrap_or(0.0);
            match k {
                "avg10" => a10 = val,
                "avg60" => a60 = val,
                "avg300" => a300 = val,
                _ => {}
            }
        }
    }
    (a10, a60, a300)
}

/// Inode utilization for a mount point, expressed as 0.0..=100.0.
/// Backed by `statvfs(2)`. Returns None if the syscall fails, the
/// filesystem reports `f_files = 0` (some pseudo-filesystems do), or the
/// call doesn't finish within `STATVFS_TIMEOUT`.
///
/// `statvfs` on a hung network/fuse mount blocks the calling thread
/// indefinitely. Running it inside `spawn_blocking` + `timeout` keeps the
/// collector loop healthy at the cost of a per-mount tokio task per tick.
pub async fn read_inode_usage(mount_point: &str) -> Option<f64> {
    const STATVFS_TIMEOUT: Duration = Duration::from_secs(2);

    let mp = mount_point.to_string();
    let blocking = tokio::task::spawn_blocking(move || statvfs_inode_usage(&mp));
    match tokio::time::timeout(STATVFS_TIMEOUT, blocking).await {
        Ok(Ok(v)) => v,
        // Either the inner spawn_blocking panicked / was cancelled, or
        // statvfs didn't return within the budget. Both surface as "no
        // data for this mount this tick" rather than a stall.
        _ => None,
    }
}

fn statvfs_inode_usage(mount_point: &str) -> Option<f64> {
    let path = CString::new(mount_point).ok()?;
    let mut buf: MaybeUninit<libc::statvfs> = MaybeUninit::uninit();
    // SAFETY: `path` is a valid NUL-terminated C string; `buf` is
    // exclusive and will be initialized by the syscall on success.
    let r = unsafe { libc::statvfs(path.as_ptr(), buf.as_mut_ptr()) };
    if r != 0 {
        return None;
    }
    let st = unsafe { buf.assume_init() };
    if st.f_files == 0 {
        return None;
    }
    let used = st.f_files.saturating_sub(st.f_ffree);
    Some((used as f64 / st.f_files as f64) * 100.0)
}
