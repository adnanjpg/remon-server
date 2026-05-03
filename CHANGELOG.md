# Changelog

All notable changes to this project will be documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
