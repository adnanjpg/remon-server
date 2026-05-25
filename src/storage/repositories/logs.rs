use sqlx::SqlitePool;

use crate::error::AppResult;

pub struct LogRepository {
    pool: SqlitePool,
}

impl LogRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // Insert log entry
    pub async fn insert(
        &self,
        level: i32,
        source: &str,
        target: &str,
        message: &str,
    ) -> AppResult<()> {
        sqlx::query(
            r#"
            INSERT INTO logs (timestamp, level, source, target, message)
            VALUES (unixepoch(), ?, ?, ?, ?)
            "#,
        )
        .bind(level)
        .bind(source)
        .bind(target)
        .bind(message)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

}
