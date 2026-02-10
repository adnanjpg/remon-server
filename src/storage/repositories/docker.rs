use sqlx::SqlitePool;

use crate::error::AppResult;

pub struct DockerRepository {
    pool: SqlitePool,
}

impl DockerRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // TODO: Implement Docker container/image tracking
    // This is a placeholder for Docker integration
    pub async fn get_tracked_containers(&self) -> AppResult<Vec<()>> {
        // Placeholder: returns empty vec for now
        Ok(vec![])
    }
}
