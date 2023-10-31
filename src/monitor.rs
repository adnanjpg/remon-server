use sysinfo::{CpuExt, System, SystemExt};

use serde::{de, Deserialize, Serialize};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::ConnectOptions;
use std::str::FromStr;

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerDescription {
    name: String,
    description: String,
}

// this is going to be improved later
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct MonitorConfig {
    pub device_id: String,
    pub cpu_threshold: f64,
    pub mem_threshold: f64,
    pub storage_threshold: f64,
}

const LE_DOT: &str = " • ";

const SQLITE_DB_PATH: &str = "sqlite:./db/mon.sqlite3";

pub fn get_default_server_desc() -> ServerDescription {
    let mut system = System::new_all();
    system.refresh_all();

    // TODO: add pc name / user name
    let cpu = system.cpus()[0].brand();
    let mem = (system.total_memory() as f64) / 1024.0 / 1024.0 / 1024.0;
    let name = system.name().unwrap_or("Unknown".to_string())
        + LE_DOT
        + match system.global_cpu_info().vendor_id() {
            "GenuineIntel" => "Intel",
            _ => "Unknown",
        };
    let description = system.long_os_version().unwrap_or("Unknown".to_string())
        + LE_DOT
        + cpu
        + LE_DOT
        + &format!("{:.1}GB", &mem);

    ServerDescription { name, description }
}

/// not tested yet
pub async fn create_table() -> Result<(), sqlx::Error> {
    let mut conn = SqliteConnectOptions::from_str(SQLITE_DB_PATH)
        .unwrap()
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS configs (
            device_id TEXT PRIMARY KEY,
            cpu_threshold REAL NOT NULL,
            mem_threshold REAL NOT NULL,
            storage_threshold REAL NOT NULL
        )",
    )
    .execute(&mut conn)
    .await?;

    Ok(())
}

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
