use sqlx::SqliteConnection;

use crate::monitor::HardwareMemInfo;

use super::{get_sql_connection, FetchId, SQLITE_DB_CONN_STR};

const HARDWARE_MEM_INFOS_TABLE_NAME: &str = "mem_infos";

pub(super) async fn insert_hardware_mem_info(info: &HardwareMemInfo) -> Result<(), sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    // check if a record with the same cpu_id already exists
    let exists_record_check = format!(
        "SELECT id FROM {} WHERE mem_id = ?",
        HARDWARE_MEM_INFOS_TABLE_NAME
    );

    let exists_check_res = sqlx::query_as::<_, FetchId>(&exists_record_check)
        .bind(&info.mem_id)
        .fetch_optional(&mut conn)
        .await?;

    // if exists, update it
    match exists_check_res {
        Some(value) => {
            let statement = format!(
                "UPDATE {} 
        SET last_check = ?
        WHERE id = ?",
                HARDWARE_MEM_INFOS_TABLE_NAME
            );

            sqlx::query(&statement)
                .bind(&info.last_check)
                .bind(&value.id)
                .execute(&mut conn)
                .await?;
        }
        None => {
            let statement = format!(
                "INSERT INTO {} (mem_id, total_space, last_check) VALUES (?, ?, ?)",
                HARDWARE_MEM_INFOS_TABLE_NAME
            );
            sqlx::query(&statement)
                .bind(&info.mem_id)
                .bind(&info.total_space)
                .bind(&info.last_check)
                .execute(&mut conn)
                .await?;
        }
    };

    Ok(())
}

pub(super) async fn fetch_latest_hardware_mems_info() -> Result<Vec<HardwareMemInfo>, sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    // get all with distinct mem_id
    let statement = format!(
        "
    SELECT *
    FROM {}
",
        HARDWARE_MEM_INFOS_TABLE_NAME
    );

    let info = sqlx::query_as::<_, HardwareMemInfo>(&statement)
        .fetch_all(&mut conn)
        .await?;

    Ok(info)
}

pub(super) async fn create_hardware_mem_infos_table(
    conn: &mut SqliteConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        mem_id TEXT NOT NULL,
        total_space INTEGER NOT NULL,
        last_check INTEGER NOT NULL
    )",
        HARDWARE_MEM_INFOS_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}
