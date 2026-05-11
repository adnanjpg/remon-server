use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use log::{debug, warn};
use sysinfo::{Components, Disks, MINIMUM_CPU_UPDATE_INTERVAL, Networks, System};

use crate::models::stats::{PressureSnapshot, StatsEvent};
use crate::services::system as system_svc;
use crate::services::tick_timer::TickStats;
use crate::state::AppState;
use crate::storage::repositories::MetricsRepository;

#[cfg(target_os = "linux")]
use crate::models::stats::{CpuStats, DiskStats, MemoryStats};
#[cfg(target_os = "linux")]
use crate::services::system_linux::{self, ProcStatSnapshot, VmstatSnapshot};

pub async fn run(state: Arc<AppState>) {
    let mut sys = System::new_all();
    let mut disks = Disks::new_with_refreshed_list();
    let mut networks = Networks::new_with_refreshed_list();
    let mut components = Components::new_with_refreshed_list();
    let metrics_repo = MetricsRepository::new(state.db.clone());

    // Track wall-clock between refreshes so disk/network rates can be
    // expressed per second instead of per-tick.
    //
    // The constructors above (`System::new_all()`, `Disks::new_with_…`,
    // `Networks::new_with_…`) all do an implicit first refresh. We stamp
    // `last_refresh` to *now* and then sleep `MINIMUM_CPU_UPDATE_INTERVAL`
    // (200 ms) before the loop's first refresh: this guarantees both
    //   1. CPU per-core deltas are non-zero on tick 1 (sysinfo requires
    //      ≥200 ms between refreshes for `cpu_usage()` to compute), and
    //   2. tick 1's disk/network rates use a real ~200 ms interval rather
    //      than the previous behavior of falling back to 0 on the first
    //      published frame.
    //
    // Without this warm-up the very first DB row written for cpu/disk/net
    // would carry a misleading 0% / 0 B-per-sec.
    let mut last_refresh: Option<Instant> = Some(Instant::now());
    tokio::time::sleep(MINIMUM_CPU_UPDATE_INTERVAL).await;

    // Linux-only: previous /proc/stat snapshot for steal/iowait/guest +
    // ctxt/forks deltas.
    #[cfg(target_os = "linux")]
    let mut last_proc_stat: Option<ProcStatSnapshot> = None;
    // Linux-only: previous /proc/vmstat snapshot for page-fault / swap rates.
    #[cfg(target_os = "linux")]
    let mut last_vmstat: Option<VmstatSnapshot> = None;

    // Sliding-window phase stats (p50/p95/p99/max). 600-sample window =
    // 20 minutes at the 2s default tick rate; flush every 30 ticks (~1
    // min) so operators see fresh distributions without log spam. The
    // window deliberately spans many flushes — that's how p99 stays
    // meaningful when one cycle's mean is noisy.
    let mut tick_stats = TickStats::new(
        "stats",
        &["refresh", "compute", "broadcast", "db_write"],
        600,
        30,
    );

    // MissedTickBehavior::Skip: drop overrun ticks, don't burst-catch up.
    let mut current_interval_ms = state
        .collector_stats_interval_ms
        .load(Ordering::Relaxed)
        .max(1000);
    let mut ticker = tokio::time::interval(Duration::from_millis(current_interval_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        let now = Instant::now();
        let interval_secs = match last_refresh {
            Some(prev) => (now - prev).as_secs_f64(),
            None => 0.0,
        };
        last_refresh = Some(now);

        let tick_ts = chrono::Utc::now().timestamp();

        // ── Phase: refresh ──────────────────────────────────────────────
        // Surgical refreshes only — process enumeration is the most
        // expensive call sysinfo offers and we don't read it here (the
        // dedicated processes collector owns that). Measured ~41% p50
        // reduction in stats refresh on Windows vs `refresh_all()`.
        let t_refresh = Instant::now();
        sys.refresh_cpu_all();
        sys.refresh_memory();
        disks.refresh(true);
        networks.refresh(true);
        // refresh(true) — let new sensors join the list (e.g. after a USB
        // GPU plug-in mid-run). Cheap on every platform.
        components.refresh(true);
        let refresh_dur = t_refresh.elapsed();

        // ── Phase: compute ──────────────────────────────────────────────
        let t_compute = Instant::now();

        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut cpu_stats = system_svc::get_cpu_stats(&sys, tick_ts);
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut memory_stats = system_svc::get_memory_stats(&sys, tick_ts);
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut disk_stats = system_svc::get_disk_stats(&disks, interval_secs, tick_ts);
        let network_stats = system_svc::get_network_stats(&networks, interval_secs, tick_ts);
        let components_snapshot = system_svc::get_components(&components, tick_ts);

        let pressure_snapshot: Option<PressureSnapshot>;
        #[cfg(target_os = "linux")]
        {
            pressure_snapshot = enrich_linux(
                &mut cpu_stats,
                &mut memory_stats,
                &mut disk_stats,
                interval_secs,
                tick_ts,
                &mut last_proc_stat,
                &mut last_vmstat,
            )
            .await;
        }
        #[cfg(not(target_os = "linux"))]
        {
            pressure_snapshot = None;
        }
        let compute_dur = t_compute.elapsed();

        // ── Phase: broadcast ────────────────────────────────────────────
        // Broadcast first — keep the live stream working even if DB writes
        // hiccup. Receivers having no listeners isn't an error here.
        let t_broadcast = Instant::now();
        let _ = state.stats_tx.send(StatsEvent::Cpu(cpu_stats.clone()));
        let _ = state
            .stats_tx
            .send(StatsEvent::Memory(memory_stats.clone()));
        let _ = state.stats_tx.send(StatsEvent::Disk(disk_stats.clone()));
        let _ = state
            .stats_tx
            .send(StatsEvent::Network(network_stats.clone()));
        if let Some(p) = pressure_snapshot.clone() {
            let _ = state.stats_tx.send(StatsEvent::Pressure(p));
        }
        let _ = state
            .stats_tx
            .send(StatsEvent::Components(components_snapshot.clone()));
        let broadcast_dur = t_broadcast.elapsed();

        // ── Phase: db_write ─────────────────────────────────────────────
        // Persist this tick into the raw bucket. One transaction keeps the
        // frame coherent — no half-written timestamps for the rollup task
        // to find.
        let t_db = Instant::now();
        if let Err(e) = metrics_repo
            .insert_raw_tick(
                &cpu_stats,
                &memory_stats,
                &disk_stats,
                &network_stats,
                pressure_snapshot.as_ref(),
                Some(&components_snapshot),
            )
            .await
        {
            warn!("Failed to persist raw stats tick: {:?}", e);
        }
        let db_write_dur = t_db.elapsed();

        tick_stats.record(&[refresh_dur, compute_dur, broadcast_dur, db_write_dur]);
        tick_stats.flush_if_needed();

        debug!(
            "Stats collected: CPU {:.1}%, Memory {:.1}%",
            cpu_stats.usage_percent,
            (memory_stats.used_bytes as f64 / memory_stats.total_bytes.max(1) as f64) * 100.0
        );

        // Rebuild on interval change to avoid an immediate double-fire.
        let new_interval_ms = state
            .collector_stats_interval_ms
            .load(Ordering::Relaxed)
            .max(1000);
        if new_interval_ms != current_interval_ms {
            current_interval_ms = new_interval_ms;
            let next = tokio::time::Instant::now() + Duration::from_millis(current_interval_ms);
            ticker = tokio::time::interval_at(next, Duration::from_millis(current_interval_ms));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        }
    }
}

/// Patch the cross-platform stats with Linux-only extras: `/proc/stat`
/// extended CPU breakdown + kernel event rates, `/proc/vmstat` page-fault
/// and swap traffic rates, per-mount inode utilization via `statvfs`, and
/// the PSI snapshot. The first tick returns a partial frame (rate fields
/// stay None) because the rate computations need a prior snapshot.
#[cfg(target_os = "linux")]
async fn enrich_linux(
    cpu: &mut CpuStats,
    memory: &mut MemoryStats,
    disks: &mut [DiskStats],
    interval_secs: f64,
    tick_ts: i64,
    last_proc_stat: &mut Option<ProcStatSnapshot>,
    last_vmstat: &mut Option<VmstatSnapshot>,
) -> Option<PressureSnapshot> {
    // /proc/stat: percentages AND kernel-event rates need a prior snapshot.
    let cur_stat = system_linux::read_proc_stat();
    if let (Some(prev), Some(cur)) = (*last_proc_stat, cur_stat) {
        if let Some(extras) = system_linux::compute_cpu_extras(prev, cur) {
            cpu.steal_percent = Some(extras.steal_percent);
            cpu.iowait_percent = Some(extras.iowait_percent);
            cpu.guest_percent = Some(extras.guest_percent);
        }
        if let Some(rates) = system_linux::compute_kernel_event_rates(prev, cur, interval_secs) {
            cpu.context_switches_per_sec = Some(rates.context_switches_per_sec);
            cpu.process_forks_per_sec = Some(rates.process_forks_per_sec);
        }
    }
    *last_proc_stat = cur_stat;

    // /proc/vmstat: page faults and swap traffic — same prior-snapshot dance.
    let cur_vmstat = system_linux::read_vmstat();
    if let (Some(prev), Some(cur)) = (*last_vmstat, cur_vmstat) {
        if let Some(rates) = system_linux::compute_vmstat_rates(prev, cur, interval_secs) {
            memory.page_faults_minor_per_sec = Some(rates.page_faults_minor_per_sec);
            memory.page_faults_major_per_sec = Some(rates.page_faults_major_per_sec);
            memory.swap_in_pages_per_sec = Some(rates.swap_in_pages_per_sec);
            memory.swap_out_pages_per_sec = Some(rates.swap_out_pages_per_sec);
        }
    }
    *last_vmstat = cur_vmstat;

    // Per-mount inode usage via statvfs — wrapped in spawn_blocking +
    // timeout so a hung network/fuse mount can't stall the tick.
    for d in disks.iter_mut() {
        d.inode_used_percent = system_linux::read_inode_usage(&d.mount_point).await;
    }

    // PSI snapshot: per-resource None on pre-4.20 kernels.
    Some(PressureSnapshot {
        cpu: system_linux::read_pressure("cpu"),
        memory: system_linux::read_pressure("memory"),
        io: system_linux::read_pressure("io"),
        timestamp: tick_ts,
    })
}
