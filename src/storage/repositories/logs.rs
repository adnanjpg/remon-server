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

/// The read path for `GET /logs`. Built by hand rather than through the
/// `query!` macro, so it never lands in the `.sqlx` cache the query-plan audit
/// sweeps — it is named here so the audit can explain it explicitly.
pub(crate) const LIST_SQL: &str = r#"SELECT id, timestamp, level, source, target, message
               FROM logs
              WHERE level <= ? AND timestamp >= ? AND timestamp <= ?
              ORDER BY timestamp DESC, id DESC
              LIMIT ?"#;

pub struct LogRepository {
    pool: SqlitePool,
}

impl LogRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a batch of log entries as one statement, so a bursty
    /// `recv_many` drain costs one round trip and one commit rather than one
    /// of each per line.
    pub async fn insert_batch(&self, rows: &[NewLogRow]) -> AppResult<()> {
        // `push_values` over an empty iterator emits `VALUES` with nothing
        // after it; the drain can legitimately hand us nothing.
        if rows.is_empty() {
            return Ok(());
        }
        let mut qb = sqlx::QueryBuilder::new(
            "INSERT INTO logs (timestamp, level, source, target, message) ",
        );
        qb.push_values(rows.iter(), |mut b, r| {
            b.push_bind(r.timestamp)
                .push_bind(r.level)
                .push_bind(&r.source)
                .push_bind(&r.target)
                .push_bind(&r.message);
        });
        qb.build().execute(&self.pool).await?;
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
        let rows = sqlx::query(LIST_SQL)
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
