use async_once::AsyncOnce;
use log::error;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Pool, Sqlite};

pub mod notification_logs;

const SQLITE_DBS_FOLDER_PATH: &str = "./db";
const SQLITE_DB_PATH: &str = "./db/monitor.sqlite3";
const SQLITE_DB_CONN_STR: &str = "sqlite:./db/monitor.sqlite3";

#[derive(Debug, sqlx::FromRow)]
pub struct FetchId {
    pub id: i64,
}

pub type SQLConnection = Pool<Sqlite>;

pub async fn get_default_sql_connection() -> Result<SQLConnection, sqlx::Error> {
    get_sql_connection().await
}
pub async fn get_sql_connection() -> Result<SQLConnection, sqlx::Error> {
    let pool = POOL.get().await;

    match pool {
        Ok(pool) => {
            let oww = pool.to_owned();

            Ok(oww)
        }
        Err(e) => {
            error!("Failed to get db connection from pool: {:?}", e);

            // TODO(adnanjpg): we can't clone sqlx::Error for some reason
            Err(sqlx::Error::PoolClosed)
        }
    }
}

// https://github.com/brettwooldridge/HikariCP/wiki/About-Pool-Sizing
// https://www.cockroachlabs.com/blog/what-is-connection-pooling/
// the info from these two links tells us that
// - we should have a pool IF we have a lot of queries happening, which we do
// - a lot of queries means a lot of connections, which has open and close overhead
// - so this means we should have a pool
// - as for the size of the pool, we should remember that each connection is a thread
// - and maintaining a lot of threads is expensive
// - as our _current_ queries are all small, a single connection should be enough
// TODO(adnanjpg): make this configurable
const MAX_CONNECTIONS: u32 = 1;

// https://stackoverflow.com/a/67758135/12555423
lazy_static! {
    static ref POOL: AsyncOnce<Result<SQLConnection, sqlx::Error>> = AsyncOnce::new(async {
        let con = SqlitePoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .connect(SQLITE_DB_CONN_STR);

        let pool = con.await;

        pool
    });
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

    let conn = get_default_sql_connection().await?;

    crate::monitor::persistence::init_db(&conn).await?;

    notification_logs::create_notification_logs_table(&conn).await?;

    Ok(())
}
