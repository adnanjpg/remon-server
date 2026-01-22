use sqlx::{Pool, Sqlite};

/// Device stored in database
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredDevice {
    pub id: String,
    pub name: String,
    pub token_hash: String,
    pub totp_secret: Option<String>,
    pub last_ip: Option<String>,
    pub last_seen: i64,
    pub created_at: i64,
    pub is_active: bool,
}

/// Create devices table
pub async fn create_devices_table(conn: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS devices (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            token_hash TEXT NOT NULL,
            totp_secret TEXT,
            last_ip TEXT,
            last_seen INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            is_active INTEGER NOT NULL DEFAULT 1
        )
        "#,
    )
    .execute(conn)
    .await?;

    Ok(())
}

/// Create sessions table
pub async fn create_sessions_table(conn: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            device_id TEXT NOT NULL,
            expires_at INTEGER NOT NULL,
            FOREIGN KEY (device_id) REFERENCES devices(id)
        )
        "#,
    )
    .execute(conn)
    .await?;

    Ok(())
}

/// Initialize device-related tables
pub async fn init_db(conn: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
    create_devices_table(conn).await?;
    create_sessions_table(conn).await?;
    Ok(())
}

// ==================== Device Repository ====================

/// Create a new device
pub async fn create_device(conn: &Pool<Sqlite>, device: &StoredDevice) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO devices (id, name, token_hash, totp_secret, last_ip, last_seen, created_at, is_active)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&device.id)
    .bind(&device.name)
    .bind(&device.token_hash)
    .bind(&device.totp_secret)
    .bind(&device.last_ip)
    .bind(device.last_seen)
    .bind(device.created_at)
    .bind(device.is_active)
    .execute(conn)
    .await?;

    Ok(())
}

/// Get device by ID
pub async fn get_device_by_id(
    conn: &Pool<Sqlite>,
    id: &str,
) -> Result<Option<StoredDevice>, sqlx::Error> {
    let device = sqlx::query_as::<_, StoredDevice>(
        r#"
        SELECT id, name, token_hash, totp_secret, last_ip, last_seen, created_at, is_active
        FROM devices WHERE id = ?
        "#,
    )
    .bind(id)
    .fetch_optional(conn)
    .await?;

    Ok(device)
}

/// Get all devices
pub async fn get_all_devices(conn: &Pool<Sqlite>) -> Result<Vec<StoredDevice>, sqlx::Error> {
    let devices = sqlx::query_as::<_, StoredDevice>(
        r#"
        SELECT id, name, token_hash, totp_secret, last_ip, last_seen, created_at, is_active
        FROM devices ORDER BY created_at DESC
        "#,
    )
    .fetch_all(conn)
    .await?;

    Ok(devices)
}

/// Update device last seen timestamp and IP
pub async fn update_last_seen(
    conn: &Pool<Sqlite>,
    id: &str,
    ip: Option<&str>,
) -> Result<(), sqlx::Error> {
    let now = chrono::Utc::now().timestamp();

    sqlx::query(
        r#"
        UPDATE devices SET last_seen = ?, last_ip = COALESCE(?, last_ip)
        WHERE id = ?
        "#,
    )
    .bind(now)
    .bind(ip)
    .bind(id)
    .execute(conn)
    .await?;

    Ok(())
}

/// Update device name
pub async fn update_device_name(
    conn: &Pool<Sqlite>,
    id: &str,
    name: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("UPDATE devices SET name = ? WHERE id = ?")
        .bind(name)
        .bind(id)
        .execute(conn)
        .await?;

    Ok(result.rows_affected())
}

/// Deactivate a device (soft delete)
pub async fn deactivate_device(conn: &Pool<Sqlite>, id: &str) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("UPDATE devices SET is_active = 0 WHERE id = ?")
        .bind(id)
        .execute(conn)
        .await?;

    Ok(result.rows_affected())
}

/// Delete a device permanently
pub async fn delete_device(conn: &Pool<Sqlite>, id: &str) -> Result<u64, sqlx::Error> {
    // First delete all sessions for this device
    sqlx::query("DELETE FROM sessions WHERE device_id = ?")
        .bind(id)
        .execute(conn)
        .await?;

    let result = sqlx::query("DELETE FROM devices WHERE id = ?")
        .bind(id)
        .execute(conn)
        .await?;

    Ok(result.rows_affected())
}

// ==================== Session Repository ====================

/// Create a new session
pub async fn create_session(
    conn: &Pool<Sqlite>,
    session_id: &str,
    device_id: &str,
    expires_at: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO sessions (id, device_id, expires_at) VALUES (?, ?, ?)")
        .bind(session_id)
        .bind(device_id)
        .bind(expires_at)
        .execute(conn)
        .await?;

    Ok(())
}

/// Delete a session
pub async fn delete_session(conn: &Pool<Sqlite>, session_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE id = ?")
        .bind(session_id)
        .execute(conn)
        .await?;

    Ok(())
}

/// Cleanup expired sessions
pub async fn cleanup_expired_sessions(conn: &Pool<Sqlite>) -> Result<u64, sqlx::Error> {
    let now = chrono::Utc::now().timestamp();

    let result = sqlx::query("DELETE FROM sessions WHERE expires_at < ?")
        .bind(now)
        .execute(conn)
        .await?;

    Ok(result.rows_affected())
}
