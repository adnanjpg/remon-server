use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use log::debug;
use sysinfo::{MINIMUM_CPU_UPDATE_INTERVAL, ProcessesToUpdate, System};

use crate::services::process;
use crate::services::tick_timer::TickStats;
use crate::state::AppState;

pub async fn run(state: Arc<AppState>) {
    let mut sys = System::new_all();

    // sysinfo computes per-process `cpu_usage()` as the delta between two
    // refreshes. `System::new_all()` already did one refresh internally,
    // so we have to wait at least `MINIMUM_CPU_UPDATE_INTERVAL` (200 ms)
    // before the next refresh — otherwise the first published tick reports
    // 0% across the board and that bad value would land in the cache.
    tokio::time::sleep(MINIMUM_CPU_UPDATE_INTERVAL).await;

    // 240-sample sliding window = 20 minutes at the 5s tick rate; flush
    // every 12 ticks (~1 min). Same reasoning as the stats collector:
    // distribution stability vs visibility freshness.
    let mut tick_stats = TickStats::new(
        "processes",
        &["refresh", "compute", "publish"],
        240,
        12,
    );

    loop {
        // ── Phase: refresh ──────────────────────────────────────────────
        // Surgical: only process info, not CPU/memory/disks/networks/
        // components inside `sys` — those get re-read by the stats collector.
        // NOTE: A/B benchmark on Windows showed processes refresh p50 went
        // up after this change, contrary to expectation. Most likely a
        // test-condition artifact (Windows AV scheduler) rather than a
        // real regression — needs re-validation on Linux to be sure.
        let t_refresh = Instant::now();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        let refresh_dur = t_refresh.elapsed();

        // ── Phase: compute ──────────────────────────────────────────────
        let t_compute = Instant::now();
        let process_list = process::get_processes(&sys);
        let compute_dur = t_compute.elapsed();

        debug!("Processes collected: {} total", process_list.total_count);

        // ── Phase: publish ──────────────────────────────────────────────
        // Latest-snapshot cache for one-shot REST consumers; broadcast for
        // future streaming subscribers. Cache write happens first so a
        // request that lands during the broadcast send still sees fresh
        // data. Both are best-effort — broadcast errors when there are no
        // subscribers, which is the normal case today.
        let t_publish = Instant::now();
        *state.processes_latest.write().await = Some(process_list.clone());
        let _ = state.processes_tx.send(process_list);
        let publish_dur = t_publish.elapsed();

        tick_stats.record(&[refresh_dur, compute_dur, publish_dur]);
        tick_stats.flush_if_needed();

        let interval_ms = state
            .collector_processes_interval_ms
            .load(Ordering::Relaxed)
            .max(500);
        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
    }
}
