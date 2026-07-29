use log::info;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::str::FromStr;
use std::time::Duration;

use crate::storage::repositories::*;

// ── SQLite tuning constants ────────────────────────────────────────────────
//
// These are the knobs we deliberately deviate from SQLite's defaults on. The
// values here reflect a read-heavy time-series workload: many concurrent
// metrics-history scans, a single writer producing 0.5–1 KB of stats every
// 2 seconds, and a few background aggregators churning through closed
// rollup buckets on a 1-minute cadence.

/// How long a query waits for a busy DB before erroring out. WAL only
/// serialises writes, so contention is brief — but background loops can
/// still collide with HTTP handlers under load.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Page cache per connection, in KB (negative value per SQLite convention).
/// 64 MB lets the working set of a typical metrics-history range query live
/// in memory; the default 2 MB forces re-reads on every scan.
const CACHE_SIZE_KB: i64 = -65_536;

/// Memory-mapped I/O ceiling, in bytes. Hot pages bypass the read syscall
/// path entirely. Pairs well with WAL for read-heavy mixes; the OS handles
/// eviction so this is an upper bound, not a reservation.
const MMAP_SIZE_BYTES: u64 = 256 * 1024 * 1024;

/// Truncate the WAL back to this size after each checkpoint instead of
/// leaving it at its high-water mark. The 2-second collector writes across
/// ~8 metric tables churn the WAL steadily; without a limit the file sits
/// at whatever the busiest burst grew it to. 8 MB comfortably spans one
/// autocheckpoint window.
const JOURNAL_SIZE_LIMIT_BYTES: i64 = 8 * 1024 * 1024;

/// Database wrapper with repository access
#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Connect to SQLite database with WAL mode and tuned pragmas.
    ///
    /// Pragmas applied:
    /// - `journal_mode=WAL`: concurrent reads while a writer is active.
    /// - `synchronous=NORMAL`: durable across power loss when paired with WAL.
    /// - `busy_timeout`: see [`BUSY_TIMEOUT`].
    /// - `foreign_keys=ON`: enforce ON DELETE CASCADE relations.
    /// - `cache_size`: see [`CACHE_SIZE_KB`].
    /// - `temp_store=MEMORY`: keep temp tables, sort scratch, and
    ///   intermediate join buffers in RAM instead of spilling to disk.
    /// - `mmap_size`: see [`MMAP_SIZE_BYTES`].
    /// - `wal_autocheckpoint=1000`: checkpoint every ~4 MB (the default,
    ///   pinned explicitly so it can't drift).
    /// - `journal_size_limit`: see [`JOURNAL_SIZE_LIMIT_BYTES`].
    pub async fn connect(url: &str, max_connections: u32) -> anyhow::Result<Self> {
        let opts = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(BUSY_TIMEOUT)
            .foreign_keys(true)
            .pragma("cache_size", CACHE_SIZE_KB.to_string())
            .pragma("temp_store", "MEMORY")
            .pragma("mmap_size", MMAP_SIZE_BYTES.to_string())
            .pragma("wal_autocheckpoint", "1000")
            .pragma("journal_size_limit", JOURNAL_SIZE_LIMIT_BYTES.to_string());

        // sqlx's idle/lifetime defaults target network databases; against a
        // local file reaping only discards a warm page cache. Holding the pool
        // full also keeps an in-memory database alive.
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .min_connections(max_connections)
            .idle_timeout(None)
            .max_lifetime(None)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(opts)
            .await?;

        info!(
            "connected to database (WAL, foreign_keys=ON, max_connections={}, cache={}MB, mmap={}MB)",
            max_connections,
            CACHE_SIZE_KB.unsigned_abs() / 1024,
            MMAP_SIZE_BYTES / 1024 / 1024
        );

        Ok(Self { pool })
    }

    /// Run all pending migrations from `./migrations`.
    ///
    /// Migrations are versioned files (`NNNN_<name>.sql`) tracked in the
    /// `_sqlx_migrations` table that sqlx maintains automatically.
    pub async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        info!("database migrations applied");
        Ok(())
    }

    pub fn config(&self) -> ConfigRepository {
        ConfigRepository::new(self.pool.clone())
    }

    /// Get raw pool (for tests, or for operations that span multiple repos).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}
