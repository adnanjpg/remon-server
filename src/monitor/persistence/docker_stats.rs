use crate::{
    monitor::models::docker::{ContainerStats, DockerStatsFrame},
    persistence::SQLConnection,
};

use super::{get_default_sql_connection, FetchId};

const DOCKER_STATS_FRAME_TABLE_NAME: &str = "docker_stats_frame";
const DOCKER_STATS_CONTAINER_TABLE_NAME: &str = "docker_stats_container";

pub async fn insert_docker_stats_frame(frame: &DockerStatsFrame) -> Result<(), sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    let statement = format!(
        "INSERT INTO {}
        (last_check)
        VALUES (?)
        RETURNING id",
        DOCKER_STATS_FRAME_TABLE_NAME
    );

    let query_res = sqlx::query_as::<_, FetchId>(&statement)
        .bind(&frame.last_check)
        .fetch_one(&conn)
        .await?;

    let frame_id = query_res.id;

    let mut owned_stats = frame.container_stats.to_owned();
    for stat in owned_stats.iter_mut() {
        stat.frame_id = frame_id;
        insert_docker_stats_container(stat).await?;
    }

    Ok(())
}

async fn insert_docker_stats_container(stats: &ContainerStats) -> Result<(), sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    let statement = format!(
        "INSERT INTO {} (frame_id, container_id, cpu_percent, memory_usage, memory_limit, network_rx_bytes, network_tx_bytes)
        VALUES (?, ?, ?, ?, ?, ?, ?)",
        DOCKER_STATS_CONTAINER_TABLE_NAME
    );

    sqlx::query(&statement)
        .bind(&stats.frame_id)
        .bind(&stats.container_id)
        .bind(&stats.cpu_percent)
        .bind(&stats.memory_usage)
        .bind(&stats.memory_limit)
        .bind(&stats.network_rx_bytes)
        .bind(&stats.network_tx_bytes)
        .execute(&conn)
        .await?;

    Ok(())
}

pub async fn get_docker_stats_between_dates(
    start_date: i64,
    end_date: i64,
) -> Result<Vec<DockerStatsFrame>, sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    let frames_statement = format!(
        "SELECT id, last_check FROM {} WHERE last_check BETWEEN ? AND ?",
        DOCKER_STATS_FRAME_TABLE_NAME
    );

    let frames_query = sqlx::query_as::<_, (i64, i64)>(&frames_statement)
        .bind(&start_date)
        .bind(&end_date)
        .fetch_all(&conn)
        .await?;

    if frames_query.is_empty() {
        return Ok(vec![]);
    }

    let frame_ids = frames_query
        .iter()
        .map(|frame| frame.0.to_string())
        .collect::<Vec<String>>()
        .join(",");

    let stats_statement = format!(
        "SELECT * FROM {} WHERE frame_id IN ({})",
        DOCKER_STATS_CONTAINER_TABLE_NAME, frame_ids
    );

    let stats_query = sqlx::query_as::<_, ContainerStats>(&stats_statement)
        .fetch_all(&conn)
        .await?;

    let frames: Vec<DockerStatsFrame> = frames_query
        .iter()
        .map(|frame| {
            let id = frame.0;
            let last_check = frame.1;

            DockerStatsFrame {
                id,
                last_check,
                container_stats: stats_query
                    .iter()
                    .filter(|s| s.frame_id == id)
                    .cloned()
                    .collect(),
            }
        })
        .collect();

    Ok(frames)
}

pub(super) async fn create_docker_stats_frames_table(
    conn: &SQLConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        last_check INTEGER NOT NULL
    )",
        DOCKER_STATS_FRAME_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}

pub(super) async fn create_docker_stats_container_table(
    conn: &SQLConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        frame_id INTEGER NOT NULL,
        container_id TEXT NOT NULL,
        cpu_percent REAL NOT NULL,
        memory_usage INTEGER NOT NULL,
        memory_limit INTEGER NOT NULL,
        network_rx_bytes INTEGER NOT NULL,
        network_tx_bytes INTEGER NOT NULL,
        FOREIGN KEY (frame_id)
            REFERENCES {} (id)
    )",
        DOCKER_STATS_CONTAINER_TABLE_NAME, DOCKER_STATS_FRAME_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}
