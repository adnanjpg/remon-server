use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::ConnectOptions;
use std::str::FromStr;

use crate::monitor::sys_status::{MonitorConfig, ServerDescription, MonitorStatus};

const SQLITE_DB_PATH: &str = "sqlite:./db/mon.sqlite3";

pub async fn fetch_monitor_config(device_id: &str) -> Result<MonitorConfig, sqlx::Error> {
    let mut conn = SqliteConnectOptions::from_str(SQLITE_DB_PATH)
        .unwrap()
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await?;

    let config = sqlx::query_as::<_, MonitorConfig>("SELECT * FROM configs WHERE device_id = ?")
        .bind(device_id)
        .fetch_one(&mut conn)
        .await?;

    Ok(config)
}

pub async fn fetch_monitor_configs() -> Result<Vec<MonitorConfig>, sqlx::Error> {
    let mut conn = SqliteConnectOptions::from_str(SQLITE_DB_PATH)
        .unwrap()
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await?;

    let configs = sqlx::query_as::<_, MonitorConfig>("SELECT * FROM configs")
        .fetch_all(&mut conn)
        .await?;

    Ok(configs)
}

pub async fn insert_monitor_config(config: &MonitorConfig) -> Result<(), sqlx::Error> {
    let mut conn = SqliteConnectOptions::from_str(SQLITE_DB_PATH)
        .unwrap()
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await?;

    sqlx::query("INSERT OR REPLACE INTO configs (device_id, cpu_threshold, mem_threshold, storage_threshold) VALUES (?, ?, ?, ?)")
        .bind(&config.device_id)
        .bind(config.cpu_threshold)
        .bind(config.mem_threshold)
        .bind(config.storage_threshold)
        .execute(&mut conn)
        .await?;

    Ok(())
}

pub async fn fetch_monitor_status() -> Result<MonitorStatus, sqlx::Error> {
    let mut conn = SqliteConnectOptions::from_str(SQLITE_DB_PATH)
        .unwrap()
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await?;

    let status = sqlx::query_as::<_, MonitorStatus>("SELECT * FROM statuses ORDER BY id DESC LIMIT 1")
        .fetch_one(&mut conn)
        .await?;

    Ok(status)
}

pub async fn insert_monitor_status(status: &MonitorStatus) -> Result<(), sqlx::Error> {
    let mut conn = SqliteConnectOptions::from_str(SQLITE_DB_PATH)
        .unwrap()
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await?;

    sqlx::query("INSERT OR REPLACE INTO statuses (cpu_usage, mem_usage, storage_usage, last_check) VALUES (?, ?, ?, ?)")
        .bind(status.cpu_usage)
        .bind(status.mem_usage)
        .bind(status.storage_usage)
        .bind(status.last_check)
        .execute(&mut conn)
        .await?;

    Ok(())
}

pub async fn init_db() {
    // check if db folder exists
    if !std::path::Path::new("./db").exists() {
        // create db folder
        std::fs::create_dir("./db").unwrap();
    }
    // if db file not exists, create it
    if !std::path::Path::new("./db/mon.sqlite3").exists() {
        // create db file
        std::fs::File::create("./db/mon.sqlite3").unwrap();
    }

    let mut conn = SqliteConnectOptions::from_str(SQLITE_DB_PATH)
        .unwrap()
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS configs (
        device_id TEXT PRIMARY KEY,
        cpu_threshold REAL NOT NULL,
        mem_threshold REAL NOT NULL,
        storage_threshold REAL NOT NULL
    )",
    )
    .execute(&mut conn)
    .await
    .unwrap();

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS statuses (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        cpu_usage REAL NOT NULL,
        mem_usage REAL NOT NULL,
        storage_usage REAL NOT NULL,
        last_check INTEGER NOT NULL
    )",
    )
    .execute(&mut conn)
    .await
    .unwrap();
}
