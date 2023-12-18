use sqlx::SqliteConnection;

use crate::monitor::models::get_hardware_info::HardwareDiskInfo;

use super::{get_sql_connection, FetchId, SQLITE_DB_CONN_STR};

const HARDWARE_DISK_INFOS_TABLE_NAME: &str = "disk_infos";

pub(super) async fn insert_hardware_disk_info(info: &HardwareDiskInfo) -> Result<(), sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    // check if a record with the same cpu_id already exists
    let exists_record_check = format!(
        "SELECT id FROM {} WHERE disk_id = ?",
        HARDWARE_DISK_INFOS_TABLE_NAME
    );

    let exists_check_res = sqlx::query_as::<_, FetchId>(&exists_record_check)
        .bind(&info.disk_id)
        .fetch_optional(&mut conn)
        .await?;

    // if exists, update it
    match exists_check_res {
        Some(value) => {
            let statement = format!(
                "UPDATE {} 
        SET last_check = ?
        WHERE id = ?",
                HARDWARE_DISK_INFOS_TABLE_NAME
            );

            sqlx::query(&statement)
                .bind(&info.last_check)
                .bind(&value.id)
                .execute(&mut conn)
                .await?;
        }
        None => {
            let statement = format!(
                "INSERT INTO {} (disk_id, name, fs_type, kind, is_removable, mount_point, total_space, last_check) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                HARDWARE_DISK_INFOS_TABLE_NAME
            );
            sqlx::query(&statement)
                .bind(&info.disk_id)
                .bind(&info.name)
                .bind(&info.fs_type)
                .bind(&info.kind)
                .bind(&info.is_removable)
                .bind(&info.mount_point)
                .bind(&info.total_space)
                .bind(&info.last_check)
                .execute(&mut conn)
                .await?;
        }
    };

    Ok(())
}

pub(super) async fn fetch_latest_hardware_disks_info() -> Result<Vec<HardwareDiskInfo>, sqlx::Error>
{
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    // get all with distinct disk_id
    let statement = format!(
        "
    SELECT *
    FROM {}
",
        HARDWARE_DISK_INFOS_TABLE_NAME
    );

    let info = sqlx::query_as::<_, HardwareDiskInfo>(&statement)
        .fetch_all(&mut conn)
        .await?;

    Ok(info)
}

pub(super) async fn create_hardware_disk_infos_table(
    conn: &mut SqliteConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        disk_id TEXT NOT NULL,
        name TEXT NOT NULL,
        fs_type TEXT NOT NULL,
        kind TEXT NOT NULL,
        is_removable INTEGER NOT NULL,
        mount_point TEXT NOT NULL,
        total_space INTEGER NOT NULL,
        last_check INTEGER NOT NULL
    )",
        HARDWARE_DISK_INFOS_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}
