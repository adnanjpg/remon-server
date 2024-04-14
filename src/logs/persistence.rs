pub mod app_logs;

use crate::persistence::SQLConnection;
pub use crate::persistence::{get_default_sql_connection, get_sql_connection, FetchId};

use self::app_logs::create_app_logs_table;
pub use self::app_logs::{get_app_ids, insert_app_log, AppLog, LogLevel};

mod notification_logs;
use self::notification_logs::create_notification_logs_table;
pub use self::notification_logs::{
    fetch_single_latest_for_device_id_and_type, insert_notification_log, NotificationLog,
    NotificationType,
};

pub async fn init_db(conn: &SQLConnection) -> Result<(), sqlx::Error> {
    create_app_logs_table(conn).await?;
    create_notification_logs_table(conn).await?;

    Ok(())
}
