## Bruno Collection — remon-server API

Mirrors the routes in `src/routes/{rest,sse,ws}/mod.rs`. Keep them in sync.

### Layout

```
bruno/
├── environments/local.bru
├── public/          health, hello, teapot
├── auth/            pair → login → refresh → logout
├── me/              fcm-token
├── system/          GET /system/info
├── process/         list, kill
├── services/        list, get, start/stop/restart/reload, enable/disable
├── timers/          list, enable/disable  (systemd only)
├── cron/            list
├── docker/          status, containers, images, ws-exec
├── metrics/         cpu, cpu/cores, memory, disk, network, components, pressure
├── alerts/          rules CRUD, active state, events
├── probes/          list, get, history, reload
└── sse/             stats (unified + per-resource), docker container logs
```

`/config` and `/notify/channels` don't have Bruno files yet — hit them directly with the auth token.

### Bootstrap

1. `auth/Pair Initiate` — starts pairing. The 8-digit code prints to the **server terminal only**.
2. Paste the code into `pairing_code` env var.
3. `auth/Pair Complete` — returns `device_id` + `device_token`; auto-saved to env. Save `device_token` securely.
4. `auth/Login` — returns `access_token` + `refresh_token`; auto-saved (`jwt_token` = access token).
5. `auth/Refresh Token` — rotates both tokens. **Wipes all sessions for the device** — old tokens stop working immediately.

### Endpoint groups

#### `auth/`
- `Pair Initiate` — 409 if a window is still active (5 min TTL)
- `Pair Complete` — 3 wrong attempts invalidate the window; constant-time comparison
- `Login`, `Refresh Token`, `Logout` (revokes calling jti only)

#### `metrics/`
Common query params: `start`, `end` (unix seconds), `resolution` (`raw|1m|5m|1h`), `limit` (default 1000, max 5000).
Auto-resolution: ≤2h → raw, ≤1d → 1m, ≤7d → 5m, else 1h.

#### `alerts/`
Expression-based rules. Evaluator picks up changes at the next tick.
- Rules: `List`, `Get`, `Create`, `Update`, `Delete`
- `active-state` — current `pending/firing` states
- `events` / `events-for-rule` — audit log

#### `services/`
Proxies to the detected init system (systemd / OpenRC / Windows SCM).
Returns 501 for unsupported operations (e.g. `reload` on Windows SCM).

#### `timers/`
Systemd timer listing and enable/disable. Returns 501 on non-systemd platforms.

#### `probes/`
Custom probe definitions. `reload-probes` re-reads manifests from disk without restart.

#### `docker/`
Full Docker surface — status, containers (start/stop/restart/pause/unpause/delete/prune/inspect/logs/stats), images, ws-exec.
Inspect response is whitelisted — env vars, mounts, host config and network IPs are not exposed.

#### `sse/`
For sustained streams use curl — Bruno's renderer is limited.

### Environment variables

| Variable | Purpose |
|---|---|
| `baseUrl` | e.g. `http://localhost:8080` |
| `device_name`, `device_id`, `device_token` | pairing |
| `pairing_code` | filled after reading server terminal |
| `jwt_token` *(secret)*, `refresh_token` *(secret)* | auth |
| `fcm_token` | optional, set at pair-complete or via PATCH /me/fcm-token |
| `container_id`, `image_id` | docker requests |
| `service_name`, `timer_name` | services / timers |
| `probe_name` | probes |
| `pid` | process kill |
| `alert_id` | alert CRUD |
| `metrics_start`, `metrics_end`, `metrics_resolution`, `metrics_limit` | metrics queries |
