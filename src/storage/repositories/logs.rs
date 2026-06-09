use sqlx::{Row, SqlitePool};

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

/// One row read back for `GET /logs`.
pub struct LogRow {
    pub id: i64,
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

    /// Newest-first page of log entries. `max_level` filters by numeric
    /// level (1=error … 5=trace), so e.g. 2 returns warnings and errors.
    /// `id DESC` tiebreak keeps same-second bursts in emission order.
    pub async fn list(
        &self,
        max_level: i32,
        start: i64,
        end: i64,
        limit: u32,
    ) -> AppResult<Vec<LogRow>> {
        let rows = sqlx::query(
            r#"SELECT id, timestamp, level, source, target, message
               FROM logs
              WHERE level <= ? AND timestamp >= ? AND timestamp <= ?
              ORDER BY timestamp DESC, id DESC
              LIMIT ?"#,
        )
        .bind(max_level)
        .bind(start)
        .bind(end)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| LogRow {
                id: r.get("id"),
                timestamp: r.get("timestamp"),
                level: r.get("level"),
                source: r.get("source"),
                target: r.get("target"),
                message: r.get("message"),
            })
            .collect())
    }
}
