// Spawned when `[assistant] process_history = true` (the default): besides
// keeping the process cache warm for GET /processes, each tick feeds the
// rolling per-process history that gives the assistant's `list_processes`
// its time context ("spike or steady state?"). With the flag off, process
// refresh falls back to on-demand with TTL-based caching.
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use log::debug;
use sysinfo::{MINIMUM_CPU_UPDATE_INTERVAL, ProcessesToUpdate, System};

use crate::models::process::{ProcessHistory, ProcessSample};
use crate::services::process;
use crate::services::tick_timer::TickStats;
use crate::state::AppState;

/// How much per-process history is kept (and how far back the assistant can
/// reason about a single pid). At the default 5s tick this is 180 samples
/// per live process — small enough to stay purely in memory.
const HISTORY_WINDOW_SECS: i64 = 900;

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
    let mut tick_stats = TickStats::new("processes", &["refresh", "compute", "publish"], 240, 12);

    let mut current_interval_ms = state
        .processes_cache_ttl_ms
        .load(Ordering::Relaxed)
        .max(1000);
    let mut ticker = tokio::time::interval(Duration::from_millis(current_interval_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
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

        // Rolling history: one sample per live pid per tick, pruned to the
        // window. A name change on a pid means pid reuse — reset its run so
        // the old process's samples don't pollute the new one's trend.
        // sysinfo's DiskUsage read/written fields are deltas since the last
        // refresh, so dividing by the tick interval yields bytes/sec.
        let now_ts = chrono::Utc::now().timestamp();
        let interval_secs = (current_interval_ms as f64 / 1000.0).max(0.001);
        {
            let mut hist = state.process_history.write().await;
            for (pid, proc_) in sys.processes() {
                let name = proc_.name().to_string_lossy().into_owned();
                let entry = hist
                    .entry(pid.as_u32())
                    .or_insert_with(ProcessHistory::default);
                if entry.name != name {
                    entry.samples.clear();
                    entry.name = name;
                }
                let du = proc_.disk_usage();
                entry.samples.push_back(ProcessSample {
                    ts: now_ts,
                    cpu_percent: proc_.cpu_usage(),
                    memory_bytes: proc_.memory(),
                    disk_read_bps: (du.read_bytes as f64 / interval_secs) as u64,
                    disk_write_bps: (du.written_bytes as f64 / interval_secs) as u64,
                });
                while entry
                    .samples
                    .front()
                    .is_some_and(|s| now_ts - s.ts > HISTORY_WINDOW_SECS)
                {
                    entry.samples.pop_front();
                }
            }
            // Exited pids age out once their newest sample leaves the window.
            hist.retain(|_, h| {
                h.samples
                    .back()
                    .is_some_and(|s| now_ts - s.ts <= HISTORY_WINDOW_SECS)
            });
        }
        let compute_dur = t_compute.elapsed();

        debug!("processes collected: {} total", process_list.total_count);

        // ── Phase: publish ──────────────────────────────────────────────
        // Latest-snapshot cache for one-shot REST consumers; broadcast for
        // future streaming subscribers. Cache write happens first so a
        // request that lands during the broadcast send still sees fresh
        // data. Both are best-effort — broadcast errors when there are no
        // subscribers, which is the normal case today.
        //
        // The snapshot is wrapped in `Arc` so the cache write and the
        // broadcast send share one heap allocation. The broadcast channel
        // itself stores `Arc<ProcessList>`, so per-receiver delivery is
        // also a refcount bump rather than a deep clone of the Vec.
        let t_publish = Instant::now();
        let process_arc = Arc::new(process_list);
        *state.processes_latest.write().await = Some(Arc::clone(&process_arc));
        let _ = state.processes_tx.send(process_arc);
        let publish_dur = t_publish.elapsed();

        tick_stats.record(&[refresh_dur, compute_dur, publish_dur]);
        tick_stats.flush_if_needed();

        let new_interval_ms = state
            .processes_cache_ttl_ms
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
