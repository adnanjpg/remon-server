use sqlx::{SqliteConnection, SqlitePool};

use crate::auth::service::CreatedTokens;
use crate::error::{AppError, AppResult};
use crate::models::auth::StoredDevice;
use crate::notify::Severity;

pub struct DeviceRepository {
    pool: SqlitePool,
}

/// A browser's Web Push registration as stored on its device row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WebPushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    /// The browser's own VAPID key (PKCS#8 PEM), which its pushes are signed with.
    pub vapid_key: String,
    /// Client handle echoed back in each payload.
    pub reference: Option<String>,
    /// `warn` or `crit`; `None` takes both.
    pub min_severity: Option<String>,
}

/// A subscribed device, ready to send to.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WebPushTarget {
    pub device_id: String,
    #[sqlx(flatten)]
    pub subscription: WebPushSubscription,
}

impl DeviceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, device: &StoredDevice) -> AppResult<()> {
        sqlx::query!(
            "INSERT INTO devices (id, name, token_hash, last_ip, last_seen, created_at, is_active)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            device.id,
            device.name,
            device.token_hash,
            device.last_ip,
            device.last_seen,
            device.created_at,
            device.is_active,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_by_id(&self, id: &str) -> AppResult<Option<StoredDevice>> {
        let row = sqlx::query_as!(
            StoredDevice,
            r#"SELECT id as "id!", name as "name!", token_hash as "token_hash!",
                      last_ip, last_seen, created_at,
                      is_active as "is_active: bool"
               FROM devices WHERE id = ?"#,
            id
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn get_all(&self) -> AppResult<Vec<StoredDevice>> {
        let rows = sqlx::query_as!(
            StoredDevice,
            r#"SELECT id as "id!", name as "name!", token_hash as "token_hash!",
                      last_ip, last_seen, created_at,
                      is_active as "is_active: bool"
               FROM devices ORDER BY created_at DESC"#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn update_last_seen(&self, id: &str, ip: Option<&str>) -> AppResult<()> {
        sqlx::query!(
            "UPDATE devices SET last_seen = unixepoch(), last_ip = COALESCE(?, last_ip)
             WHERE id = ?",
            ip,
            id,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update_name(&self, id: &str, name: &str) -> AppResult<()> {
        let result = sqlx::query!("UPDATE devices SET name = ? WHERE id = ?", name, id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound("Device".into()));
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn deactivate(&self, id: &str) -> AppResult<()> {
        let result = sqlx::query!("UPDATE devices SET is_active = 0 WHERE id = ?", id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound("Device".into()));
        }
        Ok(())
    }

    pub async fn delete(&self, id: &str) -> AppResult<()> {
        let result = sqlx::query!("DELETE FROM devices WHERE id = ?", id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound("Device".into()));
        }
        Ok(())
    }

    // Session management

    /// Replace all of this device's sessions with a fresh pair, as rotation
    /// does. A device is one client, so a login supersedes its earlier ones;
    /// otherwise every cold start leaves a refresh row alive for its full TTL.
    pub async fn replace_session_pair(
        &self,
        device_id: &str,
        tokens: &CreatedTokens,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM sessions WHERE device_id = ?")
            .bind(device_id)
            .execute(&mut *tx)
            .await?;
        Self::insert_session_pair(&mut tx, device_id, tokens).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Consume the old refresh token and replace all of this device's
    /// sessions atomically. A failed insert restores the entire old set.
    pub async fn rotate_session_pair(
        &self,
        refresh_jti: &str,
        device_id: &str,
        tokens: &CreatedTokens,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        // Write first: acquire SQLite's writer lock before reading device
        // state, avoiding a deferred read-to-write snapshot upgrade race.
        let consumed = sqlx::query(
            "DELETE FROM sessions WHERE id = ? AND device_id = ? AND expires_at > unixepoch()",
        )
        .bind(refresh_jti)
        .bind(device_id)
        .execute(&mut *tx)
        .await?;
        if consumed.rows_affected() == 0 {
            return Err(AppError::InvalidToken);
        }

        let active: Option<bool> = sqlx::query_scalar("SELECT is_active FROM devices WHERE id = ?")
            .bind(device_id)
            .fetch_optional(&mut *tx)
            .await?;
        match active {
            None => return Err(AppError::DeviceNotFound),
            Some(false) => return Err(AppError::DeviceInactive),
            Some(true) => {}
        }

        sqlx::query("DELETE FROM sessions WHERE device_id = ?")
            .bind(device_id)
            .execute(&mut *tx)
            .await?;
        Self::insert_session_pair(&mut tx, device_id, tokens).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn insert_session_pair(
        connection: &mut SqliteConnection,
        device_id: &str,
        tokens: &CreatedTokens,
    ) -> AppResult<()> {
        for (jti, expires_at) in [
            (&tokens.access_jti, tokens.access_expires_at),
            (&tokens.refresh_jti, tokens.refresh_expires_at),
        ] {
            sqlx::query("INSERT INTO sessions (id, device_id, expires_at) VALUES (?, ?, ?)")
                .bind(jti)
                .bind(device_id)
                .bind(expires_at)
                .execute(&mut *connection)
                .await?;
        }
        Ok(())
    }

    /// Returns true if a non-expired session row exists for the given jti.
    /// Used by the auth middleware to enforce revocation.
    pub async fn session_exists(&self, jti: &str) -> AppResult<bool> {
        let row: Option<i64> = sqlx::query_scalar!(
            "SELECT 1 FROM sessions WHERE id = ? AND expires_at > unixepoch()",
            jti
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    pub async fn delete_session(&self, session_id: &str) -> AppResult<()> {
        sqlx::query!("DELETE FROM sessions WHERE id = ?", session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn cleanup_expired_sessions(&self) -> AppResult<u64> {
        let result = sqlx::query!("DELETE FROM sessions WHERE expires_at < unixepoch()")
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// Per-device active session count (non-expired). Used to surface
    /// "this device has N live tokens" on the sessions management UI —
    /// devices with 0 sessions are paired but not currently logged in.
    pub async fn count_active_sessions_per_device(&self) -> AppResult<Vec<(String, i64)>> {
        let rows = sqlx::query!(
            r#"SELECT device_id, COUNT(*) as "cnt: i64"
               FROM sessions
              WHERE expires_at > unixepoch()
              GROUP BY device_id"#
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| (r.device_id, r.cnt))
        .collect();
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

    /// All active devices with a Web Push subscription. The columns are set
    /// together or cleared together; the endpoint stands for all of them.
    pub async fn list_active_web_push_targets(&self) -> AppResult<Vec<WebPushTarget>> {
        let rows = sqlx::query_as::<_, WebPushTarget>(
            r#"SELECT id AS device_id,
                      web_push_endpoint     AS endpoint,
                      web_push_p256dh       AS p256dh,
                      web_push_auth         AS auth,
                      web_push_vapid_key    AS vapid_key,
                      web_push_ref          AS reference,
                      web_push_min_severity AS min_severity
                 FROM devices
                WHERE is_active = 1 AND web_push_endpoint IS NOT NULL"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Whether any subscribed browser takes notifications of this severity.
    pub async fn has_web_push_target(&self, severity: Severity) -> AppResult<bool> {
        let found: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM devices
                            WHERE is_active = 1 AND web_push_endpoint IS NOT NULL
                              AND (web_push_min_severity IS NULL OR web_push_min_severity = 'warn'
                                   OR ? = 'crit'))",
        )
        .bind(severity.as_str())
        .fetch_one(&self.pool)
        .await?;
        Ok(found)
    }

    pub async fn set_fcm_token(&self, device_id: &str, fcm_token: Option<&str>) -> AppResult<()> {
        let result = sqlx::query!(
            "UPDATE devices SET fcm_token = ? WHERE id = ?",
            fcm_token,
            device_id,
        )
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound("Device".into()));
        }
        Ok(())
    }

    /// Persist the calling browser's Web Push subscription on its device row,
    /// or clear every field of it with `None`.
    pub async fn set_web_push_subscription(
        &self,
        device_id: &str,
        sub: Option<&WebPushSubscription>,
    ) -> AppResult<()> {
        let result = sqlx::query(
            "UPDATE devices
                SET web_push_endpoint = ?, web_push_p256dh = ?, web_push_auth = ?,
                    web_push_vapid_key = ?, web_push_ref = ?, web_push_min_severity = ?
              WHERE id = ?",
        )
        .bind(sub.map(|s| &s.endpoint))
        .bind(sub.map(|s| &s.p256dh))
        .bind(sub.map(|s| &s.auth))
        .bind(sub.map(|s| &s.vapid_key))
        .bind(sub.and_then(|s| s.reference.as_ref()))
        .bind(sub.and_then(|s| s.min_severity.as_ref()))
        .bind(device_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound("Device".into()));
        }
        Ok(())
    }
}
