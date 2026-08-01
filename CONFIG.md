# Configuration

Every configuration file is optional. The defaults are compiled into the
binary, so `./remon-server` in an empty directory is a valid, fully configured
server. Files layer on top, each overriding the last:

1. built-in defaults — the contents of `config/default.toml` as of the build
2. `<config-dir>/default.toml`
3. `<config-dir>/config.toml` — the file an installed server is meant to edit
4. `<config-dir>/<RUN_ENV>.toml` — `RUN_ENV` defaults to `development`
5. `REMON__<SECTION>__<KEY>` environment variables

## Paths

Resolved once at startup, in this order:

1. `--config-dir` / `--data-dir`, or `REMON_CONFIG_DIR` / `REMON_DATA_DIR`
2. a working directory containing `config/default.toml` — a repo checkout, so
   paths stay relative to it (`./config`, `./db`, `./probes`)
3. otherwise `/etc/remon` and `/var/lib/remon` when running as root, the
   per-user XDG directories when not, `%ProgramData%\remon` on Windows

Probes live in `<config-dir>/probes`, or `./probes` in a checkout;
`REMON_PROBES_DIR` overrides it independently.

Paths inside the config are resolved against the data directory when relative
and honoured as-is when absolute, so `database.path` can point at another
volume without moving anything else.

Run `remon-server config check` to print what a given invocation resolved to,
or `remon-server doctor` for that plus port availability, writability,
privileges and host tooling.

## Key fields

### `[server]`
- `port` — HTTP listen port (default: 8080)
- `host` — Bind address (default: `"0.0.0.0"`, use `"127.0.0.1"` to restrict to loopback)
- `trusted_proxy` — set `true` only when behind a reverse proxy that controls `X-Forwarded-For` (Caddy/nginx with the standard forwarded-for directive). When `true`, per-IP rate limiting and the `devices.last_ip` audit field read from `X-Forwarded-For` / `X-Real-IP`; when `false`, they use the TCP peer. Leaving this `false` while behind a proxy works but collapses every client into the proxy's IP — the auth-endpoint rate limit then applies globally instead of per-client. Setting it `true` while exposed directly lets any caller spoof the header.

### `[database]`
- `path` — SQLite file path. Relative paths resolve against the data directory; absolute ones are used as given.
- `folder_path` — created at boot if missing, resolved the same way
- `max_connections` — pool size (default: 5). WAL serialises writes but reads run concurrently, so keep this above 1 — with a single connection every short query queues behind long metrics-history scans.

### `[auth]`
- `jwt_secret` — **change in production** (min 32 chars enforced)
- `access_token_ttl_secs` — default 3600 (1 hour)
- `refresh_token_ttl_secs` — default 2592000 (30 days)
- `pairing_code_ttl_secs` — default 300 (5 minutes)

### `[monitoring]`
- `log_insertion_level` — minimum log level persisted to DB: `error | warn | info | debug | trace`
- `app_name` — label used in persisted log entries

### `[logging]`
- `level` — `trace | debug | info | warn | error`
- `format` — `compact` (default) | `pretty` | `json`

### `[notifications.*]`
Server-side credentials. Channel targets (chat_id, topic, URL) are managed via `POST /notifications/channels`.
- `[notifications.fcm]` — `service_account_path`
- `[notifications.telegram]` — `bot_token`
- `[notifications.ntfy]` — `token` (optional, for auth'd servers)
- `[notifications.webhook]` — `secret` (optional, sent as `Authorization: Bearer`); SSRF policy: `allow_private_targets` (default `false` — webhook URLs that resolve to loopback / RFC1918 / link-local / ULA ranges are rejected at create-time and send-time), `allowed_private_hosts` (default `[]` — case-insensitive hostname exceptions, no CIDR). Threat model: an operator account with channel-CRUD permission could otherwise use the server's network position to probe internal services or exfiltrate cloud metadata (e.g. `169.254.169.254`). For dev/homelab convenience set `allow_private_targets = true`; for production prefer the per-host allow-list. Multi-entry allow-list is best set via TOML (the env var override accepts a single value).

### `[docker]`
- `socket_path` — custom socket (empty = use `DOCKER_HOST` env or platform default). Useful for Podman: `/run/podman/podman.sock`
- `exec_enabled` — master kill-switch for `WS /docker/.../exec` (default: false; opt in explicitly to allow container exec)

### `[smart]`
SMART disk health, collected by shelling out to `smartctl` (smartmontools). When the binary is missing the collector logs one info line at boot and turns itself off; `GET /system/smart` then reports `available: false`. Readings land in `metrics_smart` (raw-only, 1-year retention) and are alertable via the `smart` namespace, e.g. `smart.health_passed{device="/dev/sda"} < 1` or `smart.temperature_c > 60`.
- `enabled` — master switch (default: true; absence of smartctl already degrades gracefully)
- `smartctl_path` — explicit binary path (default: empty = resolve `smartctl` from `PATH`)

The poll interval is runtime config, not TOML: `PATCH /config { collector_smart_interval_ms }` (default 1 800 000 = 30 min, floor 60 000). Each poll issues real commands to every disk; `-n standby` keeps sleeping HDDs asleep, so a standby disk simply skips ticks until it wakes.

Note: `smartctl` needs root/Administrator to reach the devices — the same privilege level the service/process endpoints already require.

### `[control]`
Guard rails on the endpoints that change the host rather than report on it.
- `own_service` — the service unit supervising this process. Auto-detected from `RC_SVCNAME` (OpenRC) or the cgroup path (systemd); set it explicitly only when neither is available.

Knowing its own identity lets the server refuse requests that name it. `DELETE /processes/{pid}` on its own pid and `POST /services/{name}/stop|restart|disable` on its own unit return **409** rather than switching monitoring off — the caller is working from a process or unit list and almost never means the agent. `start`, `enable` and `reload` are unaffected, since none of them can end the process.

This is accident prevention, not a security boundary: any paired device already holds full control of the host.

The deliberate paths are:
- `POST /system/restart` — ends the process so the supervisor starts a fresh one. How to apply boot-time configuration (listen port, log format, CORS origins) without shell access. Exits **75** rather than 0, because a Windows scheduled task only restarts an action that failed.
- `POST /system/shutdown` — stops monitoring this host. Every supervisor is configured to restart the agent however it went down, so this asks the supervisor to stop the unit; starting it again needs access to the host. Where no unit can be identified — a bare binary, or a container where neither `RC_SVCNAME` nor the cgroup path names one — it can only exit, and whether it returns depends on how it was started. The reply says which of the two happened.

Both answer `202` before acting, drain in-flight requests and SSE streams, and write the clean-shutdown marker so the next boot does not report a crash.

### `[assistant]`
An OpenAI-compatible chat endpoint (`POST /assistant`, `/assistant/stream`) that answers questions about this host by calling read-only tools against its own data. Inert until `api_key` is set — `enabled` defaults to true, so the key is the real switch. Put the key in the environment rather than the file: `REMON__ASSISTANT__API_KEY`.
- `enabled` — master switch (default: true, but without a key nothing runs)
- `api_key` — bearer key for the provider (default: empty = assistant off)
- `base_url` — provider endpoint; `/chat/completions` is appended (default: `https://generativelanguage.googleapis.com/v1beta/openai`)
- `model` — model id passed straight through (default: `gemini-2.5-flash`)
- `max_tokens` — response ceiling per model turn (default: 2048)
- `prometheus_url` — a Prometheus to answer PromQL against, e.g. `http://localhost:9090` (default: empty = the `prometheus_query` tool is not offered). Read-only: only the instant and range query APIs are called.
- `dev` — allow per-ask overrides (system prompt, step and token limits, loop trace) for working on the assistant itself (default: false; auth and the read-only tool contract apply either way)

Two keys here are not about the assistant alone, because the collector they drive also feeds alert rules:
- `process_history` — run the continuous process collector, so process answers carry a short history (avg/max over ~15 min) instead of a bare snapshot (default: true). Costs one full process refresh per tick; worth turning off on hosts with very large process tables.
- `process_series_top_k` — once a minute, persist the top-K process groups by cpu and by memory (union) into `metrics_process` (default: 20; `0` disables). This is what opens the `process` namespace to alert rules and history queries, e.g. `process.cpu_percent{name="clickhouse-server"} > 80`. Bounded by K, not by the host's process table. Requires `process_history`.

### `[cors]`
CORS is a browser mechanism. Native clients (mobile app, curl) authenticate with bearer tokens and are unaffected by anything here.
- `allow_any_origin` — `true` in dev, `false` in production
- `allowed_origins` — e.g. `["https://app.example.com"]`

The shipped default (`allow_any_origin = false`, empty list) is the tightest policy: no browser origin can reach the API. The server boots and logs a warning rather than refusing to start, since a fresh install has no frontend origin to name yet.

Note that a web UI served over HTTPS cannot call a server over plain HTTP — browsers block the mixed content outright, with no override. Either terminate TLS in front of the server, or serve the UI from the same origin.

## Runtime config (DB-backed, no restart)

Everything above is boot-time TOML. A separate set of knobs lives in the database, applies live, and is meant to be driven from the web UI:

- `GET/PATCH /config` — `server_name` (used in notification titles and `/summary`), collector intervals (stats / processes / docker / smart), rollup + retention tick intervals.
- `GET/PATCH /config/retention` — per-(resource, resolution) keep windows for the metrics pruner.
- `GET /config/resolutions`, `PATCH /config/resolutions/{name}` — enable/disable rollup buckets (`raw` and mid-chain parents are guarded).

## Docker feature flag

Build without Docker support:
```sh
cargo build --no-default-features
```

This disables all `/docker/*` endpoints and removes the bollard dependency.

## Production example

```toml
# /etc/remon/config.toml
[server]
host = "0.0.0.0"
trusted_proxy = true   # only if behind Caddy/nginx; see [server] above

[logging]
level = "info"
format = "json"

[cors]
allow_any_origin = false
allowed_origins = ["https://app.example.com"]
```

Or via env vars:
```sh
REMON__AUTH__JWT_SECRET="$(cat /run/secrets/jwt)" \
REMON__CORS__ALLOW_ANY_ORIGIN=false \
REMON__CORS__ALLOWED_ORIGINS="https://app.example.com" \
RUN_ENV=production \
./remon-server
```
