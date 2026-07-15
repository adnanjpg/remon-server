// Spawned when `[assistant] process_history = true` (the default): besides
// keeping the process cache warm for GET /processes, each tick feeds the
// rolling per-process history that gives the assistant's `list_processes`
// its time context ("spike or steady state?"). With the flag off, process
// refresh falls back to on-demand with TTL-based caching.
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use log::{debug, warn};
use sysinfo::{MINIMUM_CPU_UPDATE_INTERVAL, ProcessesToUpdate, System};

use crate::models::process::{ProcessHistory, ProcessSample};
use crate::services::process;
use crate::services::tick_timer::TickStats;
use crate::state::AppState;
use crate::storage::repositories::{ProcessGroupRow, ProcessMetricsRepository};

/// How much per-process history is kept (and how far back the assistant can
/// reason about a single pid). At the default 5s tick this is 180 samples
/// per live process — small enough to stay purely in memory.
const HISTORY_WINDOW_SECS: i64 = 900;

/// Cadence of the persistent name-grouped series (`metrics_process`). One
/// write a minute keeps the row volume near the other metrics tables no
/// matter how fast the sampling tick runs.
const SERIES_WRITE_INTERVAL_SECS: i64 = 60;

/// Pick the groups worth persisting: top-K by cpu plus top-K by memory,
/// deduplicated. Two rankings because the interesting culprits differ — a
/// memory hog can idle at 0% cpu and must still make the cut.
fn select_top_groups(mut groups: Vec<ProcessGroupRow>, k: usize) -> Vec<ProcessGroupRow> {
    if k == 0 || groups.is_empty() {
        return Vec::new();
    }
    groups.sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent));
    let by_cpu: Vec<String> = groups.iter().take(k).map(|g| g.name.clone()).collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.memory_bytes));
    let keep: std::collections::HashSet<String> = by_cpu
        .into_iter()
        .chain(groups.iter().take(k).map(|g| g.name.clone()))
        .collect();
    groups.retain(|g| keep.contains(&g.name));
    groups
}

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

    // Persistent name-grouped series (metrics_process). K comes from config
    // and never changes at runtime; 0 disables the writes entirely.
    let series_top_k = state.assistant_config.process_series_top_k as usize;
    let series_repo = ProcessMetricsRepository::new(state.db.clone());
    let mut last_series_write: i64 = 0;

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
        // Series write ticks also fold the same per-pid pass into name
        // groups, so persistence adds no extra process iteration.
        let write_series =
            series_top_k > 0 && now_ts - last_series_write >= SERIES_WRITE_INTERVAL_SECS;
        let mut groups: std::collections::HashMap<String, ProcessGroupRow> =
            std::collections::HashMap::new();
        {
            let mut hist = state.process_history.write().await;
            for (pid, proc_) in sys.processes() {
                let name = proc_.name().to_string_lossy().into_owned();
                let du = proc_.disk_usage();
                let disk_read_bps = (du.read_bytes as f64 / interval_secs) as u64;
                let disk_write_bps = (du.written_bytes as f64 / interval_secs) as u64;

                if write_series {
                    let g = groups
                        .entry(name.clone())
                        .or_insert_with(|| ProcessGroupRow {
                            name: name.clone(),
                            pid_count: 0,
                            cpu_percent: 0.0,
                            memory_bytes: 0,
                            disk_read_bps: 0,
                            disk_write_bps: 0,
                        });
                    g.pid_count += 1;
                    g.cpu_percent += proc_.cpu_usage() as f64;
                    g.memory_bytes += proc_.memory() as i64;
                    g.disk_read_bps += disk_read_bps as i64;
                    g.disk_write_bps += disk_write_bps as i64;
                }

                let entry = hist
                    .entry(pid.as_u32())
                    .or_insert_with(ProcessHistory::default);
                if entry.name != name {
                    entry.samples.clear();
                    entry.name = name;
                }
                entry.samples.push_back(ProcessSample {
                    ts: now_ts,
                    cpu_percent: proc_.cpu_usage(),
                    memory_bytes: proc_.memory(),
                    disk_read_bps,
                    disk_write_bps,
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

        if write_series {
            let rows = select_top_groups(groups.into_values().collect(), series_top_k);
            if !rows.is_empty() {
                match series_repo.insert_tick(now_ts, &rows).await {
                    Ok(()) => last_series_write = now_ts,
                    Err(e) => warn!("process series write failed: {e}"),
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, cpu: f64, mem: i64) -> ProcessGroupRow {
        ProcessGroupRow {
            name: name.to_string(),
            pid_count: 1,
            cpu_percent: cpu,
            memory_bytes: mem,
            disk_read_bps: 0,
            disk_write_bps: 0,
        }
    }

    #[test]
    fn union_of_cpu_and_memory_rankings() {
        // cpu-hot but memory-cold, memory-hot but cpu-cold, both-cold.
        let groups = vec![
            row("cpu-hog", 95.0, 10),
            row("mem-hog", 0.1, 8_000_000_000),
            row("idle", 0.0, 5),
        ];
        let picked = select_top_groups(groups, 1);
        let names: Vec<&str> = picked.iter().map(|g| g.name.as_str()).collect();
        assert!(names.contains(&"cpu-hog"), "top-by-cpu must survive");
        assert!(names.contains(&"mem-hog"), "top-by-memory must survive");
        assert!(!names.contains(&"idle"), "cold group must be dropped");
    }

    #[test]
    fn k_zero_disables_selection() {
        assert!(select_top_groups(vec![row("a", 50.0, 100)], 0).is_empty());
    }

    #[test]
    fn overlap_is_deduplicated() {
        // The same group tops both rankings — must appear exactly once.
        let groups = vec![row("both", 90.0, 9_000_000_000), row("meh", 1.0, 10)];
        let picked = select_top_groups(groups, 1);
        assert_eq!(picked.iter().filter(|g| g.name == "both").count(), 1);
    }

    #[test]
    fn k_larger_than_input_keeps_everything() {
        let picked = select_top_groups(vec![row("a", 1.0, 1), row("b", 2.0, 2)], 50);
        assert_eq!(picked.len(), 2);
    }
}
