use sqlx::SqliteConnection;

use crate::monitor::models::get_hardware_info::HardwareCpuInfo;

use super::{get_sql_connection, FetchId, SQLITE_DB_CONN_STR};

const HARDWARE_CPU_INFOS_TABLE_NAME: &str = "cpu_infos";

pub(super) async fn insert_hardware_cpu_info(info: &HardwareCpuInfo) -> Result<(), sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    // check if a record with the same cpu_id already exists
    let exists_record_check = format!(
        "SELECT id FROM {} WHERE cpu_id = ?",
        HARDWARE_CPU_INFOS_TABLE_NAME
    );
    let exists_check_res = sqlx::query_as::<_, FetchId>(&exists_record_check)
        .bind(&info.cpu_id)
        .fetch_optional(&mut conn)
        .await?;

    // if exists, update it
    match exists_check_res {
        Some(value) => {
            let statement = format!(
                "UPDATE {} 
        SET last_check = ?
        WHERE id = ?",
                HARDWARE_CPU_INFOS_TABLE_NAME
            );

            sqlx::query(&statement)
                .bind(&info.last_check)
                .bind(&value.id)
                .execute(&mut conn)
                .await?;
        }
        None => {
            let statement = format!(
                "INSERT INTO {} 
        (cpu_id, core_count, vendor_id, brand, last_check) 
        VALUES (?, ?, ?, ?, ?)",
                HARDWARE_CPU_INFOS_TABLE_NAME
            );

            sqlx::query(&statement)
                .bind(&info.cpu_id)
                .bind(&info.core_count)
                .bind(&info.vendor_id)
                .bind(&info.brand)
                .bind(&info.last_check)
                .execute(&mut conn)
                .await?;
        }
    };

    Ok(())
}

pub(super) async fn fetch_latest_hardware_cpus_info() -> Result<Vec<HardwareCpuInfo>, sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let statement = format!(
        "
        SELECT *
        FROM {}
        ",
        HARDWARE_CPU_INFOS_TABLE_NAME
    );
    let info = sqlx::query_as::<_, HardwareCpuInfo>(&statement)
        .fetch_all(&mut conn)
        .await?;

    Ok(info)
}

pub(super) async fn create_hardware_cpu_infos_table(
    conn: &mut SqliteConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        cpu_id TEXT NOT NULL,
        core_count INTEGER NOT NULL,
        vendor_id TEXT NOT NULL,
        brand TEXT NOT NULL,
        last_check INTEGER NOT NULL
    )",
        HARDWARE_CPU_INFOS_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}
