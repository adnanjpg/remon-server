use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{ConnectOptions, SqliteConnection};
use std::str::FromStr;

mod monitor_config;
use self::monitor_config::create_monitor_configs_table;
pub use self::monitor_config::{fetch_monitor_configs, insert_monitor_config};

mod hardware_cpu_info;
use self::hardware_cpu_info::create_hardware_cpu_infos_table;

mod hardware_disk_info;
use self::hardware_disk_info::create_hardware_disk_infos_table;

mod hardware_mem_info;
use self::hardware_mem_info::create_hardware_mem_infos_table;

mod hardware_info;
pub use self::hardware_info::{fetch_latest_hardware_info, insert_hardware_info};

use super::{
    CpuCoreInfo, CpuFrameStatus, DiskFrameStatus, MemFrameStatus, SingleDiskInfo, SingleMemInfo,
};

const SQLITE_DBS_FOLDER_PATH: &str = "./db";
const SQLITE_DB_PATH: &str = "./db/monitor.sqlite3";
const SQLITE_DB_CONN_STR: &str = "sqlite:./db/monitor.sqlite3";

const CPU_STATUS_FRAME_TABLE_NAME: &str = "cpu_status_frame";
const CPU_STATUS_FRAME_CORE_TABLE_NAME: &str = "cpu_status_frame_core";

const DISK_STATUS_FRAME_TABLE_NAME: &str = "disk_status_frame";
const DISK_STATUS_FRAME_SINGLE_TABLE_NAME: &str = "disk_status_frame_single";

const MEM_STATUS_FRAME_TABLE_NAME: &str = "mem_status_frame";
const MEM_STATUS_FRAME_SINGLE_TABLE_NAME: &str = "mem_status_frame_single";

async fn get_sql_connection(db_path: &str) -> Result<SqliteConnection, sqlx::Error> {
    let conn = SqliteConnectOptions::from_str(db_path)
        .unwrap()
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await?;

    Ok(conn)
}

// hardware info

pub async fn get_mem_status_between_dates(
    start_date: i64,
    end_date: i64,
) -> Result<Vec<MemFrameStatus>, sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let frames_statement = format!(
        "SELECT id, last_check FROM {} WHERE last_check BETWEEN ? AND ?",
        MEM_STATUS_FRAME_TABLE_NAME
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
        MEM_STATUS_FRAME_SINGLE_TABLE_NAME, frame_ids
    );

    let singles_query = sqlx::query_as::<_, SingleMemInfo>(&singles_statement)
        .fetch_all(&mut conn)
        .await?;

    let frames: Vec<MemFrameStatus> = frames_query
        .iter()
        .map(|frame| {
            let id = frame.0;
            let last_check = frame.1;

            MemFrameStatus {
                id,
                last_check,
                mems_usage: singles_query
                    .iter()
                    .filter(|f| f.frame_id == id)
                    .map(|s| s.clone())
                    .collect(),
            }
        })
        .collect();

    Ok(frames)
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

pub async fn get_cpu_status_between_dates(
    start_date: i64,
    end_date: i64,
) -> Result<Vec<CpuFrameStatus>, sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let frames_statement = format!(
        "SELECT id, last_check FROM {} WHERE last_check BETWEEN ? AND ?",
        CPU_STATUS_FRAME_TABLE_NAME
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
        CPU_STATUS_FRAME_CORE_TABLE_NAME, frame_ids
    );

    let singles_query = sqlx::query_as::<_, CpuCoreInfo>(&singles_statement)
        .fetch_all(&mut conn)
        .await?;

    let frames: Vec<CpuFrameStatus> = frames_query
        .iter()
        .map(|frame| {
            let id = frame.0;
            let last_check = frame.1;

            CpuFrameStatus {
                id,
                last_check,
                cores_usage: singles_query
                    .iter()
                    .filter(|f| f.frame_id == id)
                    .map(|s| s.clone())
                    .collect(),
            }
        })
        .collect();

    Ok(frames)
}

#[derive(Debug, sqlx::FromRow)]
struct FetchId {
    pub id: i64,
}

// cpu status
pub async fn insert_cpu_status_frame(status: &CpuFrameStatus) -> Result<(), sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let statement = format!(
        "INSERT INTO {} 
        (last_check) 
        VALUES (?)
        RETURNING id
        ",
        CPU_STATUS_FRAME_TABLE_NAME
    );

    let query_res = sqlx::query_as::<_, FetchId>(&statement)
        .bind(&status.last_check)
        .fetch_one(&mut conn)
        .await?;

    let frame_id = query_res.id;

    let mut owned_cores_usage = status.cores_usage.to_owned();
    for core in owned_cores_usage.iter_mut() {
        core.frame_id = frame_id;
        insert_cpu_status_frame_core(&core).await?;
    }

    Ok(())
}

// a single core info of a frame
async fn insert_cpu_status_frame_core(status: &CpuCoreInfo) -> Result<(), sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let statement = format!(
        "INSERT INTO {} (frame_id, cpu_id, freq, usage) VALUES (?, ?, ?, ?)",
        CPU_STATUS_FRAME_CORE_TABLE_NAME
    );
    sqlx::query(&statement)
        .bind(&status.frame_id)
        .bind(&status.cpu_id)
        .bind(&status.freq)
        .bind(&status.usage)
        .execute(&mut conn)
        .await?;

    Ok(())
}

// disk status
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

// a single core info of a frame
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

// mem status
pub async fn insert_mem_status_frame(status: &MemFrameStatus) -> Result<(), sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let statement = format!(
        "INSERT INTO {} 
        (last_check) 
        VALUES (?)
        RETURNING id
        ",
        MEM_STATUS_FRAME_TABLE_NAME
    );

    let query_res = sqlx::query_as::<_, FetchId>(&statement)
        .bind(&status.last_check)
        .fetch_one(&mut conn)
        .await?;

    let frame_id = query_res.id;

    let mut owned_singles_usage = status.mems_usage.to_owned();
    for single in owned_singles_usage.iter_mut() {
        single.frame_id = frame_id;
        insert_mem_status_frame_single(&single).await?;
    }

    Ok(())
}

// a single core info of a frame
async fn insert_mem_status_frame_single(status: &SingleMemInfo) -> Result<(), sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let statement = format!(
        "INSERT INTO {} (frame_id, mem_id, available) VALUES (?, ?, ?)",
        MEM_STATUS_FRAME_SINGLE_TABLE_NAME
    );
    sqlx::query(&statement)
        .bind(&status.frame_id)
        .bind(&status.mem_id)
        .bind(&status.available)
        .execute(&mut conn)
        .await?;

    Ok(())
}

// init db

// cpu status
async fn create_cpu_status_frames_table(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        last_check INTEGER NOT NULL
    )",
        CPU_STATUS_FRAME_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}
async fn create_cpu_status_frame_cores_table(
    conn: &mut SqliteConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        cpu_id TEXT NOT NULL,
        freq INTEGER NOT NULL,
        usage INTEGER NOT NULL,
        frame_id INTEGER NOT NULL,
        FOREIGN KEY (frame_id)
            REFERENCES {} (id)
    )",
        CPU_STATUS_FRAME_CORE_TABLE_NAME, CPU_STATUS_FRAME_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}

// disk status
async fn create_disk_status_frames_table(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
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
async fn create_disk_status_frame_singles_table(
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

// mem status
async fn create_mem_status_frames_table(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        last_check INTEGER NOT NULL
    )",
        MEM_STATUS_FRAME_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}
async fn create_mem_status_frame_singles_table(
    conn: &mut SqliteConnection,
) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        mem_id TEXT NOT NULL,
        available INTEGER NOT NULL,
        frame_id INTEGER NOT NULL,
        FOREIGN KEY (frame_id)
            REFERENCES {} (id)
    )",
        MEM_STATUS_FRAME_SINGLE_TABLE_NAME, MEM_STATUS_FRAME_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
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
