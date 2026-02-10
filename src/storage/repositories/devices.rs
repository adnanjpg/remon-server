use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::models::auth::StoredDevice;

pub struct DeviceRepository {
    pool: SqlitePool,
}

impl DeviceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, device: &StoredDevice) -> AppResult<()> {
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
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_by_id(&self, id: &str) -> AppResult<Option<StoredDevice>> {
        let row = sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>, i64, i64, bool)>(
            r#"
            SELECT id, name, token_hash, totp_secret, last_ip, last_seen, created_at, is_active
            FROM devices WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| StoredDevice {
            id: r.0,
            name: r.1,
            token_hash: r.2,
            totp_secret: r.3,
            last_ip: r.4,
            last_seen: r.5,
            created_at: r.6,
            is_active: r.7,
        }))
    }

    pub async fn get_all(&self) -> AppResult<Vec<StoredDevice>> {
        let rows = sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>, i64, i64, bool)>(
            r#"
            SELECT id, name, token_hash, totp_secret, last_ip, last_seen, created_at, is_active
            FROM devices ORDER BY created_at DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| StoredDevice {
                id: r.0,
                name: r.1,
                token_hash: r.2,
                totp_secret: r.3,
                last_ip: r.4,
                last_seen: r.5,
                created_at: r.6,
                is_active: r.7,
            })
            .collect())
    }

    pub async fn update_last_seen(&self, id: &str, ip: Option<&str>) -> AppResult<()> {
        sqlx::query(
            r#"
            UPDATE devices SET last_seen = unixepoch(), last_ip = COALESCE(?, last_ip)
            WHERE id = ?
            "#,
        )
        .bind(ip)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn update_name(&self, id: &str, name: &str) -> AppResult<()> {
        let result = sqlx::query("UPDATE devices SET name = ? WHERE id = ?")
            .bind(name)
            .bind(id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(AppError::NotFound("Device".into()));
        }

        Ok(())
    }

    pub async fn set_totp_secret(&self, id: &str, secret: Option<&str>) -> AppResult<()> {
        sqlx::query("UPDATE devices SET totp_secret = ? WHERE id = ?")
            .bind(secret)
            .bind(id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    pub async fn deactivate(&self, id: &str) -> AppResult<()> {
        let result = sqlx::query("UPDATE devices SET is_active = 0 WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(AppError::NotFound("Device".into()));
        }

        Ok(())
    }

    pub async fn delete(&self, id: &str) -> AppResult<()> {
        let result = sqlx::query("DELETE FROM devices WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(AppError::NotFound("Device".into()));
        }

        Ok(())
    }

    // Session management
    pub async fn create_session(&self, session_id: &str, device_id: &str, expires_at: i64) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO sessions (id, device_id, expires_at) VALUES (?, ?, ?)",
        )
        .bind(session_id)
        .bind(device_id)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn delete_session(&self, session_id: &str) -> AppResult<()> {
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(session_id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    pub async fn delete_device_sessions(&self, device_id: &str) -> AppResult<()> {
        sqlx::query("DELETE FROM sessions WHERE device_id = ?")
            .bind(device_id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    pub async fn cleanup_expired_sessions(&self) -> AppResult<u64> {
        let result = sqlx::query("DELETE FROM sessions WHERE expires_at < unixepoch()")
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected())
    }
}
