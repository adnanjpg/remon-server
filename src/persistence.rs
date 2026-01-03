use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Pool, Sqlite};
use tokio::sync::OnceCell;

use crate::config::Config;
use crate::logs::persistence::app_logs::create_app_logs_table;
use crate::logs::persistence::create_notification_logs_table;

#[derive(Debug, sqlx::FromRow)]
pub struct FetchId {
    pub id: i64,
}

pub type SQLConnection = Pool<Sqlite>;

pub async fn get_default_sql_connection() -> Result<SQLConnection, sqlx::Error> {
    get_sql_connection().await
}

pub async fn get_sql_connection() -> Result<SQLConnection, sqlx::Error> {
    let config = Config::new().map_err(|e| sqlx::Error::Configuration(e.into()))?;

    let pool = POOL
        .get_or_try_init(|| async {
            let connection_string = format!("sqlite:{}", config.database.path);
            SqlitePoolOptions::new()
                .max_connections(config.database.max_connections)
                .connect(&connection_string)
                .await
        })
        .await?;

    Ok(pool.clone())
}

// Using tokio::sync::OnceCell for thread-safe lazy initialization
static POOL: OnceCell<Pool<Sqlite>> = OnceCell::const_new();

pub async fn init_db() -> Result<(), sqlx::Error> {
    let config = Config::new().map_err(|e| sqlx::Error::Configuration(e.into()))?;

    // check if db folder exists
    if !std::path::Path::new(&config.database.folder_path).exists() {
        // create db folder
        std::fs::create_dir(&config.database.folder_path).unwrap();
    }
    // if db file not exists, create it
    if !std::path::Path::new(&config.database.path).exists() {
        // create db file
        std::fs::File::create(&config.database.path).unwrap();
    }

    let conn = get_default_sql_connection().await?;

    crate::monitor::persistence::init_db(&conn).await?;
    crate::logs::persistence::init_db(&conn).await?;

    create_notification_logs_table(&conn).await?;
    create_app_logs_table(&conn).await?;

    Ok(())
}
