use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{ConnectOptions, SqliteConnection};
use std::str::FromStr;

const SQLITE_DBS_FOLDER_PATH: &str = "./db";
const SQLITE_DB_PATH: &str = "./db/monitor.sqlite3";
const SQLITE_DB_CONN_STR: &str = "sqlite:./db/monitor.sqlite3";

#[derive(Debug, sqlx::FromRow)]
pub struct FetchId {
    pub id: i64,
}

pub async fn get_default_sql_connection() -> Result<SqliteConnection, sqlx::Error> {
    get_sql_connection(SQLITE_DB_CONN_STR).await
}
pub async fn get_sql_connection(db_path: &str) -> Result<SqliteConnection, sqlx::Error> {
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

    crate::monitor::persistence::init_db(&mut conn).await?;

    Ok(())
}
