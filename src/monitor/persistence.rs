use sqlx::SqliteConnection;

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

pub use crate::persistence::{get_default_sql_connection, get_sql_connection, FetchId};

pub async fn init_db(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    create_monitor_configs_table(conn).await?;

    create_hardware_cpu_infos_table(conn).await?;
    create_hardware_disk_infos_table(conn).await?;
    create_hardware_mem_infos_table(conn).await?;

    create_cpu_status_frames_table(conn).await?;
    create_cpu_status_frame_cores_table(conn).await?;

    create_disk_status_frames_table(conn).await?;
    create_disk_status_frame_singles_table(conn).await?;

    create_mem_status_frames_table(conn).await?;
    create_mem_status_frame_singles_table(conn).await?;

    Ok(())
}
