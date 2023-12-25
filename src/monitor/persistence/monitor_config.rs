use sqlx::SqliteConnection;

use crate::monitor::models::MonitorConfig;

use super::{get_sql_connection, FetchId, SQLITE_DB_CONN_STR};

const MONITOR_CONFIGS_TABLE_NAME: &str = "configs";

pub async fn insert_or_update_monitor_config(
    config: &MonitorConfig,
    device_id: &str,
) -> Result<(), sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    // check if a record with the same device_id already exists
    let exists_record_check = format!(
        "SELECT id FROM {} WHERE device_id = ?",
        MONITOR_CONFIGS_TABLE_NAME
    );
    let exists_check_res = sqlx::query_as::<_, FetchId>(&exists_record_check)
        .bind(&device_id)
        .fetch_optional(&mut conn)
        .await?;

    match exists_check_res {
        Some(value) => {
            let statement = format!(
                "UPDATE {}
                SET 
                cpu_threshold = ?, 
                disk_threshold = ?,
                mem_threshold = ?,
                fcm_token = ?,
                updated_at = ?
                WHERE id = ?",
                MONITOR_CONFIGS_TABLE_NAME
            );

            sqlx::query(&statement)
                .bind(&config.cpu_threshold)
                .bind(&config.disk_threshold)
                .bind(&config.mem_threshold)
                .bind(&config.fcm_token)
                .bind(&config.updated_at)
                .bind(value.id)
                .execute(&mut conn)
                .await?;
        }
        None => {
            let statement = format!(
                "INSERT INTO {} 
            (device_id, cpu_threshold, mem_threshold, disk_threshold, fcm_token, updated_at) 
            VALUES (?, ?, ?, ?, ?, ?)
            ",
                MONITOR_CONFIGS_TABLE_NAME
            );
            sqlx::query(&statement)
                .bind(&device_id)
                .bind(&config.cpu_threshold)
                .bind(&config.mem_threshold)
                .bind(&config.disk_threshold)
                .bind(&config.fcm_token)
                .bind(&config.updated_at)
                .execute(&mut conn)
                .await?;
        }
    };

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
        id INTEGER PRIMARY KEY NOT NULL,
        device_id TEXT NOT NULL,
        cpu_threshold REAL NOT NULL,
        mem_threshold REAL NOT NULL,
        disk_threshold REAL NOT NULL,
        fcm_token TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    )",
        MONITOR_CONFIGS_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}
