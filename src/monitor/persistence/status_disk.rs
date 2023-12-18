use sqlx::SqliteConnection;

use crate::monitor::models::get_disk_status::{DiskFrameStatus, SingleDiskInfo};

use super::{get_sql_connection, FetchId, SQLITE_DB_CONN_STR};

const DISK_STATUS_FRAME_TABLE_NAME: &str = "disk_status_frame";
const DISK_STATUS_FRAME_SINGLE_TABLE_NAME: &str = "disk_status_frame_single";

pub async fn insert_disk_status_frame(status: &DiskFrameStatus) -> Result<(), sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let statement = format!(
        "INSERT INTO {} 
        (last_check) 
        VALUES (?)
        RETURNING id
        ",
        DISK_STATUS_FRAME_TABLE_NAME
    );

    let query_res = sqlx::query_as::<_, FetchId>(&statement)
        .bind(&status.last_check)
        .fetch_one(&mut conn)
        .await?;

    let frame_id = query_res.id;

    let mut owned_cores_usage = status.disks_usage.to_owned();
    for core in owned_cores_usage.iter_mut() {
        core.frame_id = frame_id;
        insert_disk_status_frame_single(&core).await?;
    }

    Ok(())
}

async fn insert_disk_status_frame_single(status: &SingleDiskInfo) -> Result<(), sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let statement = format!(
        "INSERT INTO {} (frame_id, disk_id, available) VALUES (?, ?, ?)",
        DISK_STATUS_FRAME_SINGLE_TABLE_NAME
    );
    sqlx::query(&statement)
        .bind(&status.frame_id)
        .bind(&status.disk_id)
        .bind(&status.available)
        .execute(&mut conn)
        .await?;

    Ok(())
}

pub async fn get_disk_status_between_dates(
    start_date: i64,
    end_date: i64,
) -> Result<Vec<DiskFrameStatus>, sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let frames_statement = format!(
        "SELECT id, last_check FROM {} WHERE last_check BETWEEN ? AND ?",
        DISK_STATUS_FRAME_TABLE_NAME
    );
    let frames_query = sqlx::query_as::<_, (i64, i64)>(&frames_statement)
        .bind(&start_date)
        .bind(&end_date)
        .fetch_all(&mut conn)
        .await?;

    let frame_ids = frames_query
        .iter()
        .map(|frame| frame.0.to_string())
        .collect::<Vec<String>>()
        .join(",");
    let singles_statement = format!(
        "SELECT * FROM {} WHERE frame_id IN ({})",
        DISK_STATUS_FRAME_SINGLE_TABLE_NAME, frame_ids
    );

    let singles_query = sqlx::query_as::<_, SingleDiskInfo>(&singles_statement)
        .fetch_all(&mut conn)
        .await?;

    let frames: Vec<DiskFrameStatus> = frames_query
        .iter()
        .map(|frame| {
            let id = frame.0;
            let last_check = frame.1;

            DiskFrameStatus {
                id,
                last_check,
                disks_usage: singles_query
                    .iter()
                    .filter(|f| f.frame_id == id)
                    .map(|s| s.clone())
                    .collect(),
            }
        })
        .collect();

    Ok(frames)
}

pub(super) async fn create_disk_status_frames_table(
    conn: &mut SqliteConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        last_check INTEGER NOT NULL
    )",
        DISK_STATUS_FRAME_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}

pub(super) async fn create_disk_status_frame_singles_table(
    conn: &mut SqliteConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        frame_id INTEGER NOT NULL,
        available INTEGER NOT NULL,
        disk_id TEXT NOT NULL,
        FOREIGN KEY (frame_id)
            REFERENCES {} (id)
    )",
        DISK_STATUS_FRAME_SINGLE_TABLE_NAME, DISK_STATUS_FRAME_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}
