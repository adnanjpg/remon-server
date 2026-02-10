use sqlx::SqlitePool;

use crate::error::AppResult;

pub struct NotifierRepository {
    pool: SqlitePool,
}

impl NotifierRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // TODO: Implement notifier CRUD operations
    // This is a placeholder for notifier system integration
    pub async fn get_all_enabled(&self) -> AppResult<Vec<()>> {
        // Placeholder: returns empty vec for now
        Ok(vec![])
    }
}
