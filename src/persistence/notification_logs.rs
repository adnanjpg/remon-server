use sqlx::SqliteConnection;

use super::get_default_sql_connection;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
pub enum NotificationType {
    StatusLimitsExceeding,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct NotificationLog {
    pub id: i64,
    pub notification_type: NotificationType,
    pub device_id: String,
    pub sent_at: i64,
}

const NOTIFICATION_LOGS_TABLE_NAME: &str = "cpu_infos";

pub(super) async fn insert_notification_log(log: &NotificationLog) -> Result<(), sqlx::Error> {
    let mut conn = get_default_sql_connection().await?;

    let statement = format!(
        "INSERT INTO {} 
        (notification_type, device_id, sent_at) 
        VALUES (?, ?, ?)",
        NOTIFICATION_LOGS_TABLE_NAME
    );

    sqlx::query(&statement)
        .bind(&log.notification_type)
        .bind(&log.device_id)
        .bind(&log.sent_at)
        .execute(&mut conn)
        .await?;

    Ok(())
}

pub(super) async fn fetch_latest_for_device_id_and_type(
    device_id: &str,
    notification_type: &NotificationType,
) -> Result<Vec<NotificationLog>, sqlx::Error> {
    let mut conn = get_default_sql_connection().await?;

    let statement = format!(
        "
        SELECT *
        FROM {}
        WHERE device_id = ?
        AND notification_type = ?
        ",
        NOTIFICATION_LOGS_TABLE_NAME
    );
    let info = sqlx::query_as::<_, NotificationLog>(&statement)
        .bind(&device_id)
        .bind(&notification_type)
        .fetch_all(&mut conn)
        .await?;

    Ok(info)
}

pub(super) async fn create_notification_logs_table(
    conn: &mut SqliteConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        notification_type INTEGER NOT NULL,
        device_id TEXT NOT NULL,
        sent_at INTEGER NOT NULL
    )",
        NOTIFICATION_LOGS_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}
