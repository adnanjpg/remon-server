use crate::monitor::{CpuStatus, DiskStatus, MonitorConfig, MonitorStatus};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteRow};
use sqlx::{ConnectOptions, Row};
use sqlx::{Pool, Sqlite};
use std::str::FromStr;

const SQLITE_DB_PATH: &str = "sqlite:./db/monitor.sqlite3";

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

    let qu = sqlx::query(
        "SELECT statuses.mem_usage, statuses.last_check, cpu_statuses.vendor_id, cpu_statuses.brand, disk_statuses.name, disk_statuses.total, disk_statuses.available FROM statuses
        INNER JOIN cpu_statuses ON statuses.cpu_status_id = cpu_statuses.id
        INNER JOIN disk_statuses ON statuses.id = disk_statuses.monitor_status_id
        ORDER BY statuses.last_check DESC
        LIMIT 1",
    ).fetch_one(&mut conn).await?;

    let id: f64 = qu.get("statuses.mem_usage");

    print!("{}", id);

    Ok(MonitorStatus {
        cpu_usage: CpuStatus {
            vendor_id: String::from("vendor_id"),
            brand: String::from("brand"),
            cpu_usage: vec![],
        },
        mem_usage: 1.0,
        storage_usage: vec![],
        last_check: 1,
    })
}

pub async fn insert_monitor_status(status: &MonitorStatus) -> Result<(), sqlx::Error> {
    let mut conn = SqliteConnectOptions::from_str(SQLITE_DB_PATH)
        .unwrap()
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await?;

    // insert dummy
    sqlx::query("INSERT INTO cpu_statuses (vendor_id, brand) VALUES (?, ?)")
        .bind("vendor_id")
        .bind("brand")
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
    if !std::path::Path::new("./db/monitor.sqlite3").exists() {
        // create db file
        std::fs::File::create("./db/monitor.sqlite3").unwrap();
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
        cpu_status_id INTEGER,
        mem_usage REAL NOT NULL,
        last_check INTEGER NOT NULL,
        FOREIGN KEY (cpu_status_id) REFERENCES cpu_statuses(id) ON DELETE CASCADE
    )",
    )
    .execute(&mut conn)
    .await
    .unwrap();

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS cpu_statuses (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        vendor_id TEXT NOT NULL,
        brand TEXT NOT NULL
    )",
    )
    .execute(&mut conn)
    .await
    .unwrap();

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS disk_statuses (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        monitor_status_id INTEGER,
        name TEXT NOT NULL,
        total INTEGER NOT NULL,
        available INTEGER NOT NULL,
        FOREIGN KEY (monitor_status_id) REFERENCES statuses(id) ON DELETE CASCADE
    )",
    )
    .execute(&mut conn)
    .await
    .unwrap();
}
