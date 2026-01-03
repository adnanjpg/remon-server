# Configuration Guide

remon-server uses a layered configuration system powered by [config-rs](https://github.com/mehcode/config-rs), providing flexible and type-safe configuration management.

## Configuration Hierarchy

Configuration values are loaded in the following order (later sources override earlier ones):

1. **Base Configuration**: `config/default.toml` - Default values for all environments
2. **Environment-Specific**: `config/<RUN_ENV>.toml` - Environment-specific overrides
3. **Environment Variables**: `REMON__*` - Runtime overrides via env vars

## Environment Files

### `config/default.toml`
Contains base settings used across all environments:
```toml
[server]
port = 8080
host = "0.0.0.0"

[database]
path = "./db/monitor.sqlite3"
folder_path = "./db"
max_connections = 1

[auth]
jwt_secret = "d3f4ult"  # CHANGE IN PRODUCTION!

[monitoring]
update_interval_ms = 5000
enable_notifications = true

[grpc]
address = "127.0.0.1:50051"
enable_reflection = true

[logging]
level = "info"
format = "compact"

[fcm]
credentials_path = "./.remon-mobile-fcm-creds.json"
```

### `config/development.toml`
Development-specific overrides:
```toml
[logging]
level = "debug"

[grpc]
enable_reflection = true
```

### `config/production.toml`
Production-specific settings:
```toml
[logging]
level = "info"
format = "json"

[monitoring]
update_interval_ms = 10000

[grpc]
enable_reflection = false
```

## Environment Variables

### Switching Environments

Set the `RUN_ENV` variable to determine which config file to load:

```bash
# Development (default)
RUN_ENV=development cargo run

# Production
RUN_ENV=production cargo run

# Staging
RUN_ENV=staging cargo run
```

### Overriding Configuration

You can override any configuration value using environment variables with the `REMON__` prefix. Use double underscores (`__`) to navigate nested config structures:

```bash
# Override server port
REMON__SERVER__PORT=9000 cargo run

# Override JWT secret (recommended for production)
REMON__AUTH__JWT_SECRET="my-super-secret-key" cargo run

# Override database max connections
REMON__DATABASE__MAX_CONNECTIONS=5 cargo run

# Override logging level
REMON__LOGGING__LEVEL=debug cargo run

# Multiple overrides
REMON__SERVER__PORT=9000 \
REMON__AUTH__JWT_SECRET="prod-secret" \
REMON__LOGGING__LEVEL=info \
cargo run
```

## Configuration Structure

### Server
- `port`: HTTP server port (default: 8080)
- `host`: Bind address (default: "0.0.0.0")

### Database
- `path`: SQLite database file path (default: "./db/monitor.sqlite3")
- `folder_path`: Database folder path (default: "./db")
- `max_connections`: Connection pool size (default: 1)

### Authentication
- `jwt_secret`: Secret key for JWT tokens (**CHANGE IN PRODUCTION!**)

### Monitoring
- `update_interval_ms`: System monitoring interval in milliseconds (default: 5000)
- `enable_notifications`: Enable/disable push notifications (default: true)

### gRPC
- `address`: gRPC server address (default: "127.0.0.1:50051")
- `enable_reflection`: Enable gRPC reflection for debugging (default: true in dev, false in prod)

### Logging
- `level`: Log level - trace, debug, info, warn, error (default: "info")
- `format`: Log format - compact, json (default: "compact")

### FCM (Firebase Cloud Messaging)
- `credentials_path`: Path to FCM credentials JSON file

## Security Best Practices

1. **Never commit sensitive values** to `default.toml` or environment-specific configs
2. **Use environment variables** for secrets in production:
   ```bash
   REMON__AUTH__JWT_SECRET="$(cat /run/secrets/jwt_secret)" cargo run
   ```
3. **Change the default JWT secret** before deploying to production
4. **Use different secrets** for each environment (dev, staging, prod)

## Docker Deployment

When running in Docker, you can:

1. **Mount config files**:
   ```dockerfile
   COPY config/ /app/config/
   ```

2. **Use environment variables**:
   ```dockerfile
   ENV RUN_ENV=production
   ENV REMON__AUTH__JWT_SECRET=your-secret-here
   ```

3. **Use Docker secrets**:
   ```bash
   docker run -e REMON__AUTH__JWT_SECRET="$(docker secret inspect jwt_secret)" remon-server
   ```

## Migration from dotenv

If you're migrating from the old `.env` system:

| Old `.env` Variable | New Config Path | Environment Variable Override |
|---------------------|-----------------|-------------------------------|
| `JWT_SECRET` | `auth.jwt_secret` | `REMON__AUTH__JWT_SECRET` |
| `PORT` | `server.port` | `REMON__SERVER__PORT` |
| `GOOGLE_APPLICATION_CREDENTIALS` | `fcm.credentials_path` | `REMON__FCM__CREDENTIALS_PATH` |

## Debugging Configuration

To see the final merged configuration, you can add debug logging in `main.rs`:

```rust
let config = config::Config::new()?;
println!("Loaded config: {:#?}", config);
```

Or use environment variable to see config loading:
```bash
RUST_LOG=config=debug cargo run
```
