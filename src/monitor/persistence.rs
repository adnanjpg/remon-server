use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{ConnectOptions, SqliteConnection};
use std::str::FromStr;

mod monitor_config;
use self::monitor_config::create_monitor_configs_table;
pub use self::monitor_config::{fetch_monitor_configs, insert_or_update_monitor_config};

mod hardware_cpu_info;
use self::hardware_cpu_info::create_hardware_cpu_infos_table;

mod hardware_disk_info;
use self::hardware_disk_info::create_hardware_disk_infos_table;

mod hardware_mem_info;
use self::hardware_mem_info::create_hardware_mem_infos_table;

mod hardware_info;
pub use self::hardware_info::{fetch_latest_hardware_info, insert_hardware_info};

mod status_cpu;
use self::status_cpu::{create_cpu_status_frame_cores_table, create_cpu_status_frames_table};
pub use self::status_cpu::{get_cpu_status_between_dates, insert_cpu_status_frame};

mod status_disk;
use self::status_disk::{create_disk_status_frame_singles_table, create_disk_status_frames_table};
pub use self::status_disk::{get_disk_status_between_dates, insert_disk_status_frame};

mod status_mem;
use self::status_mem::{create_mem_status_frame_singles_table, create_mem_status_frames_table};
pub use self::status_mem::{get_mem_status_between_dates, insert_mem_status_frame};

const SQLITE_DBS_FOLDER_PATH: &str = "./db";
const SQLITE_DB_PATH: &str = "./db/monitor.sqlite3";
const SQLITE_DB_CONN_STR: &str = "sqlite:./db/monitor.sqlite3";

#[derive(Debug, sqlx::FromRow)]
struct FetchId {
    pub id: i64,
}

async fn get_sql_connection(db_path: &str) -> Result<SqliteConnection, sqlx::Error> {
    let conn = SqliteConnectOptions::from_str(db_path)
        .unwrap()
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await?;

    Ok(conn)
}

pub async fn init_db() -> Result<(), sqlx::Error> {
    // check if db folder exists
    if !std::path::Path::new(SQLITE_DBS_FOLDER_PATH).exists() {
        // create db folder
        std::fs::create_dir(SQLITE_DBS_FOLDER_PATH).unwrap();
    }
    // if db file not exists, create it
    if !std::path::Path::new(SQLITE_DB_PATH).exists() {
        // create db file
        std::fs::File::create(SQLITE_DB_PATH).unwrap();
    }

    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    create_monitor_configs_table(&mut conn).await?;

    create_hardware_cpu_infos_table(&mut conn).await?;
    create_hardware_disk_infos_table(&mut conn).await?;
    create_hardware_mem_infos_table(&mut conn).await?;

    create_cpu_status_frames_table(&mut conn).await?;
    create_cpu_status_frame_cores_table(&mut conn).await?;

    create_disk_status_frames_table(&mut conn).await?;
    create_disk_status_frame_singles_table(&mut conn).await?;

    create_mem_status_frames_table(&mut conn).await?;
    create_mem_status_frame_singles_table(&mut conn).await?;

    Ok(())
}
