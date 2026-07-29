//! Data-layer measurement suite — the baseline a change is judged against.
//!
//! Not a test: nothing here asserts. Each case seeds a file-backed database
//! shaped like a real host's retained history and prints one `BENCH` line per
//! measurement, so two runs can be diffed directly.
//!
//! ```text
//! cargo test --profile bench-fast --bin remon-server dbbench -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `--test-threads=1` is not optional. The cases share one disk and one CPU;
//! run in parallel they measure each other's contention, which is enough to
//! invert an A/B pair.
//!
//! Three deliberate choices, each of which changes the numbers if ignored:
//!
//! * **`bench-fast`, never `dev`.** Under `dev` the bundled SQLite is compiled
//!   at `opt-level = 0`, which inflates everything by roughly an order of
//!   magnitude and does so unevenly — a query dominated by SQLite's b-tree
//!   walk and one dominated by Rust-side row mapping move by different
//!   factors, so even the *ratios* stop being comparable.
//! * **File-backed, never `:memory:`.** WAL growth, checkpointing, page
//!   eviction and delete lock-hold are the things being measured and none of
//!   them exist in an in-memory database.
//! * **Seeded to the real steady state, not to a convenient size.** A live
//!   database is not "N hours of samples": retention holds raw for 24 h, 1m
//!   for 7 days, 5m for 30 days and 1h for a year, so the rolled-up tiers
//!   outweigh the raw one and every b-tree is correspondingly deeper. Seeding
//!   raw alone understates every measurement here. `BENCH_DIV` divides all
//!   four windows for a quick run (default 1 = the real thing); the row count
//!   each run actually produced is printed.
//!
//! Absolute timings are machine-specific. What travels between runs is the
//! ratio before and after a change, at the same `BENCH_DIV` on the same box.

use std::time::Instant;

use sqlx::SqlitePool;

use super::TestApp;
use crate::services::alerting::{expression, resolver};

/// Sensor/mount/interface/core counts the seed builds. Chosen to be an
/// unremarkable small server rather than a worst case — a 16-core host moves
/// `metrics_cpu_cores` and `metrics_components` proportionally.
const MOUNTS: &[&str] = &["/", "/home", "/var", "/tmp"];
const IFACES: &[&str] = &["eth0", "wg0", "docker0"];
const CORES: i64 = 8;
const SENSORS: i64 = 8;
/// Distinct process-name groups, bounded by `process_series_top_k` in prod.
const PROC_GROUPS: i64 = 30;

/// Containers the docker tier is built at. The resolver seeks once per key, and
/// containers are the namespace where that count is neither small nor fixed —
/// so it is a knob rather than a constant. `BENCH_CONTAINERS` overrides.
fn bench_containers() -> i64 {
    std::env::var("BENCH_CONTAINERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|c| *c > 0)
        .unwrap_or(12)
}

/// Key counts for the series the tier loop does not build.
const SMART_DEVICES: i64 = 2;
const PROBES: i64 = 3;
const PROBE_METRICS: i64 = 4;

/// The docker collector writes on its own 3s cadence, not the 2s raw tick.
const DOCKER_RAW_INTERVAL_SECS: i64 = 3;

/// `(seconds between rows, seconds retained)` for the tables that grow at event
/// rate rather than tick rate. The cadence is each producer's real interval;
/// the window is what `retention_policy` gives that resource in the migration.
const LOGS_RATE: (i64, i64) = (60, 2_592_000);
const HOST_EVENTS_RATE: (i64, i64) = (4_320, 7_776_000);
const ALERT_EVENTS_RATE: (i64, i64) = (21_600, 7_776_000);
const PROBE_RUNS_RATE: (i64, i64) = (60, 2_592_000);
const HEARTBEAT_RATE: (i64, i64) = (300, 2_592_000);
const INCIDENTS_RATE: (i64, i64) = (86_400, 2_592_000);
const SMART_RATE: (i64, i64) = (1_800, 31_536_000);
const HEARTBEAT_CHECKS: i64 = 2;

/// Divisor applied to every retention window. 1 seeds the true steady state.
fn bench_div() -> i64 {
    std::env::var("BENCH_DIV")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|d| *d > 0)
        .unwrap_or(1)
}

/// `(resolution, sample interval, retained seconds)` — straight out of the
/// `resolutions` and `retention_policy` seeds in the migration. Together these
/// are the shape a database converges to and stays at.
const TIERS: &[(&str, i64, i64)] = &[
    ("raw", 2, 86_400),
    ("1m", 60, 604_800),
    ("5m", 300, 2_592_000),
    ("1h", 3600, 31_536_000),
];

/// One measurement line. Fixed-width so a diff of two runs lines up.
fn report(name: &str, value: impl std::fmt::Display, unit: &str) {
    println!("BENCH  {name:<38} {value:>12}  {unit}");
}

fn micros(d: std::time::Duration) -> u128 {
    d.as_micros()
}

/// Resident set of this process. SQLite's page cache is a private allocation
/// per connection with no SQL-visible counter, so the only way to see what a
/// `cache_size` change actually costs is to ask the OS.
fn rss_bytes() -> u64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: the struct is zeroed and its size passed as the API
        // requires; the handle is a pseudo-handle that needs no release.
        unsafe {
            let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) != 0 {
                return c.WorkingSetSize as u64;
            }
        }
        0
    }
    #[cfg(target_os = "linux")]
    {
        // statm field 2 is resident pages.
        std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|pages| pages * 4096)
            .unwrap_or(0)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        0
    }
}

/// A file-backed database in the OS temp dir, wired exactly like production
/// (same pragmas, same pool construction) via the normal harness.
async fn app_on_disk(tag: &str) -> (TestApp, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("remon-dbbench-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create bench dir");
    let path = dir.join("bench.sqlite3");
    let url = format!("sqlite:{}", path.display());
    let app = TestApp::spawn_at(&url, 5).await;
    (app, dir)
}

/// Seed every tier to its retention window, ending at "now".
///
/// Anchored on the current time on purpose: the rollup reads the most recent
/// *closed* bucket, so history sitting at some fixed epoch in the past would
/// leave it nothing to fold and the steady-state measurement would time an
/// empty sweep.
///
/// Written as recursive CTEs rather than through the repositories: this
/// produces millions of rows before every case, and going through the insert
/// path would make the seed itself the dominant cost.
async fn seed(pool: &SqlitePool) -> i64 {
    let div = bench_div();
    let t0 = Instant::now();
    let now = chrono::Utc::now().timestamp();

    let series = |n: i64| {
        format!(
            "WITH RECURSIVE t(n) AS (SELECT 0 UNION ALL SELECT n+1 FROM t WHERE n < {} - 1)",
            n
        )
    };
    let values_of = |items: &[&str]| {
        items
            .iter()
            .map(|m| format!("SELECT '{}' AS k", m))
            .collect::<Vec<_>>()
            .join(" UNION ALL ")
    };
    let ints_of = |n: i64| {
        (0..n)
            .map(|i| format!("SELECT {} AS k", i))
            .collect::<Vec<_>>()
            .join(" UNION ALL ")
    };

    let mut stmts: Vec<String> = Vec::new();
    for (res, interval, retained) in TIERS {
        let step = *interval;
        let ticks = (retained / step / div).max(1);
        // End the series at now so the newest bucket is the one the rollup
        // and the resolver would actually be looking at.
        let base = now - ticks * step;

        stmts.push(format!(
            "{cte} INSERT OR IGNORE INTO metrics_cpu
               (resolution, timestamp, usage_percent, load_1m, load_5m, load_15m,
                steal_percent, iowait_percent, guest_percent, user_percent, system_percent,
                context_switches_per_sec, process_forks_per_sec)
             SELECT '{res}', {base} + n*{step}, 20.0 + (n % 40), 1.0, 1.1, 1.2,
                    NULL, NULL, NULL, 15.0, 5.0, 4000, 20 FROM t",
            cte = series(ticks)
        ));
        stmts.push(format!(
            "{cte} INSERT OR IGNORE INTO metrics_memory
               (resolution, timestamp, total_bytes, used_bytes, available_bytes,
                cached_bytes, swap_used_bytes,
                page_faults_minor_per_sec, page_faults_major_per_sec,
                swap_in_pages_per_sec, swap_out_pages_per_sec)
             SELECT '{res}', {base} + n*{step}, 16000000000, 8000000000 + n, 8000000000,
                    2000000000, 0, NULL, NULL, NULL, NULL FROM t",
            cte = series(ticks)
        ));
        stmts.push(format!(
            "{cte} INSERT OR IGNORE INTO metrics_disk
               (resolution, timestamp, mount_point, total_bytes, used_bytes, available_bytes,
                read_bytes_per_sec, write_bytes_per_sec, inode_used_percent,
                read_iops, write_iops, io_util_percent)
             SELECT '{res}', {base} + n*{step}, m.k, 500000000000, 250000000000 + n, 250000000000,
                    1000, 2000, NULL, 30, 40, NULL
             FROM t CROSS JOIN ({keys}) m",
            cte = series(ticks),
            keys = values_of(MOUNTS)
        ));
        stmts.push(format!(
            "{cte} INSERT OR IGNORE INTO metrics_network
               (resolution, timestamp, interface_name, rx_bytes_per_sec, tx_bytes_per_sec,
                rx_packets_per_sec, tx_packets_per_sec, errors_in_per_sec, errors_out_per_sec)
             SELECT '{res}', {base} + n*{step}, i.k, 100000 + n, 50000, 200, 100, 0, 0
             FROM t CROSS JOIN ({keys}) i",
            cte = series(ticks),
            keys = values_of(IFACES)
        ));
        stmts.push(format!(
            "{cte} INSERT OR IGNORE INTO metrics_components
               (resolution, timestamp, label, temperature_c, max_c, critical_c)
             SELECT '{res}', {base} + n*{step}, 'sensor' || s.k, 45.0 + (n % 20), 90.0, 100.0
             FROM t CROSS JOIN ({keys}) s",
            cte = series(ticks),
            keys = ints_of(SENSORS)
        ));
        stmts.push(format!(
            "{cte} INSERT OR IGNORE INTO metrics_pressure
               (resolution, timestamp, resource, some_avg10, some_avg60, some_avg300,
                full_avg10, full_avg60, full_avg300)
             SELECT '{res}', {base} + n*{step}, r.k, 1.0, 2.0, 3.0, 0.5, 0.6, 0.7
             FROM t CROSS JOIN (SELECT 'cpu' AS k UNION ALL SELECT 'memory' UNION ALL SELECT 'io') r",
            cte = series(ticks)
        ));

        // The collector writes the process series once a minute regardless of
        // the tick rate, so its raw tier is far sparser than the others.
        let p_step = if *res == "raw" { 60 } else { step };
        let p_ticks = (retained / p_step / div).max(1);
        let p_base = now - p_ticks * p_step;
        stmts.push(format!(
            "{cte} INSERT OR IGNORE INTO metrics_process
               (resolution, timestamp, name, pid_count, cpu_percent, memory_bytes,
                disk_read_bps, disk_write_bps)
             SELECT '{res}', {p_base} + n*{p_step}, 'proc' || g.k, 3, 5.0 + (n % 10), 100000000, 0, 0
             FROM t CROSS JOIN ({keys}) g",
            cte = series(p_ticks),
            keys = ints_of(PROC_GROUPS)
        ));

        // Per-core samples are raw-only by design — no rollup, no resolution
        // column — so this tier loop contributes them exactly once.
        if *res == "raw" {
            stmts.push(format!(
                "{cte} INSERT OR IGNORE INTO metrics_cpu_cores
                   (timestamp, core_index, usage_percent, freq_mhz)
                 SELECT {base} + n*{step}, c.k, 20.0 + (n % 50), 3200
                 FROM t CROSS JOIN ({keys}) c",
                cte = series(ticks),
                keys = ints_of(CORES)
            ));
        }
    }

    for s in stmts {
        sqlx::query(sqlx::AssertSqlSafe(s))
            .execute(pool)
            .await
            .expect("seed");
    }

    seed_operational(pool).await;

    // What production has once the retention pass has run once, which is within
    // a moment of startup. Measuring without statistics measures a state a live
    // database is only in before its first pass — and the plans differ.
    sqlx::query("ANALYZE").execute(pool).await.expect("analyze");

    let rows: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(
        "SELECT (SELECT COUNT(*) FROM metrics_cpu) + (SELECT COUNT(*) FROM metrics_memory)
              + (SELECT COUNT(*) FROM metrics_disk) + (SELECT COUNT(*) FROM metrics_network)
              + (SELECT COUNT(*) FROM metrics_cpu_cores) + (SELECT COUNT(*) FROM metrics_components)
              + (SELECT COUNT(*) FROM metrics_pressure) + (SELECT COUNT(*) FROM metrics_process)
              + (SELECT COUNT(*) FROM metrics_docker) + (SELECT COUNT(*) FROM metrics_probe)
              + (SELECT COUNT(*) FROM metrics_smart) + (SELECT COUNT(*) FROM logs)
              + (SELECT COUNT(*) FROM host_events) + (SELECT COUNT(*) FROM alert_events)
              + (SELECT COUNT(*) FROM probe_runs) + (SELECT COUNT(*) FROM heartbeat_pings)
              + (SELECT COUNT(*) FROM incident_snapshots)",
    ))
    .fetch_one(pool)
    .await
    .expect("count");

    let raw: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(
        "SELECT (SELECT COUNT(*) FROM metrics_cpu WHERE resolution='raw')
              + (SELECT COUNT(*) FROM metrics_disk WHERE resolution='raw')
              + (SELECT COUNT(*) FROM metrics_network WHERE resolution='raw')
              + (SELECT COUNT(*) FROM metrics_components WHERE resolution='raw')
              + (SELECT COUNT(*) FROM metrics_pressure WHERE resolution='raw')
              + (SELECT COUNT(*) FROM metrics_cpu_cores)
              + (SELECT COUNT(*) FROM metrics_docker WHERE resolution='raw')
              + (SELECT COUNT(*) FROM metrics_probe WHERE resolution='raw')",
    ))
    .fetch_one(pool)
    .await
    .unwrap_or(-1);

    report("seed.div", div, "(1 = real retention windows)");
    report("seed.rows_total", rows, "rows");
    report("seed.rows_raw_partition", raw, "rows");
    report("seed.elapsed", micros(t0.elapsed()) / 1000, "ms");
    rows
}

/// Fill the tables the tier loop does not reach: `metrics_docker`,
/// `metrics_probe` and `metrics_smart`, which are written on their producers'
/// own schedules, and the ledgers and histories that grow at event rate.
///
/// Seeding stopped at the eight tier tables, so the retention pass walked eight
/// of the seventeen tables production gives it and reported a total that could
/// only be a lower bound, while any plan over `logs`, `host_events` or
/// `incident_snapshots` was measured against an empty b-tree.
async fn seed_operational(pool: &SqlitePool) {
    let div = bench_div();
    let now = chrono::Utc::now().timestamp();
    let containers = bench_containers();

    let series = |n: i64| {
        format!(
            "WITH RECURSIVE t(n) AS (SELECT 0 UNION ALL SELECT n+1 FROM t WHERE n < {} - 1)",
            n.max(1)
        )
    };
    let ints_of = |n: i64| {
        (0..n)
            .map(|i| format!("SELECT {} AS k", i))
            .collect::<Vec<_>>()
            .join(" UNION ALL ")
    };
    // Rows for `(rate, window)`, and the timestamp the series starts at.
    let span = |(step, keep): (i64, i64)| {
        let ticks = (keep / step / div).max(1);
        (ticks, now - ticks * step, step)
    };

    let mut stmts: Vec<String> = Vec::new();

    // Foreign-key parents. The children carry the rows the measurements read;
    // these only have to exist and satisfy their constraints.
    stmts.push(
        "INSERT OR IGNORE INTO alert_rules (id, name, expression, severity)
         VALUES (1, 'bench-rule', 'cpu.usage_percent > 90', 'warn')"
            .into(),
    );
    for p in 0..PROBES {
        stmts.push(format!(
            "INSERT OR IGNORE INTO probe_definitions
               (name, enabled, schedule, timeout_ms, manifest_hash)
             VALUES ('probe{p}', 1, '60s', 30000, 'manifest{p}')"
        ));
    }
    for c in 0..HEARTBEAT_CHECKS {
        stmts.push(format!(
            "INSERT OR IGNORE INTO heartbeat_checks
               (id, name, slug_hash, period_secs, grace_secs)
             VALUES ({id}, 'check{c}', 'slughash{c}', 300, 60)",
            id = c + 1
        ));
    }

    // ── metric series on their own schedules ──────────────────────────────
    for (res, interval, retained) in TIERS {
        let step = if *res == "raw" {
            DOCKER_RAW_INTERVAL_SECS
        } else {
            *interval
        };
        let ticks = (retained / step / div).max(1);
        let base = now - ticks * step;
        stmts.push(format!(
            "{cte} INSERT OR IGNORE INTO metrics_docker
               (resolution, timestamp, container_id, cpu_percent, memory_used_bytes,
                memory_limit_bytes, network_rx_bytes, network_tx_bytes,
                block_read_bytes, block_write_bytes, pids)
             SELECT '{res}', {base} + n*{step}, 'container' || c.k, 5.0 + (n % 30),
                    200000000, 1000000000, 1000*n, 500*n, 4096*n, 2048*n, 8
             FROM t CROSS JOIN ({keys}) c",
            cte = series(ticks),
            keys = ints_of(containers)
        ));

        // Probes run on a schedule, so their raw tier is as sparse as the runs.
        let p_step = if *res == "raw" { 60 } else { *interval };
        let p_ticks = (retained / p_step / div).max(1);
        let p_base = now - p_ticks * p_step;
        stmts.push(format!(
            "{cte} INSERT OR IGNORE INTO metrics_probe
               (resolution, timestamp, probe_name, metric_name, labels, value)
             SELECT '{res}', {p_base} + n*{p_step}, 'probe' || p.k, 'metric' || m.k,
                    '{{}}', 1.0 + (n % 100)
             FROM t CROSS JOIN ({probes}) p CROSS JOIN ({metrics}) m",
            cte = series(p_ticks),
            probes = ints_of(PROBES),
            metrics = ints_of(PROBE_METRICS)
        ));
    }

    // SMART is raw-only and kept for a year; the poller runs every 30 minutes.
    let (ticks, base, step) = span(SMART_RATE);
    stmts.push(format!(
        "{cte} INSERT OR IGNORE INTO metrics_smart
           (resolution, timestamp, device, model, serial, health_passed,
            temperature_c, power_on_hours, power_cycles)
         SELECT 'raw', {base} + n*{step}, 'sd' || d.k, 'MODEL', 'SERIAL', 1,
                35.0 + (n % 10), n, 12
         FROM t CROSS JOIN ({keys}) d",
        cte = series(ticks),
        keys = ints_of(SMART_DEVICES)
    ));

    // ── event-rate tables ─────────────────────────────────────────────────
    let (ticks, base, step) = span(LOGS_RATE);
    stmts.push(format!(
        "{cte} INSERT INTO logs (timestamp, level, source, target, message)
         SELECT {base} + n*{step}, 2, 'server', 'remon_server::collectors',
                'collector tick completed in ' || (n % 50) || 'ms'
         FROM t",
        cte = series(ticks)
    ));

    let (ticks, base, step) = span(HOST_EVENTS_RATE);
    stmts.push(format!(
        "{cte} INSERT INTO host_events (created_at, source, kind, severity, message)
         SELECT {base} + n*{step}, 'system', 'server_started', 'info',
                'Server started after a clean shutdown'
         FROM t",
        cte = series(ticks)
    ));

    let (ticks, base, step) = span(ALERT_EVENTS_RATE);
    stmts.push(format!(
        "{cte} INSERT INTO alert_events
           (rule_id, label_set, event_type, severity, occurred_at, metric_value, notified)
         SELECT 1, '{{}}', CASE n % 2 WHEN 0 THEN 'fired' ELSE 'resolved' END,
                'warn', {base} + n*{step}, 91.5, 1
         FROM t",
        cte = series(ticks)
    ));

    let (ticks, base, step) = span(PROBE_RUNS_RATE);
    stmts.push(format!(
        "{cte} INSERT INTO probe_runs
           (probe_name, timestamp, duration_ms, exit_code, message, parse_ok)
         SELECT 'probe' || p.k, {base} + n*{step}, 12 + (n % 40), 0, NULL, 1
         FROM t CROSS JOIN ({probes}) p",
        cte = series(ticks),
        probes = ints_of(PROBES)
    ));

    let (ticks, base, step) = span(HEARTBEAT_RATE);
    stmts.push(format!(
        "{cte} INSERT INTO heartbeat_pings
           (check_id, received_at, kind, exit_code, source_ip, user_agent)
         SELECT c.k + 1, {base} + n*{step}, 'success', 0, '10.0.0.5', 'curl/8.5.0'
         FROM t CROSS JOIN ({keys}) c",
        cte = series(ticks),
        keys = ints_of(HEARTBEAT_CHECKS)
    ));

    // Few rows, but each carries a captured bundle — the reason a query that
    // selects `bundle` when it only needs the row's metadata costs what it does.
    let (ticks, base, step) = span(INCIDENTS_RATE);
    stmts.push(format!(
        "{cte} INSERT INTO incident_snapshots
           (created_at, trigger_kind, category, rule_id, rule_name, label_set,
            metric_value, bundle)
         SELECT {base} + n*{step}, 'alert', 'resource', 1, 'bench-rule', '{{}}',
                91.5, hex(zeroblob(8192))
         FROM t",
        cte = series(ticks)
    ));

    for s in stmts {
        sqlx::query(sqlx::AssertSqlSafe(s))
            .execute(pool)
            .await
            .expect("seed operational");
    }
}

/// Key counts the cardinality sweep walks. The resolver seeks once per key, so
/// this is the axis its cost actually moves along — measured across a range
/// rather than guessed at a single host's shape. `BENCH_KEYS` overrides.
/// The default stops at 20 because each point builds its own database, and the
/// harness leaks a pool per case (two spawned tasks hold the `AppState` that
/// owns the sender keeping them alive), so a later `journal_mode = WAL` can lose
/// the race for its exclusive lock — `busy_timeout` is set after it in the same
/// pragma string. Higher points are reachable with `BENCH_KEYS=2,8,20,50` on a
/// run of this case alone.
fn sweep_keys() -> Vec<i64> {
    match std::env::var("BENCH_KEYS") {
        Ok(v) => v
            .split(',')
            .filter_map(|s| s.trim().parse::<i64>().ok())
            .filter(|k| *k > 0)
            .collect(),
        Err(_) => vec![2, 4, 8, 20],
    }
}

/// `metrics_disk` alone, every tier, at an arbitrary key count.
///
/// Only this table, because the queries under measurement touch only this
/// table — but every tier of it, because the b-tree the seek descends is the
/// whole table, not the `raw` partition. Seeding `raw` alone would report a
/// shallower tree than production has.
async fn seed_disk(pool: &SqlitePool, keys: i64) {
    let div = bench_div();
    let now = chrono::Utc::now().timestamp();

    for (res, interval, retained) in TIERS {
        let step = *interval;
        let ticks = (retained / step / div).max(1);
        let base = now - ticks * step;
        let mounts = (0..keys)
            .map(|i| format!("SELECT {} AS k", i))
            .collect::<Vec<_>>()
            .join(" UNION ALL ");

        // `inode_used_percent` and `io_util_percent` stay NULL for every row —
        // which is not a convenience, it is the state of an optional field on a
        // host that never populates it, and it is the state the seek behaves
        // worst in.
        let sql = format!(
            "WITH RECURSIVE t(n) AS (SELECT 0 UNION ALL SELECT n+1 FROM t WHERE n < {ticks} - 1)
             INSERT OR IGNORE INTO metrics_disk
               (resolution, timestamp, mount_point, total_bytes, used_bytes, available_bytes,
                read_bytes_per_sec, write_bytes_per_sec, inode_used_percent,
                read_iops, write_iops, io_util_percent)
             SELECT '{res}', {base} + n*{step}, 'mnt' || m.k, 500000000000,
                    250000000000 + n, 250000000000, 1000, 2000, NULL, 30, 40, NULL
             FROM t CROSS JOIN ({mounts}) m"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(pool)
            .await
            .expect("seed disk");
    }
}

/// What the alert resolver's DB path costs as a function of key count, and what
/// it costs when the selected column is NULL for every row it could return.
///
/// Both axes exist because the single-shape measurement in [`dbbench_resolver`]
/// cannot separate them: it reported one number at one host's cardinality, and
/// the question that decides whether this path matters is how that number moves
/// — with the number of mounts, containers or interfaces a host happens to
/// have, and with whether the field is one this host ever fills in.
#[tokio::test]
#[ignore]
async fn dbbench_resolver_cardinality() {
    let populated = expression::parse("disk.used_bytes > 1").expect("parse");
    let all_null = expression::parse("disk.inode_used_percent > 1").expect("parse");

    for keys in sweep_keys() {
        let (app, dir) = app_on_disk(&format!("card{keys}")).await;
        let t = Instant::now();
        seed_disk(&app.state.db, keys).await;
        report(
            &format!("card.{keys}.seed"),
            micros(t.elapsed()) / 1000,
            "ms",
        );

        // Both arms are measured twice: once as production runs today, and once
        // after `ANALYZE`. Nothing in the server ever runs it, so `sqlite_stat1`
        // does not exist on a live database and every plan is chosen from
        // SQLite's built-in guesses — which is why the per-key seek falls back
        // to the PRIMARY KEY instead of the index built for it.
        for stats in ["no_stats", "analyzed"] {
            if stats == "analyzed" {
                let t = Instant::now();
                sqlx::query(sqlx::AssertSqlSafe("ANALYZE"))
                    .execute(&app.state.db)
                    .await
                    .expect("analyze");
                report(
                    &format!("card.{keys}.analyze_cost"),
                    micros(t.elapsed()) / 1000,
                    "ms",
                );
            }

            for (label, parsed) in [("populated", &populated), ("all_null", &all_null)] {
                // One untimed pass so first-touch page faults land outside the
                // measurement, matching `dbbench_resolver`.
                let _ = resolver::resolve(&app.state.db, &parsed.metric).await;

                let t = Instant::now();
                let out = resolver::resolve(&app.state.db, &parsed.metric).await;
                let took = micros(t.elapsed());
                let samples = out.map(|v| v.len()).unwrap_or(0);
                report(
                    &format!("card.{keys}.{label}.{stats}"),
                    took,
                    &format!("us  ({samples} samples)"),
                );
            }
        }

        // Closed before the next iteration opens its own: this case builds one
        // database per point, and five live pools against five WAL files is
        // enough for the last connection to be refused. Closing also lets the
        // directory actually go, instead of surviving the run.
        app.state.db.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}

async fn page_stats(pool: &SqlitePool, prefix: &str) {
    for (pragma, label) in [
        ("page_count", "page_count"),
        ("freelist_count", "freelist"),
        ("page_size", "page_size"),
    ] {
        let v: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("PRAGMA {pragma}")))
            .fetch_one(pool)
            .await
            .unwrap_or(-1);
        report(&format!("{prefix}.{label}"), v, "");
    }
}

// ─── Cases ──────────────────────────────────────────────────────────────────

/// The alert evaluator's DB path: one `resolve` per expression, which is what
/// every rule outside the in-memory snapshot pays on every eval tick.
///
/// The label-filtered and all-NULL variants are here because they are the two
/// shapes that behave counter-intuitively — worth having on the record so a
/// rewrite is judged on all three, not just the easy one.
#[tokio::test]
#[ignore]
async fn dbbench_resolver() {
    let (app, dir) = app_on_disk("resolver").await;
    seed(&app.state.db).await;

    let cases = [
        ("resolve.disk.used_bytes", "disk.used_bytes > 1"),
        (
            "resolve.disk.used_bytes{mount}",
            "disk.used_bytes{mount_point=\"/\"} > 1",
        ),
        // Enriched/optional field: excluded from the in-memory snapshot
        // whitelist by design, so this is one of the shapes that genuinely
        // takes the DB path on every eval tick.
        (
            "resolve.disk.inode_used(keyed,null)",
            "disk.inode_used_percent > 1",
        ),
        ("resolve.network.rx", "network.rx_bytes_per_sec > 1"),
        ("resolve.process.cpu", "process.cpu_percent > 1"),
        ("resolve.components.temp", "components.temperature_c > 1"),
        ("resolve.cpu.usage(unkeyed)", "cpu.usage_percent > 1"),
        ("resolve.cpu.steal(unkeyed,null)", "cpu.steal_percent > 1"),
    ];

    for (name, expr) in cases {
        let parsed = expression::parse(expr).expect("parse");
        // One untimed pass so the measurement isn't dominated by first-touch
        // page faults on a freshly written file.
        let _ = resolver::resolve(&app.state.db, &parsed.metric).await;

        let t = Instant::now();
        let out = resolver::resolve(&app.state.db, &parsed.metric).await;
        let took = micros(t.elapsed());
        // A resolve that errors returns instantly; reported as a sample count
        // it would read as a fast query rather than one that never ran.
        match out {
            Ok(v) => report(name, took, &format!("us  ({} samples)", v.len())),
            Err(e) => report(name, took, &format!("us  ERROR: {e}")),
        }
    }

    page_stats(&app.state.db, "resolver").await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// The collector write path. The `with`/`without` pair brackets exactly what
/// gating the per-tick components write would save: today every tick carries
/// them, while the sensor read behind it only refreshes every 30th.
#[tokio::test]
#[ignore]
async fn dbbench_write_tick() {
    use crate::models::stats::*;
    use crate::storage::repositories::MetricsRepository;

    let (app, dir) = app_on_disk("write").await;
    let repo = MetricsRepository::new(app.state.db.clone());
    let base = 1_800_000_000i64;

    let mk = |ts: i64| {
        let cpu = CpuStats {
            usage_percent: 30.0,
            per_core: (0..CORES)
                .map(|i| CoreStats {
                    core_index: i as u32,
                    usage_percent: 25.0,
                    freq_mhz: 3200,
                })
                .collect(),
            load_avg: LoadAverage {
                one: 1.0,
                five: 1.1,
                fifteen: 1.2,
            },
            timestamp: ts,
            steal_percent: None,
            iowait_percent: None,
            guest_percent: None,
            user_percent: Some(15.0),
            system_percent: Some(5.0),
            context_switches_per_sec: Some(4000),
            process_forks_per_sec: Some(20),
        };
        let memory = MemoryStats {
            total_bytes: 16_000_000_000,
            used_bytes: 8_000_000_000,
            available_bytes: 8_000_000_000,
            cached_bytes: 2_000_000_000,
            swap_total_bytes: 0,
            swap_used_bytes: 0,
            timestamp: ts,
            page_faults_minor_per_sec: None,
            page_faults_major_per_sec: None,
            swap_in_pages_per_sec: None,
            swap_out_pages_per_sec: None,
        };
        let disks: Vec<DiskStats> = MOUNTS
            .iter()
            .map(|m| DiskStats {
                mount_point: m.to_string(),
                total_bytes: 500_000_000_000,
                used_bytes: 250_000_000_000,
                available_bytes: 250_000_000_000,
                read_bytes_per_sec: 1000,
                write_bytes_per_sec: 2000,
                timestamp: ts,
                inode_used_percent: Some(12.5),
                read_iops: Some(30),
                write_iops: Some(40),
                io_util_percent: None,
            })
            .collect();
        let nets: Vec<NetworkStats> = IFACES
            .iter()
            .map(|i| NetworkStats {
                interface: i.to_string(),
                rx_bytes_per_sec: 100_000,
                tx_bytes_per_sec: 50_000,
                rx_packets_per_sec: 200,
                tx_packets_per_sec: 100,
                errors_in_per_sec: 0,
                errors_out_per_sec: 0,
                rx_bytes_total: 0,
                tx_bytes_total: 0,
                timestamp: ts,
            })
            .collect();
        let pressure = PressureSnapshot {
            cpu: Some(PressureStats {
                some_avg10: 1.0,
                some_avg60: 2.0,
                some_avg300: 3.0,
                full_avg10: 0.5,
                full_avg60: 0.6,
                full_avg300: 0.7,
            }),
            memory: None,
            io: None,
            timestamp: ts,
        };
        let components = ComponentsSnapshot {
            components: (0..SENSORS)
                .map(|i| ComponentInfo {
                    label: format!("sensor{i}"),
                    temperature_c: Some(45.0),
                    max_c: Some(90.0),
                    critical_c: Some(100.0),
                })
                .collect(),
            timestamp: ts,
        };
        (cpu, memory, disks, nets, pressure, components)
    };

    const TICKS: i64 = 20;
    const REPS: usize = 9;

    let mut with_us: Vec<u128> = Vec::new();
    let mut without_us: Vec<u128> = Vec::new();
    let mut ts = base;

    for rep in 0..REPS {
        // Alternate which arm runs first. A WAL checkpoint fires at a fixed
        // page count, so whichever arm happens to cross it absorbs its cost;
        // a fixed order charges that to the same arm every rep and can invert
        // the comparison outright.
        let with_first = rep % 2 == 0;
        for arm_with_components in [with_first, !with_first] {
            let t = Instant::now();
            for _ in 0..TICKS {
                ts += 2;
                let (c, m, d, n, p, comp) = mk(ts);
                let components = if arm_with_components {
                    Some(&comp)
                } else {
                    None
                };
                repo.insert_raw_tick(&c, &m, &d, &n, Some(&p), components)
                    .await
                    .expect("tick");
            }
            let per_tick = micros(t.elapsed()) / TICKS as u128;
            if arm_with_components {
                with_us.push(per_tick);
            } else {
                without_us.push(per_tick);
            }
        }
    }

    let median = |mut v: Vec<u128>| -> u128 {
        v.sort_unstable();
        v[v.len() / 2]
    };
    let with_comp = median(with_us);
    let without_comp = median(without_us);

    report("write.tick.with_components", with_comp, "us/tick");
    report("write.tick.without_components", without_comp, "us/tick");
    // Derived, not measured: the collector re-reads the sensors once every
    // `COMPONENTS_REFRESH_EVERY_N_TICKS` ticks, so this is what an average
    // tick costs when the series write follows that cadence instead of
    // repeating the cached reading every time.
    const REFRESH_EVERY: u128 = 30;
    let amortised = (with_comp + (REFRESH_EVERY - 1) * without_comp) / REFRESH_EVERY;
    report(
        "write.tick.amortised_at_1_in_30",
        amortised,
        "us/tick  (derived from the two arms)",
    );
    // Signed on purpose: a negative share means the two arms are inside the
    // noise floor and the run should be discarded, not read as a result.
    report(
        "write.tick.components_share",
        format!(
            "{:.1}",
            (with_comp as f64 - without_comp as f64) * 100.0 / with_comp.max(1) as f64
        ),
        "%",
    );

    let comp_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM metrics_components")
        .fetch_one(&app.state.db)
        .await
        .unwrap_or(-1);
    report("write.components_rows_written", comp_rows, "rows");

    page_stats(&app.state.db, "write").await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rollup, in the two states that matter: a normal tick with one closed bucket
/// to fold, and a tick for a resource that used to produce rows and stopped.
/// The second is the one that does not self-correct.
#[tokio::test]
#[ignore]
async fn dbbench_rollup() {
    let (app, dir) = app_on_disk("rollup").await;
    seed(&app.state.db).await;

    // Steady state: pretend every resource was folded up to one bucket ago.
    let now = chrono::Utc::now().timestamp();
    for res in [
        "cpu",
        "memory",
        "disk",
        "network",
        "docker",
        "process",
        "pressure",
        "components",
        "probe",
    ] {
        for (target, width) in [("1m", 60i64), ("5m", 300), ("1h", 3600)] {
            let cursor = (now / width - 2) * width;
            sqlx::query(
                "INSERT INTO rollup_state (resource, resolution, last_bucket_ts, last_run_at)
                 VALUES (?, ?, ?, 0)
                 ON CONFLICT(resource, resolution) DO UPDATE SET last_bucket_ts = excluded.last_bucket_ts",
            )
            .bind(res)
            .bind(target)
            .bind(cursor)
            .execute(&app.state.db)
            .await
            .expect("cursor");
        }
    }

    let t = Instant::now();
    crate::services::rollup::run_once(&app.state)
        .await
        .expect("rollup");
    report("rollup.tick.steady", micros(t.elapsed()) / 1000, "ms");

    // A cursor parked 900 buckets back with data behind it: the tick has real
    // aggregation to do and the per-tick budget is what stops it doing all of
    // it at once. This is the cost of catching up after an outage.
    for (target, width) in [("1m", 60i64), ("5m", 300), ("1h", 3600)] {
        let stale = (now / width - 900) * width;
        sqlx::query("UPDATE rollup_state SET last_bucket_ts = ? WHERE resource = 'docker' AND resolution = ?")
            .bind(stale)
            .bind(target)
            .execute(&app.state.db)
            .await
            .expect("stale cursor");
    }

    let t = Instant::now();
    crate::services::rollup::run_once(&app.state)
        .await
        .expect("rollup");
    report(
        "rollup.tick.backfill_with_data",
        micros(t.elapsed()) / 1000,
        "ms",
    );

    // The same stale cursor with nothing behind it. Every rollup resource is
    // seeded now, so the dark state has to be made rather than assumed: the
    // sweep walks bucket after bucket and none of them writes anything, and the
    // question is whether the cursor still advances or the range is re-swept
    // forever.
    sqlx::query("DELETE FROM metrics_docker")
        .execute(&app.state.db)
        .await
        .expect("empty docker");
    for (target, width) in [("1m", 60i64), ("5m", 300), ("1h", 3600)] {
        let stale = (now / width - 900) * width;
        sqlx::query("UPDATE rollup_state SET last_bucket_ts = ? WHERE resource = 'docker' AND resolution = ?")
            .bind(stale)
            .bind(target)
            .execute(&app.state.db)
            .await
            .expect("stale cursor");
    }

    let t = Instant::now();
    crate::services::rollup::run_once(&app.state)
        .await
        .expect("rollup");
    report(
        "rollup.tick.one_dark_resource",
        micros(t.elapsed()) / 1000,
        "ms",
    );

    // ...and again, to show whether the sweep converges or repeats.
    let t = Instant::now();
    crate::services::rollup::run_once(&app.state)
        .await
        .expect("rollup");
    report(
        "rollup.tick.dark_resource_repeat",
        micros(t.elapsed()) / 1000,
        "ms",
    );

    // The number that decides whether the sweep is self-correcting: how far
    // the cursor moved from where it was parked. Zero means every one of those
    // buckets will be swept again on the next tick, and the one after that.
    let cursor_after: i64 = sqlx::query_scalar(
        "SELECT last_bucket_ts FROM rollup_state WHERE resource='docker' AND resolution='1m'",
    )
    .fetch_one(&app.state.db)
    .await
    .unwrap_or(-1);
    let stale_1m = (now / 60 - 900) * 60;
    report(
        "rollup.dark_cursor_moved_by",
        cursor_after - stale_1m,
        "s  (0 = never advances)",
    );

    page_stats(&app.state.db, "rollup").await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Per-row INSERT loop against one multi-row VALUES, for the tick sizes the
/// repositories actually use. Both arms in one run, alternating, because the
/// difference is a property of the technique and has to be separated from
/// whatever else the machine is doing.
#[tokio::test]
#[ignore]
async fn dbbench_batch_insert() {
    let (app, dir) = app_on_disk("batch").await;
    let pool = &app.state.db;

    // 40 = two rankings of `process_series_top_k` deduplicated, the real
    // per-tick row count for the process series.
    const ROWS: i64 = 40;
    const REPS: usize = 9;

    let mut loop_us: Vec<u128> = Vec::new();
    let mut batch_us: Vec<u128> = Vec::new();
    let mut ts = 1_900_000_000i64;

    for rep in 0..REPS {
        for loop_first in [rep % 2 == 0, rep % 2 != 0] {
            ts += 60;
            let t = Instant::now();
            if loop_first {
                let mut tx = pool.begin().await.expect("begin");
                for i in 0..ROWS {
                    sqlx::query(
                        "INSERT INTO metrics_process
                           (resolution, timestamp, name, pid_count,
                            cpu_percent, memory_bytes, disk_read_bps, disk_write_bps)
                         VALUES ('raw', ?, ?, ?, ?, ?, ?, ?)
                         ON CONFLICT(resolution, timestamp, name) DO NOTHING",
                    )
                    .bind(ts)
                    .bind(format!("proc{i}"))
                    .bind(3i64)
                    .bind(5.0f64)
                    .bind(100_000_000i64)
                    .bind(0i64)
                    .bind(0i64)
                    .execute(&mut *tx)
                    .await
                    .expect("insert");
                }
                tx.commit().await.expect("commit");
            } else {
                let mut qb = sqlx::QueryBuilder::new(
                    "INSERT INTO metrics_process
                       (resolution, timestamp, name, pid_count,
                        cpu_percent, memory_bytes, disk_read_bps, disk_write_bps) ",
                );
                qb.push_values(0..ROWS, |mut b, i| {
                    b.push_bind("raw")
                        .push_bind(ts)
                        .push_bind(format!("proc{i}"))
                        .push_bind(3i64)
                        .push_bind(5.0f64)
                        .push_bind(100_000_000i64)
                        .push_bind(0i64)
                        .push_bind(0i64);
                });
                qb.push(" ON CONFLICT(resolution, timestamp, name) DO NOTHING");
                qb.build().execute(pool).await.expect("batch insert");
            }
            let took = micros(t.elapsed());
            if loop_first {
                loop_us.push(took);
            } else {
                batch_us.push(took);
            }
        }
    }

    let median = |mut v: Vec<u128>| -> u128 {
        v.sort_unstable();
        v[v.len() / 2]
    };
    let per_row_loop = median(loop_us);
    let one_batch = median(batch_us);

    report("batch.loop_in_tx", per_row_loop, "us/tick (40 rows)");
    report("batch.single_values", one_batch, "us/tick (40 rows)");
    report(
        "batch.saving",
        format!(
            "{:.1}",
            (per_row_loop as f64 - one_batch as f64) * 100.0 / per_row_loop.max(1) as f64
        ),
        "%",
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// What a `cache_size` change actually buys and costs, both arms in one run.
///
/// The page cache is private to each connection, so the configured size is a
/// per-connection ceiling multiplied by the pool — the reason the number is
/// worth revisiting at all. Shrinking it is only safe if the read path does
/// not pay for it, and that has to be measured against the same data in the
/// same process; between runs this machine drifts by more than the effect.
#[tokio::test]
#[ignore]
async fn dbbench_cache_size() {
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
    use std::str::FromStr;

    let (app, dir) = app_on_disk("cache").await;
    seed(&app.state.db).await;
    let path = dir.join("bench.sqlite3");
    // Release the seeding pool so its cache is not counted against the arms.
    app.state.db.close().await;
    drop(app);

    let baseline_rss = rss_bytes();
    report("cache.rss_baseline", baseline_rss / 1024 / 1024, "MB");

    for cache_kb in [-65_536i64, -8_000] {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite:{}", path.display()))
            .expect("opts")
            .journal_mode(SqliteJournalMode::Wal)
            .pragma("cache_size", cache_kb.to_string())
            .pragma("temp_store", "MEMORY")
            .pragma("mmap_size", (256u64 * 1024 * 1024).to_string());
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .expect("pool");

        // Force all five connections into existence and let each fill its own
        // cache — one at a time would only ever warm the first.
        let before = rss_bytes();
        for _ in 0..3 {
            let scans = (0..5).map(|_| async {
                let _: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM metrics_disk WHERE resolution = 'raw'",
                )
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
            });
            futures_util::future::join_all(scans).await;
        }

        let t = Instant::now();
        for _ in 0..5 {
            let _: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM metrics_disk WHERE resolution = 'raw'")
                    .fetch_one(&pool)
                    .await
                    .unwrap_or(0);
        }
        let read_us = micros(t.elapsed()) / 5;
        let after = rss_bytes();

        let label = if cache_kb == -65_536 { "64MB" } else { "8MB" };
        report(
            &format!("cache.{label}.rss_growth"),
            (after.saturating_sub(before)) / 1024 / 1024,
            "MB  (5 warmed connections)",
        );
        report(&format!("cache.{label}.read"), read_us, "us/scan");

        pool.close().await;
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Retention: one unbounded DELETE per policy. The number that matters is not
/// the total but the longest single statement — that is how long the sole
/// writer is unavailable to the collectors, against a 5 s busy timeout.
#[tokio::test]
#[ignore]
async fn dbbench_retention() {
    use crate::storage::repositories::MetricsRepository;

    let (app, dir) = app_on_disk("retention").await;
    seed(&app.state.db).await;
    let repo = MetricsRepository::new(app.state.db.clone());

    // Every resource the pass actually walks, not just the tier tables: the
    // ledgers, the log and the two histories are deleted by the same pass and
    // are what a total measured over eight tables was missing.
    const RESOURCES: &[&str] = &[
        "cpu",
        "memory",
        "disk",
        "network",
        "cpu_cores",
        "components",
        "pressure",
        "process",
        "docker",
        "probe",
        "smart",
        "logs",
        "probe_runs",
        "heartbeat_pings",
        "alert_events",
        "host_events",
        "incident_snapshots",
    ];
    let now = chrono::Utc::now().timestamp();

    // Each resource ages out on its own window — a day for the raw metric
    // tiers, 30 days for logs and the two histories, 90 for the ledgers, a year
    // for SMART. One shared cutoff would time an hour's worth on some tables
    // and a mass purge on others under the same label.
    let mut keep: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    for resource in RESOURCES {
        let secs: Option<i64> = sqlx::query_scalar(
            "SELECT keep_seconds FROM retention_policy WHERE resource = ? AND resolution = 'raw'",
        )
        .bind(resource)
        .fetch_optional(&app.state.db)
        .await
        .expect("read retention policy");
        keep.insert(resource, secs.unwrap_or(86_400));
    }

    // Two passes, because they are different questions.
    //
    // The hourly pass on a host that has been up for a while deletes exactly
    // the rows that aged out since the last one — an hour's worth. That is the
    // cost the daemon actually pays, forever.
    //
    // The second is what happens the first time an operator shortens a keep
    // window: one statement against most of a table. That is the case that
    // decides whether the delete has to be chunked, and it is reachable from
    // the API.
    for label in ["steady", "purge_to_1h"] {
        let mut worst = 0u128;
        let mut worst_res = "";
        let mut total = 0u128;
        for resource in RESOURCES {
            let cutoff = if label == "steady" {
                now - keep[*resource] + 3600
            } else {
                now - 3600
            };
            let t = Instant::now();
            let n = repo
                .delete_older_than(resource, "raw", cutoff)
                .await
                .expect("delete");
            let took = micros(t.elapsed());
            total += took;
            if took > worst {
                worst = took;
                worst_res = resource;
            }
            report(
                &format!("retention.{label}.{resource}"),
                took / 1000,
                &format!("ms  ({n} rows)"),
            );
        }
        report(&format!("retention.{label}.total"), total / 1000, "ms");
        report(
            &format!("retention.{label}.worst_stmt"),
            worst / 1000,
            &format!("ms  ({worst_res}; busy_timeout is 5000 ms)"),
        );
    }

    // WAL after the pass: an unchunked delete cannot be checkpointed while it
    // runs, so this is where the growth shows up.
    let wal = dir.join("bench.sqlite3-wal");
    let wal_bytes = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    report("retention.wal_bytes", wal_bytes, "bytes");

    page_stats(&app.state.db, "retention").await;
    let _ = std::fs::remove_dir_all(&dir);
}
