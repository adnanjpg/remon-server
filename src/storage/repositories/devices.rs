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
        let row = sqlx::query_as::<_, StoredDevice>(
            "SELECT id, name, token_hash, totp_secret, last_ip, last_seen, created_at, is_active
             FROM devices WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn get_all(&self) -> AppResult<Vec<StoredDevice>> {
        let rows = sqlx::query_as::<_, StoredDevice>(
            "SELECT id, name, token_hash, totp_secret, last_ip, last_seen, created_at, is_active
             FROM devices ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
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
    pub async fn create_session(
        &self,
        session_id: &str,
        device_id: &str,
        expires_at: i64,
    ) -> AppResult<()> {
        sqlx::query("INSERT INTO sessions (id, device_id, expires_at) VALUES (?, ?, ?)")
            .bind(session_id)
            .bind(device_id)
            .bind(expires_at)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    /// Returns true if a non-expired session row exists for the given jti.
    /// Used by the auth middleware to enforce revocation.
    pub async fn session_exists(&self, jti: &str) -> AppResult<bool> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT 1 FROM sessions WHERE id = ? AND expires_at > unixepoch()")
                .bind(jti)
                .fetch_optional(&self.pool)
                .await?;

        Ok(row.is_some())
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

    /// Per-device active session count (non-expired). Used to surface
    /// "this device has N live tokens" on the sessions management UI —
    /// devices with 0 sessions are paired but not currently logged in.
    pub async fn count_active_sessions_per_device(&self) -> AppResult<Vec<(String, i64)>> {
        let rows = sqlx::query_as::<_, (String, i64)>(
            r#"
            SELECT device_id, COUNT(*) as cnt
              FROM sessions
             WHERE expires_at > unixepoch()
             GROUP BY device_id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// All active devices that have an FCM push token registered. The
    /// notification fan-out targets exactly this set.
    pub async fn list_active_fcm_targets(&self) -> AppResult<Vec<(String, String)>> {
        // (device_id, fcm_token)
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT id, fcm_token FROM devices
              WHERE is_active = 1 AND fcm_token IS NOT NULL AND fcm_token != ''",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// All active devices with a complete Web Push subscription triplet.
    /// `(device_id, endpoint, p256dh, auth)`. Devices that opted out (or
    /// never opted in) read as IS NULL on at least one of the columns
    /// and are filtered here so the channel doesn't even try to encrypt
    /// with missing keys.
    pub async fn list_active_web_push_targets(
        &self,
    ) -> AppResult<Vec<(String, String, String, String)>> {
        let rows = sqlx::query_as::<_, (String, String, String, String)>(
            r#"
            SELECT id, web_push_endpoint, web_push_p256dh, web_push_auth
              FROM devices
             WHERE is_active = 1
               AND web_push_endpoint IS NOT NULL AND web_push_endpoint != ''
               AND web_push_p256dh   IS NOT NULL AND web_push_p256dh   != ''
               AND web_push_auth     IS NOT NULL AND web_push_auth     != ''
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn set_fcm_token(&self, device_id: &str, fcm_token: Option<&str>) -> AppResult<()> {
        let result = sqlx::query("UPDATE devices SET fcm_token = ? WHERE id = ?")
            .bind(fcm_token)
            .bind(device_id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(AppError::NotFound("Device".into()));
        }
        Ok(())
    }

    /// Persist (or clear) the calling browser's Web Push subscription
    /// triplet on its device row. All-three-or-none semantics: passing
    /// any field as `None` while the others are `Some` would leave the
    /// row in an unsendable mixed state, so callers should pass `None`
    /// everywhere to unsubscribe.
    pub async fn set_web_push_subscription(
        &self,
        device_id: &str,
        endpoint: Option<&str>,
        p256dh: Option<&str>,
        auth: Option<&str>,
    ) -> AppResult<()> {
        let result = sqlx::query(
            "UPDATE devices SET web_push_endpoint = ?, web_push_p256dh = ?, web_push_auth = ? WHERE id = ?",
        )
        .bind(endpoint)
        .bind(p256dh)
        .bind(auth)
        .bind(device_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound("Device".into()));
        }
        Ok(())
    }
}
