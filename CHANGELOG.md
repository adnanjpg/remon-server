# Changelog

All notable changes to this project will be documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.15.3] - 2026-07-19

### Performance

- **Alert evaluator resolves host metrics from the live in-memory snapshot instead of the DB.** cpu/memory/disk/network/pressure rules on always-present fields (`usage_percent`, `used_bytes`, `used_percent`, network rates, PSI) now evaluate against the `stats_latest` snapshot the collector already maintains — zero DB round-trips per eval tick for the common case. Optional/enriched fields (`steal_percent`, `iowait_percent`, `inode_used_percent`, page faults, …) and the boot window (before the first snapshot) fall through to the DB query, preserving the resolver's latest-non-null fallback exactly. Phase 1 of moving alerting off the DB hot path; the evaluator, state machine, notifications and `/alerts/*` API are unchanged. Binary-only change — no schema change.

## [0.15.2] - 2026-07-19

### Performance

- **Alert evaluator stops full-scanning the raw metric tables every tick.** Added `(resolution, <key>, timestamp)` indexes to the keyed metrics tables (disk, network, docker, components, pressure, smart) so the resolver's "latest value per key" query (`WHERE resolution=? … GROUP BY <key>`) resolves the per-key `MAX(timestamp)` through an index seek instead of scanning the whole raw partition into a GROUP BY temp-b-tree sorter. A profiling pass (perf + heaptrack) pinned that sorter (`sqlite3VdbeSorterWrite`) as the #1 allocation site — ~4M short-lived allocations per minute under a couple of disk rules evaluating every 10s — and the SQLite worker thread as ~76% of process CPU. The index removes the sorter from the query plan; correctness is unchanged (same rows, same latest-non-null fallback). Schema-only change (no runtime behaviour change), folded into the initial migration per the pre-1.0 policy.

## [0.15.1] - 2026-07-19

### Added

- **`server_started` host event** — the daemon now records its own startup on every run, so a gap in the metric series from a plain restart (deploy, manual bounce) is explained on the timeline, not just host reboots. It is `warn` only for a genuine crash-restart (same host boot as the previous run, which never wrote its clean-shutdown marker) and `info` otherwise — including the first run and any start right after a host reboot (where the unclean-ness belongs to the reboot, already carried by the `boot` event). This folds in the former `agent_restart` event: the crash-restart signal is now `server_started` at `warn`.

## [0.15.0] - 2026-07-19

### Breaking

- Two new tables (`host_events`, `runtime_state`) folded into the initial migration (pre-1.0 policy — no migration chaining). Existing databases fail the migration checksum at boot: delete the database folder and re-pair devices after upgrading.

### Added

- **Host-event ledger (`host_events`)** — the discrete-happenings counterpart to the metric series: things that *happened*, recorded once, queryable forever (well, 90 days). Three sources: `system` (detected by the daemon), `operator` (an authenticated device did something through the API, with actor attribution), `agent` (reserved). Ages out via the standard retention engine.
- **`GET /events`** — one normalized timeline unioning the ledger with `alert_events` (projected as `alert_fired`/`alert_resolved`, severity mapped from the rule) and `incident_snapshots` (projected as `incident_captured`, `ref` pointing at the bundle id). Same range contract as `/metrics/*` (`start`/`end`/`limit`, default last 24 h), plus `kinds=` / `sources=` CSV filters — built to be drawn straight onto charts as annotation markers/bands and to back a timeline feed.
- **Boot / powercycle detection** — on startup the daemon compares the host's boot time against the persisted previous value: a change records a `boot` event *stamped with the actual boot moment* (so the annotation lines up with the gap in the charts), flagged `warn` when the previous run never wrote its clean-shutdown marker — power loss, crash, or hard reset. Same-boot restarts after an unclean exit record `agent_restart`.
- **OOM-kill sweep (Linux)** — a 5-minute kernel-journal scan turns "Out of memory: Killed process" lines (global and cgroup) into `oom_kill` events with pid/name, stamped at kill time and cursor-deduped across restarts. The only real answer to "why did my process vanish at 03:12".
- **SMART health transition events** — the SMART collector now diffs each device's verdict against the last known one (seeded from the DB, so failures that happen while the daemon is down are still caught): `passed→failed` records an error-severity `smart_health` event, recovery records `info`.
- **Operator audit trail** — mutating endpoints record who did what: service/timer actions, container lifecycle, process kills (with the process name when the snapshot has it), alert silence/unsilence, probe reloads, runtime-config/retention/resolution changes, device pairing and revocation. Attribution comes from the JWT device identity; the ledger write is fire-and-forget and never fails the action it records.

## [0.14.0] - 2026-07-18

### Added

- **`POST /assistant/stream`** — the assistant ask, streamed as SSE. Progress frames as the loop works: `step` (a model turn or tool call starting — the client can show *which* tool is running), `delta` (answer text as the provider generates it — native Anthropic hosts only; OpenAI-compat providers get steps and the answer arrives whole), then exactly one terminal `done` (full answer + proposals + optional trace, authoritative) or `error`. Pre-loop failures (bad request, disabled assistant, missing key) stay plain HTTP errors so clients can tell "can't start" from "died mid-answer". A disconnected client aborts the loop on its next frame — no tokens burn for a listener that left. The buffered `POST /assistant` is unchanged; older clients keep working.

## [0.13.0] - 2026-07-18

### Breaking

- `server_config` gained `collector_smart_interval_ms`, folded into the initial migration (pre-1.0 policy — no migration chaining). Existing databases fail the migration checksum at boot: delete the database folder and re-pair devices after upgrading.
- `[smart] interval_secs` left the TOML — the SMART poll interval is runtime config now (`PATCH /config { collector_smart_interval_ms }`, default 30 min, floor 60 s) and applies without a restart. A leftover `interval_secs` key in an existing TOML is ignored.

### Added

- **Notifications carry the server's name** — alert fired/resolved titles and the channel test notification are prefixed `[server_name]`, so several remon instances reporting into one Telegram chat / ntfy topic are tellable apart. The previously write-only `server_name` finally earns its keep.
- **`GET/PATCH /config/retention`** — read and batch-tune the per-(resource, resolution) keep windows behind the metrics pruner. The batch validates as a whole (nothing half-applies), floor 1 h, cap 10 y; the retention task picks changes up on its next tick.
- **`GET /config/resolutions`, `PATCH /config/resolutions/{name}`** — inspect and enable/disable rollup buckets. Chain guards keep the ladder contiguous: `raw` can't be disabled, a parent feeding an enabled child can't be disabled, a child under a disabled parent can't be enabled.
- **`server_name` in `GET /system/info`** — clients read the canonical name from the response they already cache; remon-web now prefers it over the locally-typed alias (sidebar, server cards) and syncs it on pairing.
- **`updated_at` in `GET/PATCH /config`** — audit timestamp of the last runtime-config write.

## [0.12.0] - 2026-07-14

### Added

- **Incident flight recorder** — the moment an alert first crosses its threshold (ok→pending), the daemon freezes a bounded context bundle: host vitals cross-section, top processes by cpu/memory with their recent in-memory history (spike vs steady state), the daemon's recent errors, system-level error events (journald; OOM kills, segfaults), co-active alerts and failed units — plus a T+60s follow-up sample. Captures are spawned and cooldown-deduped (15 min per rule+label), so rule evaluation latency is untouched and flapping can't spam. The core is trigger-agnostic: `POST /incidents/capture {reason, category}` lets an operator or an external detector (fail2ban action, IDS hook) anchor a snapshot on demand. Snapshots age out with the standard retention engine (30 days).
- **Assistant: incident tools** — `list_incidents` + `incident_detail` answer "what caused that alert at 03:12" from the frozen bundle instead of reconstructing; `capture_incident` freezes the current moment when the assistant notices something anomalous no alert covers (observability-only write — it never touches the host).
- **Assistant: `read_system_events` tool** — OS-level error/warning events: journald on Linux, the System+Application event logs on Windows (PowerShell `Get-WinEvent`). Bounded lines, look-back window, 10s hard timeout.
- **Persistent process series (`metrics_process`)** — once a minute the processes collector folds its per-pid pass into name groups and stores the top-K by cpu and by memory (union; `[assistant] process_series_top_k`, default 20, 0 disables). Bounded by K, never by the host's process table — the same trade prometheus' process-exporter makes. Rolled up raw→1m→5m→1h and aged out like every other metric.
- **`process` metric namespace** — `process.cpu_percent{name="clickhouse-server"} > 90` now works as an alert expression; `metric_history` answers "which process was eating cpu at 3am"; `query_metric` reads the latest per-group values. Quiet processes may have no samples (top-K caveat, documented in the tool description).

## [0.11.0] - 2026-07-13

### Added

- **Assistant: native Anthropic provider** — when `[assistant] base_url` points at `api.anthropic.com`, the assistant speaks the native Messages API instead of OpenAI-compat: same tool loop, plus prompt caching (`cache_control` breakpoints on the system+tools prefix and the growing conversation, so tool-loop round trips re-read context at ~0.1x input price) and per-turn usage logging (`in/out/cache_write/cache_read`) at debug level.
- **Assistant: `list_processes` grew real diagnostic depth** (driven by the assistant's own tool-gap review, gathered via dev mode): per-process `cmdline`, `parent_pid`, `uptime_seconds`, `threads`; `min_cpu_percent` / `min_memory_percent` / `name_contains` filters; docker container short-id from the pid's cgroup (Linux); and — when `[assistant] process_history = true` (default) — a `history` block per process (avg/max cpu, avg memory, disk read/write B/s over up to 15 minutes) so spike-vs-sustained is answerable before proposing a kill/restart. The previously dormant processes collector now runs continuously behind that flag, feeding an in-memory rolling window (and keeping GET /processes warm as a side effect).
- **Assistant: `list_probes` tool + probe metrics** — the assistant can now see custom probes (name, description, schedule, last-run status/message, latest emitted metrics) via `list_probes`, and read probe metric values/trends through `query_metric` / `metric_history` under the `probe` namespace (label `probe_name`). Previously it had no visibility into the probe engine at all.
- **Assistant: longer request timeout** — the `/assistant` endpoint now has its own 150s request timeout (the rest of the REST API keeps 30s). A multi-step tool-use loop — especially at higher effort or with dev `max_steps` raised — legitimately runs past 30s; it was being cut off with a 408. Web client timeout raised to match.
- **Assistant: `read_service_logs` tool** — one-shot `journalctl -u <unit>` tail (Linux), the read-only sibling of the SSE follow stream: bounded lines (10-200), optional `since_minutes`, 10s hard timeout, newest-lines-first size clamp, and the same unit-name charset validation as the SSE handler. The assistant can now diagnose any systemd service from its journal, not just docker containers.
- **Assistant dev mode: per-ask `model` override** — try a different model on the same provider (e.g. claude-haiku-4-5 vs claude-sonnet-5 on one question) without touching config; the trace's model turns name the model that ran.
- **Assistant: conversation memory** — `POST /assistant` accepts a client-replayed `history` of prior question/answer turns (the daemon stays stateless; capped at 12 turns, answers clipped), so follow-ups like "do all of those" resolve against the previous answer.
- **Assistant: dev mode** — with `[assistant] dev = true`, an ask may carry per-request overrides for developing the assistant itself: `system` (prompt iteration without a rebuild), `max_steps`/`max_tokens` (bounded ceilings), `no_tools` (bare-model chat), and `trace` (per-turn model usage/latency + per-tool args/result-preview/latency returned with the answer). Overrides are rejected with 403 while the flag is off; device auth and the read-only/propose-only tool contract apply regardless.
- **Assistant: transient-failure retry** — provider round trips retry on 429/5xx/network errors with bounded backoff (1s → 3s, `Retry-After` honored with a 10s cap) before surfacing an error. Applies to every provider, not just Anthropic.

## [0.10.0] - 2026-07-05

### Breaking

- `heartbeat_checks` / `heartbeat_pings` tables were folded into the initial migration (pre-1.0 policy — no migration chaining). Existing databases fail the migration checksum at boot: delete the database folder and re-pair devices after upgrading.

### Added

- **Heartbeat checks** — push-model dead-man's switches, the inverse of a probe: an external job (cron, backup, anything that can `curl`) proves liveness by pinging an anonymous capability URL; missing the deadline (`period + grace`) flips the check to `down`. No scheduler and no watchdog task — state is derived from timestamps at read/eval time, so the alert rule's own tick is the deadline check and nothing new can leak.
  - `GET|POST /ping/{slug}` (+ `/fail`, `/{exit_code}`) — the slug is the credential: 128-bit random, stored blake3-hashed, shown once at create/rotate, scrubbed from request logs like `access_token`. Fail bodies (≤4 KiB) land on the ping log for 3am debugging. Own per-IP rate-limit bucket sized for NAT'd cron fleets.
  - **Pause windows** ("this silence is expected") — operator pause via `POST /heartbeats/{id}/pause` (indefinite / until / duration, 30d cap), or announced by the service itself via `POST /ping/{slug}/pause?duration=3h` (24h cap; bare form means "quiet until my next ping"). Operator always outranks service. A pause expiring re-anchors the deadline — one fresh `period + grace` instead of an instant page at window end.
  - **`heartbeat` alert namespace** — `heartbeat.up < 1` (one unfiltered rule covers every check, current and future, via label_sets) and `heartbeat.late == 1` for a warn tier that holds through `down`. Paused/disabled checks read `up` so declared maintenance resolves an open alert instead of stranding it. Rules validate before the first check exists.
  - CRUD under `/heartbeats` incl. slug rotation and a 30-day ping log (`/heartbeats/{id}/pings`).

### Fixed

- Alert evaluator: a label_set that vanished from resolver output while `firing` stranded forever — never resolved, never re-notified. Vanished `firing` rows now emit a synthetic resolve ("target removed") and are pruned; `pending` rows are pruned silently. Deleting or renaming an alert target (mount, probe stream, heartbeat check) is an ordinary operation now, not a state leak.
- Alert events sharing the same second are returned in insertion order (`id` tiebreak) instead of arbitrary order.

## [0.9.2] - 2026-06-23

### Breaking

- A `server_secrets` table was folded into the initial migration (pre-1.0 policy — no migration chaining). Existing databases fail the migration checksum at boot: delete the database folder and re-pair devices after upgrading.

### Changed

- **JWT secret is now auto-managed.** A weak or unset `auth.jwt_secret` no longer aborts startup; the server generates a strong per-install secret on first boot and persists it in `server_secrets`. An explicit strong secret in config/env still takes precedence, so sharing or rotating a secret across instances is unchanged.

### Fixed

- Graceful shutdown is no longer triggered spuriously when the Ctrl+C / SIGTERM handler fails to install — the signal future stays pending on error instead of resolving immediately.
- Probe scheduler aborts in-flight tasks for probes removed, disabled, or failed on reload.
- SSE: the `journalctl` follower is killed when the client disconnects.
- Services: enable/disable on a timer targets the `.timer` unit, not the `.service`.
- Auth: refresh tokens are consumed single-use, closing the rotation race.

## [0.9.1] - 2026-06-10

### Added

- **`GET /logs`** — read API over the daemon's own log table (written since 0.7.4 but previously unreadable without opening the SQLite file). Filters: `start`/`end`, `level` (minimum severity), `limit` (default 500, max 5000); newest-first.
- **`GET /ready`** — readiness as opposed to `/health` liveness: performs a DB round-trip and returns 503 with `{"failed_check":"db"}` when the pool can't serve queries. Public, like `/health`. (Named `/ready` rather than k8s-style `/readyz` to stay symmetric with the established `/health`.)

### Removed

- The toy `GET /hello` and `GET /teapot` endpoints; use `GET /health` for connectivity checks. No known client called either.
- The Bruno API collection (`bruno/`). It duplicated the route table by hand and drifted; the authoritative reference is `src/routes/{rest,sse,ws}/mod.rs` plus the DTO modules, which is also what coding agents read.

### Fixed

- Docker WS exec now connects through the same socket-path-aware helper as every REST route, so `docker.socket_path` (Podman, custom sockets) applies to `/docker/containers/{id}/exec` too — previously it silently used the platform default socket.

### Changed

- `GET /health` returns JSON (`{"status":"ok"}`) instead of a plain-text greeting. Status-code-based checks (curl `--fail`, the peer-health example probe) are unaffected.
- DB log writer drains the channel in batches (up to 64 entries per transaction) instead of one commit per line, and stores the emission timestamp captured at the call site rather than the insert time.

## [0.9.0] - 2026-06-10

### Breaking

- `metrics_smart` was folded into the initial migration (pre-1.0 policy — no migration chaining). Existing databases fail the migration checksum at boot: delete the database folder and re-pair devices after upgrading.

### Added

- **SMART disk health** — built-in collector wrapping `smartctl --json` (the same approach Scrutiny/Netdata take; there is no viable pure-Rust SMART stack covering ATA + NVMe + USB bridges). Auto-detects the binary at boot and degrades gracefully when absent. Polls every 30 min by default (`[smart]` config: `enabled`, `smartctl_path`, `interval_secs`), uses `-n standby` so sleeping HDDs are never spun up. Readings (health verdict, temperature, power-on hours, ATA attrs 5/197/198/199, NVMe wear/spare/media-errors) land in the new `metrics_smart` table (raw-only, 1-year retention seed).
- **`GET /system/smart`** — latest reading per device plus an `available` flag distinguishing "no smartmontools" from "no readings yet".
- **`smart` alert namespace** — `smart.health_passed{device="/dev/sda"} < 1`, `smart.temperature_c > 60`, `smart.reallocated_sectors > 0` etc.; device label sourced from `/system/smart` in the alerts schema.

- **`GET /summary`** — one-call host overview aimed at multi-server clients: server name, hostname, OS, version, uptime, latest CPU/memory gauges, fullest-mount disk percentage, and pending/firing alert counts. A fleet view polls this once per daemon instead of fanning out to `/system/info` + `/metrics/*` + `/alerts/state`. Live-gauge fields are `null` until the first collector tick.
- `probes/examples/peer-health.sh` — sibling-daemon reachability probe. A daemon cannot report its own death; in a multi-server setup each daemon watches a peer's `/health` so a dead host still produces an alert (`probe`/`peer-health`/`up < 1`).

## [0.8.3] - 2026-06-05

### Added

- Alerting: `disk.used_percent` and `disk.total_bytes` fields, so a per-mount "% full" rule (e.g. `disk.used_percent{mount_point="/data"} > 90`) is now directly expressible. `used_percent` is computed as `used_bytes/total_bytes*100`.

### Changed

- Probe reload endpoint renamed `POST /admin/probes/reload` → `POST /probes/reload`, aligning it with the other `/probes/*` routes (the `/admin/` prefix implied a privilege tier that does not exist).

## [0.8.2] - 2026-06-02

### Security

- Web push: validate the subscription `endpoint` against the SSRF policy at subscribe and send time, blocking loopback / link-local / RFC1918 targets.
- ntfy: validate the `server` URL against the SSRF policy at create, boot, and send time, matching the webhook channel.
- Probes: clear supplementary groups and set the target gid before `setuid` when dropping privileges as root.

### Fixed

- Notifications: retry FCM / web-push per device instead of per channel, so a slow fan-out no longer re-notifies already-reached devices.
- Alerting: keep a keyed target whose latest sample is NULL by applying the `IS NOT NULL` filter inside the `MAX(timestamp)` subquery.
- Probes: drain stdout/stderr concurrently with `wait()` so a probe that writes past the pipe buffer no longer deadlocks into a timeout.
- Probes: truncate output on a UTF-8 char boundary to avoid a panic on multi-byte tails.

## [0.8.1] - 2026-05-26

### Removed

- Adaptive stats sampling. Collector now ticks at `collector_stats_interval_ms` unconditionally; the prior `base × 4` slowdown on idle caused chart-granularity drift on the overview without meaningful CPU/IO savings. `collector_stats_base_interval_ms` is gone from the `/config` response — use `collector_stats_interval_ms`.

## [0.8.0] - 2026-05-26

### Fixed

- Alerting: hot-reload rules on `PUT` without server restart; remove duplicate fire-body expression; prune stale `Ok` state rows on rule delete.
- Probes: restore `pid` variable used in unix process-group kill path.

### Changed

- Removed dead code, abandoned TOTP path, and unused accessors/methods.
- Clippy clean across all crates; trailing-newline / formatting pass.
- CI: release workflow now grants `contents: write` and attaches built binaries to the GitHub release.

## [0.7.8] - 2026-05-15

### Added

- **`GET /metrics/batch`** — fetch many time-series resources in a single request. `?resources=cpu,memory,disk,network,components,cpu_cores` (whitelist, max 8, no duplicates). Window via `?span=1h|30m|24h|7d|<seconds>` or the existing `?start=&end=` (mutually exclusive). Shared `?resolution=` and `?limit=` apply to every series; `cpu_cores` ignores resolution (no rollup). Per-resource reads run in parallel on the same SQLite pool. Aimed at mobile clients — collapses N dashboard requests into one round trip, one JWT header, one envelope. Pressure / probe metrics stay out of batch (sub-key shape doesn't fit a comma list).
- Response shape: `{ start, end, resolution, series: [{ resource, points }, ...] }`. `series` order mirrors the request. New `BatchSeries` enum is `#[serde(tag = "resource")]` so future resources stay additive.

## [0.7.7] - 2026-05-15

### Security

- **Webhook channel: default-deny private / loopback ranges.** Webhook URLs that resolve to `127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16` (incl. cloud metadata `169.254.169.254`), `0.0.0.0`, `255.255.255.255`, multicast/documentation v4 ranges, `::1`, `::`, `fe80::/10`, `fc00::/7`, multicast v6, or any `::ffff:<private-v4>` mapped address are now rejected at create-time (400 BAD_REQUEST) and at send-time (re-resolved each invocation as a DNS-rebinding defense). Closes the SSRF gap noted under v0.7.4 "Loopback / RFC1918 blocking is not enabled by default."
- Two override knobs under `[notifications.webhook]`: `allow_private_targets: bool = false` (master switch for dev / homelab), `allowed_private_hosts: Vec<String> = []` (production allow-list, case-insensitive exact hostname match).
- **Breaking default for upgraders**: an existing webhook channel that targets a private address will be skipped at boot with a single warn line (`Skipping webhook channel '<name>': … — see CONFIG.md`) and will refuse to send. To preserve the prior behavior set `allow_private_targets = true` in config, or migrate the trusted host to `allowed_private_hosts`.

### Changed

- `POST /notifications/channels` and `PUT /notifications/channels/{id}` now validate the webhook URL against the SSRF policy before persisting. Previously bad URLs were inserted silently and only skipped at reload — fixed.

## [0.7.6] - 2026-05-15

### Added

- **Alert silence** — temporarily suppress `Fired` notifications without disabling the rule. `POST /alerts/{id}/silence { "duration_secs": N }` mutes for N seconds; `DELETE /alerts/{id}/silence` lifts immediately. `PATCH /alerts/{id}` also accepts a `silenced_until` field (absolute timestamp; `null` to clear). State transitions and `alert_events` history continue while silenced — the suppressed fires are recorded with `notified: false`. `Resolved` notifications are never silenced. Channel-agnostic: the gate sits before fanout, so FCM and WebPush silence uniformly.
- `silenced_until: Option<i64>` field on the `AlertRuleDto` response (unix epoch seconds; `null` when not silenced).

### Changed

- `alert_rules` schema gained a nullable `silenced_until INTEGER` column (migration `0002_alert_silence.sql`).

## [0.7.4] - 2026-05-11

### Added

- **`[server] trusted_proxy` config** — when set, auth-endpoint rate limit and `devices.last_ip` key on `X-Forwarded-For` instead of the TCP peer

### Added

- **Per-route body size limits** — 8 KiB on the rate-limited anonymous auth subrouter (`/auth/pair/*`, `/auth/login`, `/auth/refresh`), 64 KiB globally
- **30s request timeout** on REST routes (SSE and WS exempted — long-lived streams)
- **`?offset=` on long-tail history endpoints** — `/alerts/events`, `/alerts/{id}/events`, `/probes/{name}/history` now support client-side pagination back through older rows (cap 100 000)
- **`?signal=` on `DELETE /processes/{pid}`** — `9` (SIGKILL) or `15` (SIGTERM, default); rejected otherwise. Ignored on Windows

### Changed

- **Single-facade logging** — `env_logger` removed; the project's `log::*!` and `tracing::*!` call sites now share one `tracing-subscriber` Registry. The `tracing-log` bridge keeps every existing `log::*!` site working unchanged. Stdout and the in-DB `logs` viewer publish the same events through a composed fmt layer + custom `DbLayer`.
- **Per-target log filter defaults** — third-party crates (`sqlx`, `hyper`, `h2`, `rustls`) are clamped to `warn` so enabling `debug` on the app code no longer floods stdout with raw SQL bodies. Operators override the whole filter via `RUST_LOG` env var.
- **ANSI escapes auto-disabled** for non-tty stdout — files, journald, and Loki ingest now receive plain text.
- **Unified timestamp format** — every log line uses RFC 3339 UTC with microsecond precision. Was previously a mix of local-time seconds (app logs) and UTC microseconds (request spans).
- **`logs` table no longer stores third-party noise** — `DbLayer` filters on the `remon_server::*` target prefix, including events bridged from `log::*!` (target is recovered via the `log.target` field). Stdout still receives everything that passes the global filter.
- `tick_timer` percentile flush demoted from `info!` to `debug!`; production logs no longer get a per-collector latency line every minute.
- Removed the duplicate `Probe loader: ...` boot line emitted from `main.rs` — the scheduler already logs it.
- Pairing code no longer logged; terminal display simplified
- FCM error responses sanitized before surfacing; raw body kept at `debug` only
- Stats / processes collector intervals share a single tick timestamp so every metric in one frame writes the same key
- Linux-only metric enrichment extracted into a dedicated `enrich_linux` async fn
- Collector loops now drive off `tokio::time::interval` with `MissedTickBehavior::Skip` instead of `sleep`-after-work; cadence no longer drifts when one tick's work overruns the period. Rollup, retention, and alert evaluator loops use the same pattern
- Notification fanout retries each channel once on transient failure (5 s timeout per attempt, 500 ms backoff). Drops fewer alerts on TCP resets, DNS blips, and upstream 503s
- `WebPushChannel` now reuses the `NotificationManager`'s shared HTTP client (connection pool + 15 s timeout) instead of building its own bare `Client::new()`
- `CompressionLayer` excludes `text/event-stream` — gzip was buffering SSE frames until a compression window filled, breaking live updates
- `Authorization: Bearer …` parsing is now case-insensitive on the scheme (RFC 7235)
- `PATCH /config` rejects `server_name` longer than 128 characters

### Fixed

- Auth-endpoint rate limit collapsed all callers into one bucket when behind a reverse proxy
- `PATCH /config` and the collector floor now reject sub-second sampling intervals — second-resolution timestamps would otherwise collide on the `metrics_*` PK and silently drop earlier rows
- `read_inode_usage` (statvfs per mount) runs inside `spawn_blocking` with a 2s timeout so a hung network/fuse mount can't stall the collector
- Metric inserts switched from `INSERT OR REPLACE` to `INSERT ... ON CONFLICT DO NOTHING` and emit a warn-level log on collision; a same-timestamp row written twice is now visible instead of silently overwritten
- Rollup loop no longer advances the cursor past a failed bucket — on error it stops at the last contiguous-successful bucket and retries the failure on the next tick. Previously a transient DB error on bucket N would leave N permanently un-rolled
- Retention task warns when a `retention_policy` row references an unknown resource instead of silently no-op'ing
- Alert evaluator now persists the state row BEFORE emitting notify/event side effects. A transient DB error on the state upsert previously caused the next tick to re-see Pending state and re-fire, producing a notification storm for as long as the DB hiccup lasted
- Webhook channel rejects URLs that don't start with `http://` or `https://` — cheap SSRF defense against `file://` / `gopher://` etc. Loopback / RFC1918 blocking is not enabled by default

### Removed

- Empty `collectors/docker` stub — container stats remain pull-only via `/docker/containers/*` and `/sse/docker/*` (the `collector_docker_interval_ms` runtime field is kept for a future implementation)

### Build

- `tokio` features narrowed from `"full"` to the explicit set in use
- `chrono` switched to `default-features = false`
- Removed unused deps: `fast_qr`, `base32`, `local-ip-address`, `maplit`, `strum`, `strum_macros`, `hostname`, `env_logger`
- Added `tracing-log` feature to `tracing-subscriber` to bridge `log::*!` macro call sites into the new single subscriber

## [0.7.3] - 2026-05-11

### Added

- **Web Push notifications** — VAPID + RFC 8291 payload encryption, in-house implementation on `ring` (no OpenSSL)
- **Network totals** — `total_rx_bytes` / `total_tx_bytes` per interface
- **`X-Forwarded-For` on `POST /auth/login`** — first entry stored as `devices.last_ip`
- **`parent_pid` on `ProcessInfo`**
- **Example probes** — `ssl-cert-expiry.sh`, `disk-free-root.sh`

### Changed

- `sysinfo` bumped 0.30 → 0.39.1

## [0.5.0] - 2026-05

### Added

- **Platform abstraction** — `ServiceManager` async trait with backends for systemd, OpenRC, Windows SCM, and an unsupported fallback
- **Service endpoints** — `GET/POST /services`, `GET /services/{name}`, start/stop/restart/reload, enable/disable
- **Systemd timers** — `GET /timers`, enable/disable; returns 501 on non-systemd platforms
- **Cron listing** — `GET /cron`; reads system and user crontab files, expands shorthands
- **Service log streaming** — `GET /sse/services/{name}/logs` (Linux only, journalctl-backed)
- **Service watcher** — background poller that sends FCM notifications on unit failure
- **`GET /system/info`** — hostname, OS, kernel, uptime, CPU model, memory, disks, network interfaces
- **Alert engine v2** — expression-based rules with `pending/firing/ok` lifecycle, configurable for-duration and cooldown
- **Alert state & events** — `GET /alerts/state`, `GET /alerts/events`, per-rule event history
- **Notification channels** — CRUD + test for FCM, Telegram, ntfy, webhook (`/notify/channels`)
- **DB indexes** — `alert_state(state)`, `devices(is_active, fcm_token)`
- **Per-collector telemetry** — p50/p95/p99/max timing logged once per minute

### Changed

- Stats collector uses targeted refreshes instead of `refresh_all()`; processes served from a cached snapshot
- DB writes use batch INSERTs for multi-row tables
- Process kill goes through `libc::kill` / `OpenProcess+TerminateProcess` directly instead of sysinfo
- `SystemdManager::get` uses targeted `systemctl show` instead of list-and-find

### Removed

- gRPC (tonic, prost, tonic-prost-build)

### Security

- Service name validated against `[A-Za-z0-9._@:-]` at handler level
- `DELETE /processes/{pid}` rejects `pid == 0`

### Build

- rand 0.10, reqwest 0.13, axum 0.8.9, ring 0.17, windows-sys 0.61

## [0.4.0] - 2026-04-28

### Added

- **Time-series rollup** — configurable resolutions (raw/1m/5m/1h), background worker, resume cursor
- **Retention policies** — per `(resource, resolution)` keep window, hourly cleanup
- **Adaptive sampling** — collector slows down when no SSE/WS subscribers are active
- **Metrics endpoints** — `/metrics/cpu`, `/metrics/memory`, `/metrics/disk`, `/metrics/network` with time range and resolution params
- **Alert rules** — threshold-based CRUD (`GET/POST/PUT/DELETE /alerts`), FCM fan-out, cooldown gate
- **`PATCH /me/fcm-token`** — register or clear FCM push token
- **`POST /auth/logout`** — revoke calling session
- **Runtime config** — `GET/PATCH /config`; changes apply without restart
- **CORS layer** — `allow_any_origin` for dev, explicit allow-list for prod
- **Hosts table** — multi-host ready; local machine seeded at boot
- **Auth rate limiting** — burst 5 / ~1 per 12s on pairing and login endpoints

### Changed

- Migrations moved to versioned `migrations/*.sql` files with `sqlx::migrate!`
- SQLite opened with WAL mode, NORMAL sync, 5s busy timeout, foreign keys on
- Auth middleware checks `jti` against `sessions` table on every request
- Refresh rotation invalidates all device sessions on use
- Pairing code 6 → 8 digits, constant-time comparison, 3-attempt cap

### Removed

- OTP authentication (TOTP QR, `/auth/login/otp`)

### Security

- JTI revocation for access and refresh tokens
- Strict JWT validation (HS256 only, required spec claims)
- Container inspect response whitelisted — env vars, mounts, host config no longer exposed

## [0.3.0] - 2026-01-22

### Added

- WebSocket Docker exec — `WS /ws/docker/containers/{id}/exec`

### Changed

- Routes reorganized into `routes/rest/`, `routes/sse/`, `routes/ws/`
- Endpoints renamed to RESTful resource-based conventions
- SSE endpoints moved to `/sse/` namespace

## [0.2.3] - 2026-01-11

### Added

- Memory cached/buffers fields in `/get-system-info` (Linux: `/proc/meminfo`, Windows: 0)

## [0.2.2] - 2026-01-07

### Added

- Docker log time filtering (`start_time`, `end_time`)
- SSE log streaming — `GET /docker/containers/{id}/logs/stream`
- Container stats endpoint — CPU, memory, network I/O, block I/O
- Podman support via socket path config
- `GET /get-system-info` — uptime, load average, memory, swap, network, process counts
- `GET /get-network-status` — per-interface rx/tx history

### Changed

- Container inspect merged into `GET /docker/containers/{id}`

## [0.2.1] - 2026-01-05

### Added

- Docker container monitoring and log retrieval

## [0.2.0] - 2026-01-04

### Added

- Structured HTTP request logging via `tracing`
- Hierarchical config with `config-rs` (`config/default.toml`, environment overrides)
- JWT secret strength validation at startup
- Bruno API collection

### Changed

- Migrated from Hyper 0.14 to Axum 0.8
- Database connection pooling with `tokio::sync::OnceCell`

### Removed

- `dotenv`, `lazy_static`, `async_once`

### Security

- Production startup fails on weak JWT secret

## [0.1.2]

### Added

- `DELETE /processes/{pid}` — kill process by PID

## [0.1.1]

### Added

- gRPC support with proto file and build.rs
- Versioning and CHANGELOG

## [0.1.0]

### Added

- Initial release
