use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use log::warn;
use sysinfo::{Components, Disks, MINIMUM_CPU_UPDATE_INTERVAL, Networks, System};

use crate::models::stats::{AllStats, PressureSnapshot, StatsEvent};
use crate::services::system as system_svc;
use crate::services::tick_timer::TickStats;
use crate::state::AppState;
use crate::storage::repositories::MetricsRepository;

#[cfg(target_os = "linux")]
use crate::models::stats::{CpuStats, DiskStats, MemoryStats};
#[cfg(target_os = "linux")]
use crate::services::system_linux::{self, DiskstatEntry, ProcStatSnapshot, VmstatSnapshot};
#[cfg(target_os = "linux")]
use std::collections::HashMap;

pub async fn run(state: Arc<AppState>) {
    let mut sys = System::new();
    sys.refresh_cpu_all();
    sys.refresh_memory();
    let mut disks = Disks::new_with_refreshed_list();
    let mut networks = Networks::new_with_refreshed_list();
    let mut components = Components::new_with_refreshed_list();
    let metrics_repo = MetricsRepository::new(state.db.clone());

    // sysinfo requires ≥200 ms between refreshes for `cpu_usage()` to
    // compute; without this warm-up the first tick reports 0% / 0 B/s.
    let mut last_refresh: Option<Instant> = Some(Instant::now());
    tokio::time::sleep(MINIMUM_CPU_UPDATE_INTERVAL).await;

    // Linux-only: previous /proc/stat snapshot for steal/iowait/guest +
    // ctxt/forks deltas.
    #[cfg(target_os = "linux")]
    let mut last_proc_stat: Option<ProcStatSnapshot> = None;
    // Linux-only: previous /proc/vmstat snapshot for page-fault / swap rates.
    #[cfg(target_os = "linux")]
    let mut last_vmstat: Option<VmstatSnapshot> = None;
    // Linux-only: previous /proc/diskstats snapshot for IOPS + utilization.
    #[cfg(target_os = "linux")]
    let mut last_diskstats: Option<HashMap<String, DiskstatEntry>> = None;

    // Sliding-window phase stats (p50/p95/p99/max). The window deliberately
    // spans many flushes so p99 stays meaningful when one cycle is noisy.
    // refresh_* are split per sysinfo call so the heavy one stands out.
    let mut tick_stats = TickStats::new(
        "stats",
        &[
            "refresh_cpu",
            "refresh_mem",
            "refresh_disks",
            "refresh_net",
            "refresh_comp",
            "compute",
            "broadcast",
            "db_write",
        ],
        600,
        30,
    );

    // `refresh(true)` re-enumerates hot-plug entries; `refresh(false)`
    // only updates counters. Hot-plug is rare relative to the tick rate.
    const DISCOVERY_EVERY_N_TICKS: u32 = 15;
    // Components refresh hits WMI/COM on Windows and is by far the heaviest
    // sysinfo call on that platform. Temperatures don't change fast enough
    // to justify per-tick polling; skip on most ticks.
    const COMPONENTS_REFRESH_EVERY_N_TICKS: u32 = 30;
    let mut tick_count: u32 = 0;

    // MissedTickBehavior::Skip: drop overrun ticks, don't burst-catch up.
    let mut current_interval_ms = state
        .collector_stats_interval_ms
        .load(Ordering::Relaxed)
        .max(1000);
    let mut ticker = tokio::time::interval(Duration::from_millis(current_interval_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shutdown = state.shutdown.subscribe();

    while crate::shutdown::tick_or_stop(&mut ticker, &mut shutdown).await {
        let now = Instant::now();
        let interval_secs = match last_refresh {
            Some(prev) => (now - prev).as_secs_f64(),
            None => 0.0,
        };
        last_refresh = Some(now);

        let tick_ts = chrono::Utc::now().timestamp();

        // ── Phase: refresh ──────────────────────────────────────────────
        tick_count = tick_count.wrapping_add(1);
        let do_discovery = tick_count.is_multiple_of(DISCOVERY_EVERY_N_TICKS);
        let t = Instant::now();
        sys.refresh_cpu_all();
        let d_cpu = t.elapsed();
        let t = Instant::now();
        sys.refresh_memory();
        let d_mem = t.elapsed();
        let t = Instant::now();
        disks.refresh(do_discovery);
        let d_disks = t.elapsed();
        let t = Instant::now();
        networks.refresh(do_discovery);
        let d_net = t.elapsed();
        let do_comp = tick_count.is_multiple_of(COMPONENTS_REFRESH_EVERY_N_TICKS);
        let t = Instant::now();
        if do_comp {
            components.refresh(do_discovery);
        }
        let d_comp = t.elapsed();

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
                &mut last_diskstats,
            )
            .await;
        }
        #[cfg(not(target_os = "linux"))]
        {
            pressure_snapshot = None;
        }

        // Move each owned metric into an `Arc` once, post-enrich. From this
        // point on, the broadcast send, the primer-cache write and every
        // SSE receiver pulling from the broadcast all share the same heap
        // allocation per metric — clones become atomic refcount bumps
        // instead of deep copies of the inner Vec/String. With N
        // subscribers fanning out we save (N - 1) deep clones per metric
        // per tick.
        let cpu = Arc::new(cpu_stats);
        let memory = Arc::new(memory_stats);
        let disks_arc = Arc::new(disk_stats);
        let network = Arc::new(network_stats);
        let pressure = pressure_snapshot.map(Arc::new);
        // The DB layer always wants the full components snapshot; the
        // broadcast/cache path only emits one when the list is non-empty.
        // Both share the same `Arc`.
        let components_full = Arc::new(components_snapshot);
        let components_event =
            (!components_full.components.is_empty()).then(|| Arc::clone(&components_full));

        let compute_dur = t_compute.elapsed();

        // ── Phase: broadcast ────────────────────────────────────────────
        // Empty Components frames are dropped.
        let t_broadcast = Instant::now();
        let _ = state.stats_tx.send(StatsEvent::Cpu(Arc::clone(&cpu)));
        let _ = state.stats_tx.send(StatsEvent::Memory(Arc::clone(&memory)));
        let _ = state
            .stats_tx
            .send(StatsEvent::Disk(Arc::clone(&disks_arc)));
        let _ = state
            .stats_tx
            .send(StatsEvent::Network(Arc::clone(&network)));
        if let Some(p) = pressure.as_ref() {
            let _ = state.stats_tx.send(StatsEvent::Pressure(Arc::clone(p)));
        }
        if let Some(c) = components_event.as_ref() {
            let _ = state.stats_tx.send(StatsEvent::Components(Arc::clone(c)));
        }
        let broadcast_dur = t_broadcast.elapsed();

        // SSE primer cache. Written after broadcast to preserve the
        // invariant that a live subscriber never sees a frame the cache
        // hasn't seen. Every field is an `Arc::clone` — the bundle and
        // the in-flight broadcast events share storage.
        let bundle = AllStats {
            cpu: Arc::clone(&cpu),
            memory: Arc::clone(&memory),
            disks: Arc::clone(&disks_arc),
            network: Arc::clone(&network),
            pressure: pressure.clone(),
            components: components_event,
        };
        *state.stats_latest.write().await = Some(bundle);

        // Wake the alert evaluator on fresh data — it resolves host metrics
        // from `stats_latest`, so it should re-evaluate the moment the
        // snapshot advances rather than on its own timer. `send_modify` bumps
        // the watch generation even with no subscribers.
        state.stats_signal.send_modify(|g| *g = g.wrapping_add(1));

        // ── Phase: db_write ─────────────────────────────────────────────
        let t_db = Instant::now();
        if let Err(e) = metrics_repo
            .insert_raw_tick(
                &cpu,
                &memory,
                &disks_arc,
                &network,
                pressure.as_deref(),
                // Only on the ticks that actually re-read the sensors. The
                // other 29 in 30 would restamp an unchanged reading with a
                // fresh timestamp — wasted writes, and a row asserting a
                // measurement that was never taken, which the rollup then
                // averages as though it were 30 observations. The live
                // broadcast and the SSE primer above still carry the cached
                // reading every tick; only the series stops repeating it.
                do_comp.then(|| components_full.as_ref()),
            )
            .await
        {
            warn!("failed to persist raw stats tick: {:?}", e);
        }
        let db_write_dur = t_db.elapsed();

        tick_stats.record(&[
            d_cpu,
            d_mem,
            d_disks,
            d_net,
            d_comp,
            compute_dur,
            broadcast_dur,
            db_write_dur,
        ]);
        tick_stats.flush_if_needed();

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
#[allow(clippy::too_many_arguments)]
async fn enrich_linux(
    cpu: &mut CpuStats,
    memory: &mut MemoryStats,
    disks: &mut [DiskStats],
    interval_secs: f64,
    tick_ts: i64,
    last_proc_stat: &mut Option<ProcStatSnapshot>,
    last_vmstat: &mut Option<VmstatSnapshot>,
    last_diskstats: &mut Option<HashMap<String, DiskstatEntry>>,
) -> Option<PressureSnapshot> {
    // /proc/stat: percentages AND kernel-event rates need a prior snapshot.
    let cur_stat = system_linux::read_proc_stat();
    if let (Some(prev), Some(cur)) = (*last_proc_stat, cur_stat) {
        if let Some(extras) = system_linux::compute_cpu_extras(prev, cur) {
            cpu.steal_percent = Some(extras.steal_percent);
            cpu.iowait_percent = Some(extras.iowait_percent);
            cpu.guest_percent = Some(extras.guest_percent);
            cpu.user_percent = Some(extras.user_percent);
            cpu.system_percent = Some(extras.system_percent);
        }
        if let Some(rates) = system_linux::compute_kernel_event_rates(prev, cur, interval_secs) {
            cpu.context_switches_per_sec = Some(rates.context_switches_per_sec);
            cpu.process_forks_per_sec = Some(rates.process_forks_per_sec);
        }
    }
    *last_proc_stat = cur_stat;

    // /proc/vmstat: page faults and swap traffic — same prior-snapshot dance.
    let cur_vmstat = system_linux::read_vmstat();
    if let (Some(prev), Some(cur)) = (*last_vmstat, cur_vmstat)
        && let Some(rates) = system_linux::compute_vmstat_rates(prev, cur, interval_secs)
    {
        memory.page_faults_minor_per_sec = Some(rates.page_faults_minor_per_sec);
        memory.page_faults_major_per_sec = Some(rates.page_faults_major_per_sec);
        memory.swap_in_pages_per_sec = Some(rates.swap_in_pages_per_sec);
        memory.swap_out_pages_per_sec = Some(rates.swap_out_pages_per_sec);
    }
    *last_vmstat = cur_vmstat;

    // /proc/diskstats: IOPS + utilization per block device.
    // Mount points don't map 1:1 to device names, so we match by the
    // device name sysinfo exposes (last component of the device path).
    let cur_diskstats = system_linux::read_diskstats();
    if let (Some(prev_map), Some(cur_map)) = (last_diskstats.as_ref(), cur_diskstats.as_ref()) {
        for d in disks.iter_mut() {
            // sysinfo gives us the mount point; derive the device name from
            // the device field if available, otherwise skip this mount.
            // We match by iterating cur_map keys — the kernel reports the
            // bare device name (e.g. "sda", "nvme0n1", "vda").
            let dev_name = d.mount_point.trim_start_matches('/').replace('/', "_");
            // Try exact match first, then fallback: find the device whose
            // read+write delta is closest to what sysinfo reports in bytes.
            // For simplicity we just match by common device name patterns.
            for (dev, cur_entry) in cur_map {
                if let Some(prev_entry) = prev_map.get(dev) {
                    // Skip partition entries (e.g. sda1) if the parent device
                    // (sda) is also present — avoids double-counting.
                    let is_partition = dev.chars().last().is_some_and(|c| c.is_ascii_digit())
                        && cur_map.contains_key(dev.trim_end_matches(|c: char| c.is_ascii_digit()));
                    if is_partition {
                        continue;
                    }
                    if let Some(rates) =
                        system_linux::compute_disk_io_rates(prev_entry, cur_entry, interval_secs)
                    {
                        // Assign to first unset disk entry as a best-effort match.
                        // On single-disk systems this is always correct; on multi-disk
                        // systems with mount-to-device ambiguity it may misattribute.
                        if d.read_iops.is_none() {
                            d.read_iops = Some(rates.read_iops);
                            d.write_iops = Some(rates.write_iops);
                            d.io_util_percent = Some(rates.io_util_percent);
                        }
                    }
                }
            }
            let _ = dev_name; // suppress unused warning
        }
    }
    *last_diskstats = cur_diskstats;

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
