use crate::monitor::MonitorConfig;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{ConnectOptions, SqliteConnection};
use std::str::FromStr;

use super::{
    CpuCoreInfo, CpuFrameStatus, DiskFrameStatus, HardwareCpuInfo, HardwareDiskInfo, HardwareInfo,
    MemFrameStatus, SingleDiskInfo, SingleMemInfo,
};

const SQLITE_DBS_FOLDER_PATH: &str = "./db";
const SQLITE_DB_PATH: &str = "./db/monitor.sqlite3";
const SQLITE_DB_CONN_STR: &str = "sqlite:./db/monitor.sqlite3";

const MONITOR_CONFIGS_TABLE_NAME: &str = "configs";

const HARDWARE_CPU_INFOS_TABLE_NAME: &str = "cpu_infos";
const HARDWARE_DISK_INFOS_TABLE_NAME: &str = "disk_infos";

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

pub async fn fetch_monitor_configs() -> Result<Vec<MonitorConfig>, sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let statement = format!("SELECT * FROM {}", MONITOR_CONFIGS_TABLE_NAME);
    let configs = sqlx::query_as::<_, MonitorConfig>(&statement)
        .fetch_all(&mut conn)
        .await?;

    Ok(configs)
}

pub async fn insert_monitor_config(
    config: &MonitorConfig,
    device_id: &str,
) -> Result<(), sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let statement = format!("INSERT OR REPLACE INTO {} (device_id, cpu_threshold, mem_threshold, storage_threshold) VALUES (?, ?, ?, ?)",MONITOR_CONFIGS_TABLE_NAME);
    sqlx::query(&statement)
        .bind(&device_id)
        .bind(config.cpu_threshold)
        .bind(config.mem_threshold)
        .bind(config.storage_threshold)
        .execute(&mut conn)
        .await?;

    Ok(())
}

// hardware info
async fn insert_hardware_cpu_info(info: &HardwareCpuInfo) -> Result<(), sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let statement = format!(
        "INSERT INTO {} (cpu_id, core_count, vendor_id, brand) VALUES (?, ?, ?, ?)",
        HARDWARE_CPU_INFOS_TABLE_NAME
    );
    sqlx::query(&statement)
        .bind(&info.cpu_id)
        .bind(&info.core_count)
        .bind(&info.vendor_id)
        .bind(&info.brand)
        .execute(&mut conn)
        .await?;

    Ok(())
}

async fn insert_hardware_disk_info(info: &HardwareDiskInfo) -> Result<(), sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    let statement = format!(
        "INSERT INTO {} (disk_id, name) VALUES (?, ?)",
        HARDWARE_DISK_INFOS_TABLE_NAME
    );
    sqlx::query(&statement)
        .bind(&info.disk_id)
        .bind(&info.name)
        .execute(&mut conn)
        .await?;

    Ok(())
}

pub async fn insert_hardware_info(status: &HardwareInfo) -> Result<(), sqlx::Error> {
    for cpu_info in status.cpu_info.iter() {
        insert_hardware_cpu_info(cpu_info).await?;
    }

    for disk_info in status.disks_info.iter() {
        insert_hardware_disk_info(disk_info).await?;
    }

    Ok(())
}

async fn fetch_latest_hardware_cpus_info() -> Result<Vec<HardwareCpuInfo>, sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    // get all with distinct core count or distinct vendor_id or distinct brand
    let statement = format!(
        "
SELECT *
FROM {} t1
WHERE EXISTS (
    SELECT 1
    FROM {} t2
    WHERE t1.core_count <> t2.core_count
    OR t1.vendor_id <> t2.vendor_id
    OR t1.brand <> t2.brand
)
ORDER BY t1.core_count, t1.vendor_id, t1.brand
        ",
        HARDWARE_CPU_INFOS_TABLE_NAME, HARDWARE_CPU_INFOS_TABLE_NAME
    );
    let info = sqlx::query_as::<_, HardwareCpuInfo>(&statement)
        .fetch_all(&mut conn)
        .await?;

    Ok(info)
}

async fn fetch_latest_hardware_disks_info() -> Result<Vec<HardwareDiskInfo>, sqlx::Error> {
    let mut conn = get_sql_connection(SQLITE_DB_CONN_STR).await?;

    // get all with distinct name
    let statement = format!(
        "
SELECT *
FROM {} t1
WHERE EXISTS (
    SELECT 1
    FROM {} t2
    WHERE t1.name <> t2.name
)
ORDER BY t1.name
",
        HARDWARE_DISK_INFOS_TABLE_NAME, HARDWARE_DISK_INFOS_TABLE_NAME,
    );

    let info = sqlx::query_as::<_, HardwareDiskInfo>(&statement)
        .fetch_all(&mut conn)
        .await?;

    Ok(info)
}

pub async fn fetch_latest_hardware_info() -> Result<HardwareInfo, sqlx::Error> {
    let cpu_info = fetch_latest_hardware_cpus_info().await?;
    let disks_info = fetch_latest_hardware_disks_info().await?;

    Ok(HardwareInfo {
        cpu_info,
        disks_info,
    })
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
        "INSERT INTO {} (frame_id, disk_id, total, available) VALUES (?, ?, ?, ?)",
        DISK_STATUS_FRAME_SINGLE_TABLE_NAME
    );
    sqlx::query(&statement)
        .bind(&status.frame_id)
        .bind(&status.disk_id)
        .bind(&status.total)
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
        "INSERT INTO {} (frame_id, mem_id, total, available) VALUES (?, ?, ?, ?)",
        MEM_STATUS_FRAME_SINGLE_TABLE_NAME
    );
    sqlx::query(&statement)
        .bind(&status.frame_id)
        .bind(&status.mem_id)
        .bind(&status.total)
        .bind(&status.available)
        .execute(&mut conn)
        .await?;

    Ok(())
}

// init db
async fn create_configs_table(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        device_id TEXT PRIMARY KEY,
        cpu_threshold REAL NOT NULL,
        mem_threshold REAL NOT NULL,
        storage_threshold REAL NOT NULL
    )",
        MONITOR_CONFIGS_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}

async fn create_hardware_cpu_infos_table(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
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

async fn create_hardware_disk_infos_table(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        disk_id TEXT NOT NULL,
        name TEXT NOT NULL,
        last_check INTEGER NOT NULL
    )",
        HARDWARE_DISK_INFOS_TABLE_NAME
    );

    sqlx::query(&statement).execute(conn).await?;

    Ok(())
}

// cpu status
async fn create_cpu_status_frames_table(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    let statement = format!(
        "CREATE TABLE IF NOT EXISTS {} (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        last_check INTEGER NOT NULL,
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
        freq REAL NOT NULL,
        usage REAL NOT NULL,
        frame_id INTEGER NOT NULL,
        FOREIGN KEY (frame_id)
            REFERENCES {} (frame_id) 
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
        last_check INTEGER NOT NULL,
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
        disk_id TEXT NOT NULL,
        total REAL NOT NULL,
        available REAL NOT NULL,
        frame_id INTEGER NOT NULL,
        FOREIGN KEY (frame_id)
            REFERENCES {} (frame_id) 
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
        last_check INTEGER NOT NULL,
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
        total REAL NOT NULL,
        available REAL NOT NULL,
        frame_id INTEGER NOT NULL,
        FOREIGN KEY (frame_id)
            REFERENCES {} (frame_id) 
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

    create_configs_table(&mut conn).await?;

    create_hardware_cpu_infos_table(&mut conn).await?;
    create_hardware_disk_infos_table(&mut conn).await?;

    create_cpu_status_frames_table(&mut conn).await?;
    create_cpu_status_frame_cores_table(&mut conn).await?;

    create_disk_status_frames_table(&mut conn).await?;
    create_disk_status_frame_singles_table(&mut conn).await?;

    create_mem_status_frames_table(&mut conn).await?;
    create_mem_status_frame_singles_table(&mut conn).await?;

    Ok(())
}
