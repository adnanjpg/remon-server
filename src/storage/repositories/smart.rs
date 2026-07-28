//! SMART disk-health repository — `metrics_smart` reads/writes.
//!
//! Raw-only resource (no rollup): the collector ticks every ~30 min and
//! the values move on the scale of hours, so a year of raw rows per
//! device stays tiny. Retention is seeded as `('smart','raw', 1y)`.

use sqlx::SqlitePool;

use crate::error::AppResult;

/// One device reading, as parsed from `smartctl -a --json` output.
/// ATA-only fields are `None` on NVMe devices and vice versa.
#[derive(Debug, Clone, PartialEq)]
pub struct SmartDeviceRow {
    pub device: String,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub health_passed: Option<bool>,
    pub temperature_c: Option<f64>,
    pub power_on_hours: Option<i64>,
    pub power_cycles: Option<i64>,
    pub reallocated_sectors: Option<i64>,
    pub pending_sectors: Option<i64>,
    pub uncorrectable_sectors: Option<i64>,
    pub udma_crc_errors: Option<i64>,
    pub percentage_used: Option<i64>,
    pub available_spare_percent: Option<i64>,
    pub media_errors: Option<i64>,
}

/// `SmartDeviceRow` + the tick it was recorded at. Read-side shape for
/// `GET /system/smart`.
#[derive(Debug, Clone)]
pub struct SmartLatestRow {
    pub timestamp: i64,
    pub row: SmartDeviceRow,
}

pub struct SmartRepository {
    pool: SqlitePool,
}

impl SmartRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Persist one collector tick. Same-timestamp collisions are dropped
    /// (and logged by the caller's interval floor making them unreachable
    /// in practice), mirroring the other metrics tables.
    pub async fn insert_tick(&self, timestamp: i64, rows: &[SmartDeviceRow]) -> AppResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut qb = sqlx::QueryBuilder::new(
            "INSERT INTO metrics_smart \
             (resolution, timestamp, device, model, serial, health_passed, \
              temperature_c, power_on_hours, power_cycles, \
              reallocated_sectors, pending_sectors, uncorrectable_sectors, \
              udma_crc_errors, percentage_used, available_spare_percent, \
              media_errors) ",
        );
        qb.push_values(rows.iter(), |mut b, r| {
            b.push_bind("raw")
                .push_bind(timestamp)
                .push_bind(&r.device)
                .push_bind(&r.model)
                .push_bind(&r.serial)
                .push_bind(r.health_passed.map(i64::from))
                .push_bind(r.temperature_c)
                .push_bind(r.power_on_hours)
                .push_bind(r.power_cycles)
                .push_bind(r.reallocated_sectors)
                .push_bind(r.pending_sectors)
                .push_bind(r.uncorrectable_sectors)
                .push_bind(r.udma_crc_errors)
                .push_bind(r.percentage_used)
                .push_bind(r.available_spare_percent)
                .push_bind(r.media_errors);
        });
        qb.push(" ON CONFLICT(resolution, timestamp, device) DO NOTHING");
        qb.build().execute(&self.pool).await?;
        Ok(())
    }

    /// Latest reading per device — backs `GET /system/smart`. A device
    /// that disappears (unplugged USB enclosure) stops producing rows but
    /// keeps its last reading until retention ages it out; clients can
    /// treat a stale `timestamp` as "not currently present".
    pub async fn read_latest(&self) -> AppResult<Vec<SmartLatestRow>> {
        let rows = sqlx::query!(
            r#"SELECT timestamp as "timestamp!", device as "device!", model, serial,
                    health_passed, temperature_c, power_on_hours, power_cycles,
                    reallocated_sectors, pending_sectors, uncorrectable_sectors,
                    udma_crc_errors, percentage_used, available_spare_percent,
                    media_errors
               FROM metrics_smart
              WHERE resolution = 'raw'
                AND (device, timestamp) IN (
                  SELECT device, MAX(timestamp)
                    FROM metrics_smart
                   WHERE resolution = 'raw'
                   GROUP BY device
                )
              ORDER BY device ASC"#
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| SmartLatestRow {
                timestamp: r.timestamp,
                row: SmartDeviceRow {
                    device: r.device,
                    model: r.model,
                    serial: r.serial,
                    health_passed: r.health_passed.map(|v| v != 0),
                    temperature_c: r.temperature_c,
                    power_on_hours: r.power_on_hours,
                    power_cycles: r.power_cycles,
                    reallocated_sectors: r.reallocated_sectors,
                    pending_sectors: r.pending_sectors,
                    uncorrectable_sectors: r.uncorrectable_sectors,
                    udma_crc_errors: r.udma_crc_errors,
                    percentage_used: r.percentage_used,
                    available_spare_percent: r.available_spare_percent,
                    media_errors: r.media_errors,
                },
            })
            .collect())
    }
}
