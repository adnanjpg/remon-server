use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::SqlitePool;
use std::str::FromStr;
use std::time::Duration;
use log::info;

use crate::storage::repositories::*;

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
    /// - `busy_timeout=5s`: avoid SQLITE_BUSY under contention.
    /// - `foreign_keys=ON`: enforce ON DELETE CASCADE relations.
    pub async fn connect(url: &str, max_connections: u32) -> anyhow::Result<Self> {
        let opts = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(opts)
            .await?;

        info!(
            "Connected to database (WAL, foreign_keys=ON, max_connections={})",
            max_connections
        );

        Ok(Self { pool })
    }

    /// Run all pending migrations from `./migrations`.
    ///
    /// Migrations are versioned files (`NNNN_<name>.sql`) tracked in the
    /// `_sqlx_migrations` table that sqlx maintains automatically.
    pub async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        info!("Database migrations applied");
        Ok(())
    }

    // Repository accessors

    pub fn alerts(&self) -> AlertRepository {
        AlertRepository::new(self.pool.clone())
    }

    pub fn config(&self) -> ConfigRepository {
        ConfigRepository::new(self.pool.clone())
    }

    pub fn devices(&self) -> DeviceRepository {
        DeviceRepository::new(self.pool.clone())
    }

    pub fn metrics(&self) -> MetricsRepository {
        MetricsRepository::new(self.pool.clone())
    }

    pub fn resolutions(&self) -> ResolutionRepository {
        ResolutionRepository::new(self.pool.clone())
    }

    pub fn retention(&self) -> RetentionRepository {
        RetentionRepository::new(self.pool.clone())
    }

    pub fn rollup_state(&self) -> RollupStateRepository {
        RollupStateRepository::new(self.pool.clone())
    }

    pub fn logs(&self) -> LogRepository {
        LogRepository::new(self.pool.clone())
    }

    /// Get raw pool (for tests, or for operations that span multiple repos).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}
