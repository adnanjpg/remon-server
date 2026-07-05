# remon-server

Server component of Remon — a self-hosted system monitoring platform. Exposes a REST/SSE/WebSocket API consumed by the web UI and mobile clients.

> Early development. API may change between versions.

## Features

- **System metrics** — CPU, memory, disk, network, pressure, hardware components; time-series with configurable rollup (raw / 1m / 5m / 1h) and retention
- **SMART disk health** — via `smartctl` (auto-detected, optional); per-device health verdict, temperature, wear and error counters; alertable (`smart.health_passed < 1`)
- **Processes** — list and kill
- **Services** — systemd (full), OpenRC (full), Windows SCM (full); timers, cron listing, live log streaming
- **Docker / Podman** — container lifecycle, logs, stats, exec over WebSocket; optional at compile time (`--no-default-features`)
- **Alert engine** — expression-based rules (`cpu.usage_percent > 80`), pending/firing/ok lifecycle, configurable for-duration and cooldown
- **Notification channels** — FCM, Telegram, ntfy, webhook; managed via REST API
- **Custom probes** — shell scripts with inline YAML header; drop into `probes/`, hot-reload via `POST /probes/reload`
- **Heartbeat checks** — push-model dead-man's switches for cron jobs and external services: `curl` a capability URL on schedule, alert when it goes quiet (`heartbeat.up < 1`); pause windows for planned downtime, service-announced via the same URL
- **Device pairing** — 8-digit code, Argon2-hashed token, JWT access+refresh with JTI revocation

## Requirements

Rust toolchain (stable).

## Quickstart

```sh
# 1. Copy env file
cp .env.example .env

# 2. (Optional) Left unset, the server generates and persists a JWT secret on
#    first boot. Override only to share/rotate one across instances:
#    REMON__AUTH__JWT_SECRET="your-secret-here"

# 3. Run
cargo run

# Production build
cargo run --release
```

Set `RUN_ENV=production` to load `config/production.toml` (create as needed).

## Configuration

See [CONFIG.md](CONFIG.md) for all options. The layered system:
1. `config/default.toml` — base defaults
2. `config/<RUN_ENV>.toml` — environment overrides
3. `REMON__*` environment variables — runtime overrides

Key values to set in production:
```toml
# config/production.toml
# [auth] jwt_secret is optional — auto-generated and persisted on first boot.
# Set it only to share or rotate the secret across instances.

[logging]
format = "json"

[cors]
allow_any_origin = false
allowed_origins = ["https://your-frontend.com"]
```

## FCM Push Notifications

1. Create a Firebase project and download the service account JSON
2. Set the path in config or env:
   ```
   REMON__NOTIFICATIONS__FCM__SERVICE_ACCOUNT_PATH=/path/to/service-account.json
   ```
3. Register device FCM tokens via `PATCH /me/fcm-token` after pairing

## Custom Probes

Drop a shell script into `probes/` with an inline header:

```sh
# @probe name=my-check
# @probe interval=1m
# @probe platforms=linux

# Output one JSON line:
echo '{"message":"ok","metrics":[{"name":"value","value":42}]}'
```

See `probes/examples/` for a full example. Reload without restart:
```sh
curl -X POST http://localhost:8080/probes/reload \
  -H "Authorization: Bearer $TOKEN"
```

## Heartbeat Checks

The inverse of a probe: instead of the server running a script, an external
job proves it is alive by pinging a capability URL. Miss the deadline
(`period + grace`) and the check reads `down`; the first ping brings it back.

```sh
# Create a check (the slug is shown ONCE — store it in the job's env)
curl -X POST http://localhost:8080/heartbeats \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"name":"db-backup","period_secs":86400,"grace_secs":3600}'

# From the monitored job — no token needed, the slug is the credential:
curl https://remon.example.com/ping/<slug>          # I'm alive
curl https://remon.example.com/ping/<slug>/$?       # report exit code
curl -X POST .../ping/<slug>/fail --data 'trace'    # explicit failure

# Planned downtime, announced by the service itself (capped at 24h):
curl -X POST '.../ping/<slug>/pause?duration=3h&reason=deploy'
curl -X POST .../ping/<slug>/resume                 # done early
```

Alerting goes through the normal rule engine — one unfiltered rule covers
every check, present and future: `heartbeat.up < 1` (crit). Add
`heartbeat.late == 1` (warn) to hear about the grace window before the page.
Operator pauses (`POST /heartbeats/{id}/pause`, indefinite allowed) always
override service-announced ones. When a pause expires the check gets one
fresh `period + grace` before it can go down — maintenance ending is not an
instant page.

## Build without Docker

```sh
cargo build --release --no-default-features
```

Removes all `/docker/*` endpoints and the bollard dependency.

## API Reference

The route table in `src/routes/{rest,sse,ws}/mod.rs` is the authoritative endpoint list; request/response shapes live in `src/routes/dtos/` and behaviour notes in the handler doc comments. Bootstrap flow: `POST /auth/pair/initiate` → read the 8-digit code from the server terminal → `POST /auth/pair/complete` → `POST /auth/login`.
