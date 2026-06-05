# remon-server

Server component of Remon — a self-hosted system monitoring platform. Exposes a REST/SSE/WebSocket API consumed by the web UI and mobile clients.

> Early development. API may change between versions.

## Features

- **System metrics** — CPU, memory, disk, network, pressure, hardware components; time-series with configurable rollup (raw / 1m / 5m / 1h) and retention
- **Processes** — list and kill
- **Services** — systemd (full), OpenRC (full), Windows SCM (full); timers, cron listing, live log streaming
- **Docker / Podman** — container lifecycle, logs, stats, exec over WebSocket; optional at compile time (`--no-default-features`)
- **Alert engine** — expression-based rules (`cpu.usage_percent > 80`), pending/firing/ok lifecycle, configurable for-duration and cooldown
- **Notification channels** — FCM, Telegram, ntfy, webhook; managed via REST API
- **Custom probes** — shell scripts with inline YAML header; drop into `probes/`, hot-reload via `POST /probes/reload`
- **Device pairing** — 8-digit code, Argon2-hashed token, JWT access+refresh with JTI revocation

## Requirements

Rust toolchain (stable).

## Quickstart

```sh
# 1. Copy env file
cp .env.example .env

# 2. Edit config/default.toml — set a strong jwt_secret for production,
#    or override via environment variable:
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
[auth]
jwt_secret = "change-me"   # or via REMON__AUTH__JWT_SECRET

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

## Build without Docker

```sh
cargo build --release --no-default-features
```

Removes all `/docker/*` endpoints and the bollard dependency.

## API Reference

See the Bruno collection in `bruno/` for a complete, runnable API reference. Bootstrap flow: `auth/Pair Initiate` → read code from server terminal → `auth/Pair Complete` → `auth/Login`.
