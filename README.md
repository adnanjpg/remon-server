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
- **Host-event timeline** — `GET /events` unions system events (boot/powercycle detection, OOM kills, SMART health transitions), alert fire/resolve, incident captures, and an operator audit trail (who restarted what, from which device) into one stream, ready for chart annotations
- **Notification channels** — FCM, Telegram, ntfy, webhook; managed via REST API
- **Custom probes** — shell scripts with inline YAML header; drop into `probes/`, hot-reload via `POST /probes/reload`
- **Heartbeat checks** — push-model dead-man's switches for cron jobs and external services: `curl` a capability URL on schedule, alert when it goes quiet (`heartbeat.up < 1`); pause windows for planned downtime, service-announced via the same URL
- **Device pairing** — 8-digit code, Argon2-hashed token, JWT access+refresh with JTI revocation

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/adnanjpg/remon-server/dev/packaging/install.sh | sudo sh
```

Resolves the build for your machine, verifies it against the published
checksums, and — where systemd is running — leaves an enabled service behind.
Re-run it to upgrade; configuration and the database are never touched.

Linux amd64 and arm64 are statically linked, so there is no glibc floor and no
runtime dependency to install. Windows builds are published as a zip.

| | |
|---|---|
| binary | `/usr/local/bin/remon-server` |
| config | `/etc/remon/config.toml` |
| data | `/var/lib/remon` |
| logs | `journalctl -fu remon-server` |
| remove | `curl -fsSL .../packaging/uninstall.sh \| sudo sh` |

Prefer to place it yourself? Grab the tarball from
[Releases](https://github.com/adnanjpg/remon-server/releases) — the binary
carries its own defaults, so it runs with no config file at all:

```sh
./remon-server                       # uses built-in defaults
./remon-server doctor                # paths, config, port, host tooling
./remon-server --help
```

## Running from source

```sh
cargo run              # development
cargo run --release
```

A checkout keeps everything relative to it (`config/`, `db/`, `probes/`), so
this is unchanged by the install layout above. Set `RUN_ENV=production` to
load `config/production.toml` (create as needed).

## Configuration

See [CONFIG.md](CONFIG.md) for all options. Every file is optional — defaults
are compiled into the binary. Layers, applied in order:

1. built-in defaults (the contents of `config/default.toml` at build time)
2. `<config-dir>/default.toml`
3. `<config-dir>/config.toml` — the file an installed server edits
4. `<config-dir>/<RUN_ENV>.toml`
5. `REMON__*` environment variables

`<config-dir>` is `./config` in a checkout, `/etc/remon` when installed, or
whatever `--config-dir` / `REMON_CONFIG_DIR` says.

Key values to set in production:
```toml
# /etc/remon/config.toml
# [auth] jwt_secret is optional — auto-generated and persisted on first boot.
# Set it only to share or rotate the secret across instances.

[logging]
format = "json"

[cors]
# Browser clients only; native apps use bearer tokens and ignore this.
allow_any_origin = false
allowed_origins = ["https://your-frontend.com"]
```

## Diagnostics

```sh
remon-server doctor         # resolved paths, config, port, privileges, tooling
remon-server config check   # validate and print the effective layout
```

`doctor` never opens the database or mutates anything, so it is safe to run
against a live install.

## FCM Push Notifications

1. Create a Firebase project and download the service account JSON
2. Set the path in config or env:
   ```
   REMON__NOTIFICATIONS__FCM__SERVICE_ACCOUNT_PATH=/path/to/service-account.json
   ```
3. Register device FCM tokens via `PATCH /me/fcm-token` after pairing

## Custom Probes

Drop a shell script into the probes directory (`./probes` in a checkout,
`/etc/remon/probes` when installed) with an inline header:

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
