use super::{get_default_sql_connection, SQLConnection};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, sqlx::Type, Clone)]
#[serde(rename_all = "lowercase")]
pub enum NotificationType {
    StatusLimitsExceeding,
    ServiceTest
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct NotificationLog {
    pub id: i64,
    pub notification_type: NotificationType,
    pub device_id: String,
    pub fcm_token: String,
    pub title: String,
    pub body: String,
    pub sent_at: i64,
}

const NOTIFICATION_LOGS_TABLE_NAME: &str = "notification_logs";

pub async fn insert_notification_log(log: &NotificationLog) -> Result<(), sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    let statement = format!(
        "INSERT INTO {} 
        (notification_type, device_id, fcm_token, title, body, sent_at) 
        VALUES (?, ?, ?, ?, ?, ?)",
        NOTIFICATION_LOGS_TABLE_NAME
    );

    sqlx::query(&statement)
        .bind(&log.notification_type)
        .bind(&log.device_id)
        .bind(&log.fcm_token)
        .bind(&log.title)
        .bind(&log.body)
        .bind(&log.sent_at)
        .execute(&conn)
        .await?;

    Ok(())
}

pub async fn fetch_single_latest_for_device_id_and_type(
    device_id: &str,
    notification_type: &NotificationType,
) -> Result<Option<NotificationLog>, sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    let statement = format!(
        "
        SELECT *
        FROM {}
        WHERE device_id = ?
        AND notification_type = ?
        ORDER BY sent_at DESC
        ",
        NOTIFICATION_LOGS_TABLE_NAME
    );
    let info = sqlx::query_as::<_, NotificationLog>(&statement)
        .bind(&device_id)
        .bind(&notification_type)
        .fetch_optional(&conn)
        .await?;

    Ok(info)
}

pub(super) async fn create_notification_logs_table(
    conn: &SQLConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        notification_type INTEGER NOT NULL,
        device_id TEXT NOT NULL,
        fcm_token TEXT NOT NULL,
        title TEXT NOT NULL,
        body TEXT NOT NULL,
        sent_at INTEGER NOT NULL
    )",
        NOTIFICATION_LOGS_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}
