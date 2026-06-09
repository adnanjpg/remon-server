# Configuration

remon-server uses layered config: `config/default.toml` → `config/<RUN_ENV>.toml` → `REMON__*` env vars.

Set `RUN_ENV=production` to load `config/production.toml` (optional file, create as needed).

## Key fields

### `[server]`
- `port` — HTTP listen port (default: 8080)
- `host` — Bind address (default: `"0.0.0.0"`, use `"127.0.0.1"` to restrict to loopback)
- `trusted_proxy` — set `true` only when behind a reverse proxy that controls `X-Forwarded-For` (Caddy/nginx with the standard forwarded-for directive). When `true`, per-IP rate limiting and the `devices.last_ip` audit field read from `X-Forwarded-For` / `X-Real-IP`; when `false`, they use the TCP peer. Leaving this `false` while behind a proxy works but collapses every client into the proxy's IP — the auth-endpoint rate limit then applies globally instead of per-client. Setting it `true` while exposed directly lets any caller spoof the header.

### `[database]`
- `path` — SQLite file path
- `folder_path` — created at boot if missing
- `max_connections` — pool size (1 is correct for SQLite WAL)

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
- `interval_secs` — poll interval (default: 1800; floor 60). Each poll issues real commands to every disk; `-n standby` keeps sleeping HDDs asleep, so a standby disk simply skips ticks until it wakes.

Note: `smartctl` needs root/Administrator to reach the devices — the same privilege level the service/process endpoints already require.

### `[cors]`
- `allow_any_origin` — `true` in dev, `false` in production
- `allowed_origins` — required when `allow_any_origin = false`, e.g. `["https://app.example.com"]`

## Docker feature flag

Build without Docker support:
```sh
cargo build --no-default-features
```

This disables all `/docker/*` endpoints and removes the bollard dependency.

## Production example

```toml
# config/production.toml
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
