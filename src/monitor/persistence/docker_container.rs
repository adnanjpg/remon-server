use crate::{monitor::models::docker::ContainerInfo, persistence::SQLConnection};

use super::{get_default_sql_connection, FetchId};

const DOCKER_CONTAINERS_TABLE_NAME: &str = "docker_containers";

pub async fn upsert_container_info(container: &ContainerInfo) -> Result<i64, sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    let statement = format!(
        "INSERT INTO {} (container_id, name, image, created_at, last_seen)
        VALUES (?, ?, ?, ?, ?)
        ON CONFLICT(container_id) DO UPDATE SET
            name = excluded.name,
            image = excluded.image,
            last_seen = excluded.last_seen
        RETURNING id",
        DOCKER_CONTAINERS_TABLE_NAME
    );

    let query_res = sqlx::query_as::<_, FetchId>(&statement)
        .bind(&container.container_id)
        .bind(&container.name)
        .bind(&container.image)
        .bind(&container.created_at)
        .bind(&container.last_seen)
        .fetch_one(&conn)
        .await?;

    Ok(query_res.id)
}

pub async fn fetch_all_containers() -> Result<Vec<ContainerInfo>, sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    let statement = format!("SELECT * FROM {}", DOCKER_CONTAINERS_TABLE_NAME);

    let containers = sqlx::query_as::<_, ContainerInfo>(&statement)
        .fetch_all(&conn)
        .await?;

    Ok(containers)
}

pub async fn fetch_container_by_id(container_id: &str) -> Result<Option<ContainerInfo>, sqlx::Error> {
    let conn = get_default_sql_connection().await?;

    let statement = format!(
        "SELECT * FROM {} WHERE container_id = ?",
        DOCKER_CONTAINERS_TABLE_NAME
    );

    let container = sqlx::query_as::<_, ContainerInfo>(&statement)
        .bind(container_id)
        .fetch_optional(&conn)
        .await?;

    Ok(container)
}

pub(super) async fn create_docker_containers_table(
    conn: &SQLConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        container_id TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL,
        image TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        last_seen INTEGER NOT NULL
    )",
        DOCKER_CONTAINERS_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}
