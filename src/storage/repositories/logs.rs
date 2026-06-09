use sqlx::SqlitePool;

use crate::error::AppResult;

/// One row bound for `logs` insertion. `timestamp` is the emission time
/// captured by `DbLayer`, not the insert time — with the batched writer
/// the two can drift by a flush interval.
pub struct NewLogRow {
    pub timestamp: i64,
    pub level: i32,
    pub source: String,
    pub target: String,
    pub message: String,
}

pub struct LogRepository {
    pool: SqlitePool,
}

impl LogRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a batch of log entries in one transaction. SQLite's cost is
    /// per-transaction fsync, so a bursty `recv_many` drain lands as a
    /// single commit instead of one commit per line.
    pub async fn insert_batch(&self, rows: &[NewLogRow]) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        for r in rows {
            sqlx::query(
                r#"
                INSERT INTO logs (timestamp, level, source, target, message)
                VALUES (?, ?, ?, ?, ?)
                "#,
            )
            .bind(r.timestamp)
            .bind(r.level)
            .bind(&r.source)
            .bind(&r.target)
            .bind(&r.message)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
