# Changelog

All notable changes to this project will be documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
