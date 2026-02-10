use sqlx::SqlitePool;

use crate::error::AppResult;

pub struct AlertRepository {
    pool: SqlitePool,
}

impl AlertRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // TODO: Implement alert rule CRUD operations
    // This is a placeholder for alert system integration
    pub async fn get_enabled_rules(&self) -> AppResult<Vec<()>> {
        // Placeholder: returns empty vec for now
        Ok(vec![])
    }
}
