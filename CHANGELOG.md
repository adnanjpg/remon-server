# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),

## [0.3.0] - 2026-01-22

### Added

- **WebSocket Docker Exec**: Interactive container execution via WebSocket
  - New endpoint: `WS /ws/docker/containers/{id}/exec`
  - Bidirectional real-time communication between client and container
  - Support for custom commands, PTY allocation, and stdin/stdout/stderr streams
  - Graceful error handling and disconnect management

### Changed

- **API Restructuring**: Complete reorganization into protocol-based architecture
  - Modular route system: `routes/rest/`, `routes/sse/`, `routes/ws/`
  - Removed monolithic `api/handlers/` directory
  - Reduced `main.rs` from 309 to 202 lines (-35%)
- **Endpoint Standardization**: RESTful naming and HTTP method corrections
  - Monitor: `/get-cpu-status` → `/monitor/cpu` (and 7 more endpoints)
  - Process: `GET /kill-process?pid=X` → `DELETE /processes/{pid}`
  - Auth: `/get-otp-qr` → `/auth/otp/qr`, `/login` → `/auth/login`
  - Logs: `/logs/get-app-ids` → `/logs/apps`, `/logs/get-app-logs` → `/logs`
  - Misc: `/healthcheck` → `/health`
  - All endpoints now follow resource-based, pluralized naming conventions
- **SSE Separation**: Server-Sent Events endpoints moved to dedicated namespace
  - `/docker/containers/{id}/logs/stream` → `/sse/docker/containers/{id}/logs/stream`
- **Bruno Collection**: Updated all request files
  - URLs updated to new RESTful endpoints
  - File names updated to match endpoint structure
  - Changed `kill-process` from GET to DELETE method

### Removed

- **Deprecated Endpoints**: Removed all old-style verb-based/rpc-like endpoints
- **Old Handler Module**: Removed `src/api/handlers/` directory

## [0.2.3] - 2026-01-11

### Added

- **Memory Cached/Buffers**: Enhanced memory monitoring with detailed cache information
  - Added `cached` field to memory info (file system cache)
  - Added `buffers` field to memory info (kernel buffers)
  - Linux: Parse `/proc/meminfo` for accurate cached/buffers values
  - Windows: Return 0 for cached/buffers (not available via sysinfo)
  - Automatic inclusion in `GET /get-system-info` endpoint response

## [0.2.2] - 2026-01-07

### Added

- **Docker Logs Enhancement**: Time filtering with `start_time`, `end_time` (epoch ms)
- **Log Streaming (SSE)**: New `GET /docker/containers/{id}/logs/stream` endpoint
  - Real-time log streaming via Server-Sent Events
  - Initial tail lines configurable, stays open until disconnect
- **Real-time Container Stats**: New `GET /docker/containers/{id}/stats` endpoint
  - CPU, memory (usage/limit/percent), network I/O, block I/O, pids
- **Block I/O Metrics**: Added `block_read_bytes`, `block_write_bytes`, `pids` to container stats
- **Podman Support**: Works with Podman via socket path configuration
- **Docker Status Enhancement**: Added `backend` (docker/podman), `api_version`, `os`, `arch`
- **System Info Endpoint**: New `GET /get-system-info` for htop-like monitoring
  - boot_time, uptime, load_average (1/5/15 min)
  - Memory details (total, used, free, available)
  - Swap usage (total, used, free)
  - Network I/O per interface
  - Process stats (total, running, sleeping, stopped, zombie)
- **Network Historical**: New `GET /get-network-status` for network I/O history
  - Per-interface rx/tx bytes and packets (cumulative)
  - Optional time range filtering
- **Memory Details**: Added `total` and `used` to `GET /get-mem-status`
- **Optional Time Params**: CPU/Mem/Disk/Network status endpoints now return latest frame if no params
- **Bruno Collection**: Added `get-container-stats.bru`, `stream-container-logs.bru`, `get-system-info.bru`, `get-network-status.bru`

### Changed

- **Container Inspect**: `GET /docker/containers/{id}` now returns full inspect details (ports, volumes, env, networks, health)
- **Endpoint Consolidation**: Removed separate `/docker/containers/{id}/inspect` endpoint (merged into `/docker/containers/{id}`)

### Removed

- Separate `/docker/containers/{id}/inspect` endpoint (now part of `/docker/containers/{id}`)

## [0.2.1] - 2026-01-05

### Added

- **Docker Monitoring**: Monitor containers and view logs

## [0.2.0] - 2026-01-04

### Added

- **HTTP Request Logging**: Structured logging with `tracing` and `tracing-subscriber` for all HTTP requests/responses with latency tracking
- **Configuration Management System**: Migrated from `dotenv` to `config-rs` for hierarchical, type-safe configuration
  - Environment-specific configs: `config/default.toml`, `config/development.toml`, `config/production.toml`
  - Environment variable overrides with `REMON__*` prefix
  - Comprehensive `CONFIG.md` documentation
- **JWT Security Validation**: Startup validation for JWT secret strength
  - Minimum 32-character requirement enforced in production
  - Development warnings for weak secrets
  - Automatic failure in production if secret is weak/default
- **Bruno API Collection**: Complete API testing collection with 18 endpoints organized into folders (Public, Auth, Monitor, Process, Logs)
- **Tower-HTTP TraceLayer**: HTTP middleware for request/response logging with latency tracking in milliseconds

### Changed

- **API Framework Migration**: Complete migration from Hyper 0.14 to Axum 0.8
  - Reduced codebase from ~1200 lines to ~550 lines (54% reduction)
  - Replaced manual routing with declarative Axum router
  - Implemented extractors for type-safe request handling
  - Centralized authentication via middleware (eliminated per-endpoint checks)
  - Automatic JSON serialization/deserialization
- **Database Connection Pooling**: Thread-safe implementation using `tokio::sync::OnceCell` instead of `async_once`
- **Configuration**: All hardcoded values moved to configuration files
  - Server port, database settings, JWT secret, monitoring intervals, gRPC settings
  - Environment-based config loading (RUN_ENV: development/production)
- **Dependencies**: Updated to latest versions
  - `axum = "0.8.8"`
  - `tower = "0.5.2"`
  - `config = "0.14"`
  - `tracing = "0.1"`
  - `tracing-subscriber = "0.3"`

### Fixed

- **TOTP Secret Generation**: Fixed panic due to oversized secrets
  - Now uses blake3 hash to generate consistent 160-bit secrets
  - Proper error handling instead of `.unwrap()`
- **Logger Initialization**: Fixed panic when multiple loggers try to initialize
  - Changed `init()` to `try_init()` to handle conflicts gracefully
- **Thread Safety**: Fixed `Send` trait issues with database connection pooling

### Removed

- `dotenv` dependency (replaced by `config-rs`)
- `lazy_static` dependency (replaced by `tokio::sync::OnceCell`)
- `async_once` dependency (replaced by `tokio::sync::OnceCell`)
- Old Hyper-based API handlers
- Manual routing code and request parsing

### Security

- **JWT Secret Enforcement**: Production builds now fail to start with weak/default JWT secrets
- **Configuration Validation**: Startup validation prevents insecure deployments
- **Development Warnings**: Clear warnings in development mode for security misconfigurations

## [0.1.2]

### Added

- v0.1.2 kill process added

## [0.1.1]

### Added

- v0.1.1 CHANGELOG.md added
- v0.1.1 versioning started
- v0.1.1 gRPC added for other services to notify their status
- v0.1.1 proto file(s) introduced for gRPC. its a simple for now but will be extended
- v0.1.1 build.rs added to compile proto files

### Changed

- v0.1.1 tokio crate updated to latest version
- v0.1.1 Duration::seconds() is now deprecated, changed to try_seconds()

## [0.1.0]

### Added

- v0.1.0 initial

### Fixed

### Changed

### Removed
