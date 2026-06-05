# Changelog

All notable changes to this project will be documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
