use crate::{
    monitor::models::get_network_status::{NetworkFrameStatus, SingleNetworkInfo},
    persistence::SQLConnection,
};

use super::{get_default_sql_connection, FetchId};

const NETWORK_STATUS_FRAME_TABLE_NAME: &str = "network_status_frame";
const NETWORK_STATUS_FRAME_SINGLE_TABLE_NAME: &str = "network_status_frame_single";

pub async fn insert_network_status_frame(status: &NetworkFrameStatus) -> Result<(), sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    let statement = format!(
        "INSERT INTO {} 
        (last_check) 
        VALUES (?)
        RETURNING id
        ",
        NETWORK_STATUS_FRAME_TABLE_NAME
    );

    let query_res = sqlx::query_as::<_, FetchId>(&statement)
        .bind(&status.last_check)
        .fetch_one(&conn)
        .await?;

    let frame_id = query_res.id;

    let mut owned_interfaces = status.interfaces.to_owned();
    for interface in owned_interfaces.iter_mut() {
        interface.frame_id = frame_id;
        insert_network_status_frame_single(&interface).await?;
    }

    Ok(())
}

async fn insert_network_status_frame_single(status: &SingleNetworkInfo) -> Result<(), sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    let statement = format!(
        "INSERT INTO {} (frame_id, interface_name, rx_bytes, tx_bytes, rx_packets, tx_packets) VALUES (?, ?, ?, ?, ?, ?)",
        NETWORK_STATUS_FRAME_SINGLE_TABLE_NAME
    );
    sqlx::query(&statement)
        .bind(&status.frame_id)
        .bind(&status.interface_name)
        .bind(&status.rx_bytes)
        .bind(&status.tx_bytes)
        .bind(&status.rx_packets)
        .bind(&status.tx_packets)
        .execute(&conn)
        .await?;

    Ok(())
}

pub async fn get_network_status_between_dates(
    start_date: i64,
    end_date: i64,
) -> Result<Vec<NetworkFrameStatus>, sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    let frames_statement = format!(
        "SELECT id, last_check FROM {} WHERE last_check BETWEEN ? AND ?",
        NETWORK_STATUS_FRAME_TABLE_NAME
    );
    let frames_query = sqlx::query_as::<_, (i64, i64)>(&frames_statement)
        .bind(&start_date)
        .bind(&end_date)
        .fetch_all(&conn)
        .await?;

    let frame_ids = frames_query
        .iter()
        .map(|frame| frame.0.to_string())
        .collect::<Vec<String>>()
        .join(",");

    if frame_ids.is_empty() {
        return Ok(vec![]);
    }

    let singles_statement = format!(
        "SELECT * FROM {} WHERE frame_id IN ({})",
        NETWORK_STATUS_FRAME_SINGLE_TABLE_NAME, frame_ids
    );

    let singles_query = sqlx::query_as::<_, SingleNetworkInfo>(&singles_statement)
        .fetch_all(&conn)
        .await?;

    let frames: Vec<NetworkFrameStatus> = frames_query
        .iter()
        .map(|frame| {
            let id = frame.0;
            let last_check = frame.1;

            NetworkFrameStatus {
                id,
                last_check,
                interfaces: singles_query
                    .iter()
                    .filter(|f| f.frame_id == id)
                    .map(|s| s.clone())
                    .collect(),
            }
        })
        .collect();

    Ok(frames)
}

/// Get the latest network status frame (most recent)
pub async fn get_latest_network_status() -> Result<Option<NetworkFrameStatus>, sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    let frame_statement = format!(
        "SELECT id, last_check FROM {} ORDER BY last_check DESC LIMIT 1",
        NETWORK_STATUS_FRAME_TABLE_NAME
    );
    let frame_query = sqlx::query_as::<_, (i64, i64)>(&frame_statement)
        .fetch_optional(&conn)
        .await?;

    match frame_query {
        Some(frame) => {
            let id = frame.0;
            let last_check = frame.1;

            let singles_statement = format!(
                "SELECT * FROM {} WHERE frame_id = ?",
                NETWORK_STATUS_FRAME_SINGLE_TABLE_NAME
            );
            let singles_query = sqlx::query_as::<_, SingleNetworkInfo>(&singles_statement)
                .bind(id)
                .fetch_all(&conn)
                .await?;

            Ok(Some(NetworkFrameStatus {
                id,
                last_check,
                interfaces: singles_query,
            }))
        }
        None => Ok(None),
    }
}

pub(super) async fn create_network_status_frames_table(
    conn: &SQLConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        last_check INTEGER NOT NULL
    )",
        NETWORK_STATUS_FRAME_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}

pub(super) async fn create_network_status_frame_singles_table(
    conn: &SQLConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        interface_name TEXT NOT NULL,
        rx_bytes INTEGER NOT NULL,
        tx_bytes INTEGER NOT NULL,
        rx_packets INTEGER NOT NULL,
        tx_packets INTEGER NOT NULL,
        frame_id INTEGER NOT NULL,
        FOREIGN KEY (frame_id)
            REFERENCES {} (id)
    )",
        NETWORK_STATUS_FRAME_SINGLE_TABLE_NAME, NETWORK_STATUS_FRAME_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}
