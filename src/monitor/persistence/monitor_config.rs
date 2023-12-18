use sqlx::SqliteConnection;

use crate::monitor::MonitorConfig;

use super::{get_sql_connection, SQLITE_DB_CONN_STR};

const MONITOR_CONFIGS_TABLE_NAME: &str = "configs";

pub async fn insert_monitor_config(
    config: &MonitorConfig,
    device_id: &str,
) -> Result<(), sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let statement = format!("INSERT OR REPLACE INTO {} (device_id, cpu_threshold, mem_threshold, storage_threshold) VALUES (?, ?, ?, ?)",MONITOR_CONFIGS_TABLE_NAME);
    sqlx::query(&statement)
        .bind(&device_id)
        .bind(config.cpu_threshold)
        .bind(config.mem_threshold)
        .bind(config.storage_threshold)
        .execute(&mut conn)
        .await?;

    Ok(())
}

pub async fn fetch_monitor_configs() -> Result<Vec<MonitorConfig>, sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let statement = format!("SELECT * FROM {}", MONITOR_CONFIGS_TABLE_NAME);
    let configs = sqlx::query_as::<_, MonitorConfig>(&statement)
        .fetch_all(&mut conn)
        .await?;

    Ok(configs)
}

pub(super) async fn create_monitor_configs_table(
    conn: &mut SqliteConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        device_id TEXT PRIMARY KEY,
        cpu_threshold REAL NOT NULL,
        mem_threshold REAL NOT NULL,
        storage_threshold REAL NOT NULL
    )",
        MONITOR_CONFIGS_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}
