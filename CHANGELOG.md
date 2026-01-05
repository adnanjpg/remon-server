# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),

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
