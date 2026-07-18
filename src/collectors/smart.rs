//! SMART disk-health collector — a thin wrapper around `smartctl`.
//!
//! Nobody reimplements SMART ioctls: ATA vs NVMe command sets, USB-SATA
//! bridge translation, and per-vendor raw-value quirks are exactly what
//! smartmontools already solved on every platform we target. So, like
//! Scrutiny and Netdata, we shell out to `smartctl --json` and parse.
//!
//! Flow per tick:
//!   1. `smartctl --scan --json`          → device list
//!   2. `smartctl -a --json=c -n standby -d <type> <dev>` per device
//!   3. parse → one `SmartDeviceRow` per responsive device → insert
//!
//! `-n standby` is load-bearing: a sleeping HDD would be spun up by a
//! SMART read. A standby device answers with no `smart_status` block;
//! we skip it for the tick and keep its last persisted reading.
//!
//! When the binary is missing the task logs once and exits — hosts
//! without smartmontools pay nothing and `GET /system/smart` reports
//! `available: false`.

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::Utc;
use log::{debug, info, warn};
use serde_json::Value;
use tokio::process::Command;

use crate::config::SmartConfig;
use crate::state::AppState;
use crate::storage::repositories::{SmartDeviceRow, SmartRepository};

/// Hard floor on the poll interval — each tick issues real commands to
/// every disk, so sub-minute cadences are never sensible.
const MIN_INTERVAL_MS: u64 = 60_000;

fn effective_interval_ms(state: &AppState) -> u64 {
    state
        .collector_smart_interval_ms
        .load(Ordering::Relaxed)
        .max(MIN_INTERVAL_MS)
}
/// Timeout per smartctl invocation. A hung USB bridge must not stall
/// the whole scan.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);

pub fn spawn(state: Arc<AppState>, cfg: SmartConfig) {
    if !cfg.enabled {
        info!("SMART collector disabled by config");
        return;
    }
    tokio::spawn(async move { run(state, cfg).await });
}

async fn run(state: Arc<AppState>, cfg: SmartConfig) {
    let bin = if cfg.smartctl_path.trim().is_empty() {
        "smartctl".to_string()
    } else {
        cfg.smartctl_path.trim().to_string()
    };

    // Probe for the binary once. Absence is a supported configuration,
    // not an error loop.
    match run_smartctl(&bin, &["--version"]).await {
        Ok(_) => {
            state.smart_available.store(true, Ordering::Relaxed);
            info!("SMART collector: smartctl found ('{}')", bin);
        }
        Err(e) => {
            info!(
                "SMART collector: smartctl not available ('{}': {}); disk health \
                 monitoring off, install smartmontools to enable",
                bin, e
            );
            return;
        }
    }

    let repo = SmartRepository::new(state.db.clone());
    // Interval lives in runtime config (PATCH /config); re-read after every
    // tick and rebuild the ticker when it changed — same pattern as the
    // rollup/retention workers.
    let mut current_interval_ms = effective_interval_ms(&state);
    let mut ticker = tokio::time::interval(Duration::from_millis(current_interval_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        let new_interval_ms = effective_interval_ms(&state);
        if new_interval_ms != current_interval_ms {
            current_interval_ms = new_interval_ms;
            let next = tokio::time::Instant::now() + Duration::from_millis(current_interval_ms);
            ticker = tokio::time::interval_at(next, Duration::from_millis(current_interval_ms));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        }

        let rows = match collect_once(&bin).await {
            Ok(r) => r,
            Err(e) => {
                warn!("SMART collector: scan failed: {}", e);
                continue;
            }
        };
        if rows.is_empty() {
            debug!("SMART collector: no responsive devices this tick");
            continue;
        }

        let now = Utc::now().timestamp();
        if let Err(e) = repo.insert_tick(now, &rows).await {
            warn!("SMART collector: persist failed: {:?}", e);
        } else {
            debug!("SMART collector: stored {} device reading(s)", rows.len());
        }
    }
}

/// One full pass: scan, then query each device. Per-device failures are
/// logged and skipped — one dead bridge must not hide the other disks.
async fn collect_once(bin: &str) -> Result<Vec<SmartDeviceRow>, String> {
    let scan_out = run_smartctl(bin, &["--scan", "--json"]).await?;
    let scan: Value =
        serde_json::from_str(&scan_out).map_err(|e| format!("scan JSON parse: {}", e))?;

    let devices = parse_scan(&scan);
    let mut rows = Vec::with_capacity(devices.len());
    for (name, dev_type) in devices {
        let args = ["-a", "--json=c", "-n", "standby", "-d", &dev_type, &name];
        // smartctl uses a nonzero exit bitmask even on useful output
        // (e.g. bit 3 = "disk failing" — exactly when we want the data),
        // so parse stdout regardless and let content decide.
        let out = match run_smartctl(bin, &args).await {
            Ok(o) => o,
            Err(e) => {
                warn!("SMART collector: query failed for {}: {}", name, e);
                continue;
            }
        };
        let json: Value = match serde_json::from_str(&out) {
            Ok(v) => v,
            Err(e) => {
                warn!("SMART collector: bad JSON from {}: {}", name, e);
                continue;
            }
        };
        match parse_device(&name, &json) {
            Some(row) => rows.push(row),
            None => debug!(
                "SMART collector: {} in standby or not reporting, skipped",
                name
            ),
        }
    }
    Ok(rows)
}

/// Run smartctl, capturing stdout. Errors on spawn failure or timeout;
/// nonzero exit with output is fine (see caller).
async fn run_smartctl(bin: &str, args: &[&str]) -> Result<String, String> {
    let child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    let out = tokio::time::timeout(CMD_TIMEOUT, child)
        .await
        .map_err(|_| format!("timed out after {:?}", CMD_TIMEOUT))?
        .map_err(|e| e.to_string())?;

    if out.stdout.is_empty() {
        return Err(format!("no output (exit: {:?})", out.status.code()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ===== Parsing =====
//
// smartctl's JSON is large and vendor-variable, so we navigate a
// `serde_json::Value` and pick fields defensively instead of modelling
// the whole document.

/// `--scan --json` → `[(device_name, device_type)]`. The scan-reported
/// type is fed back via `-d` per smartmontools guidance (it encodes the
/// bridge/protocol needed to reach the device).
fn parse_scan(v: &Value) -> Vec<(String, String)> {
    v["devices"]
        .as_array()
        .map(|devs| {
            devs.iter()
                .filter_map(|d| {
                    let name = d["name"].as_str()?.to_string();
                    let typ = d["type"].as_str().unwrap_or("auto").to_string();
                    Some((name, typ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `-a --json` for one device → row. Returns `None` when the device is
/// in standby / did not produce a SMART verdict (no `smart_status`).
fn parse_device(device: &str, v: &Value) -> Option<SmartDeviceRow> {
    let health_passed = v["smart_status"]["passed"].as_bool()?;

    let nvme = &v["nvme_smart_health_information_log"];

    // ATA temperature lives at `temperature.current`; NVMe duplicates it
    // there too in recent smartctl, but fall back to the NVMe log.
    let temperature_c = v["temperature"]["current"]
        .as_f64()
        .or_else(|| nvme["temperature"].as_f64());

    let power_on_hours = v["power_on_time"]["hours"]
        .as_i64()
        .or_else(|| nvme["power_on_hours"].as_i64());
    let power_cycles = v["power_cycle_count"]
        .as_i64()
        .or_else(|| nvme["power_cycles"].as_i64());

    Some(SmartDeviceRow {
        device: device.to_string(),
        model: v["model_name"].as_str().map(str::to_string),
        serial: v["serial_number"].as_str().map(str::to_string),
        health_passed: Some(health_passed),
        temperature_c,
        power_on_hours,
        power_cycles,
        reallocated_sectors: ata_raw_attr(v, 5),
        pending_sectors: ata_raw_attr(v, 197),
        uncorrectable_sectors: ata_raw_attr(v, 198),
        udma_crc_errors: ata_raw_attr(v, 199),
        percentage_used: nvme["percentage_used"].as_i64(),
        available_spare_percent: nvme["available_spare"].as_i64(),
        media_errors: nvme["media_errors"].as_i64(),
    })
}

/// Raw value of one ATA SMART attribute by id, e.g. 5 = Reallocated_Sector_Ct.
fn ata_raw_attr(v: &Value, id: i64) -> Option<i64> {
    v["ata_smart_attributes"]["table"]
        .as_array()?
        .iter()
        .find(|attr| attr["id"].as_i64() == Some(id))
        .and_then(|attr| attr["raw"]["value"].as_i64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_parses_devices() {
        let v: Value = serde_json::from_str(
            r#"{"devices":[
                {"name":"/dev/sda","type":"sat","protocol":"ATA"},
                {"name":"/dev/nvme0","type":"nvme","protocol":"NVMe"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            parse_scan(&v),
            vec![
                ("/dev/sda".to_string(), "sat".to_string()),
                ("/dev/nvme0".to_string(), "nvme".to_string())
            ]
        );
    }

    #[test]
    fn scan_empty_or_missing_is_empty() {
        assert!(parse_scan(&serde_json::json!({})).is_empty());
        assert!(parse_scan(&serde_json::json!({"devices": []})).is_empty());
    }

    #[test]
    fn ata_device_parses() {
        let v: Value = serde_json::from_str(
            r#"{
                "model_name": "WDC WD40EFRX-68N32N0",
                "serial_number": "WD-WCC7K1234567",
                "smart_status": {"passed": true},
                "temperature": {"current": 34},
                "power_on_time": {"hours": 21345},
                "power_cycle_count": 87,
                "ata_smart_attributes": {"table": [
                    {"id": 5,   "name": "Reallocated_Sector_Ct",   "raw": {"value": 0}},
                    {"id": 194, "name": "Temperature_Celsius",     "raw": {"value": 34}},
                    {"id": 197, "name": "Current_Pending_Sector",  "raw": {"value": 2}},
                    {"id": 198, "name": "Offline_Uncorrectable",   "raw": {"value": 0}},
                    {"id": 199, "name": "UDMA_CRC_Error_Count",    "raw": {"value": 13}}
                ]}
            }"#,
        )
        .unwrap();
        let row = parse_device("/dev/sda", &v).expect("should parse");
        assert_eq!(row.device, "/dev/sda");
        assert_eq!(row.model.as_deref(), Some("WDC WD40EFRX-68N32N0"));
        assert_eq!(row.health_passed, Some(true));
        assert_eq!(row.temperature_c, Some(34.0));
        assert_eq!(row.power_on_hours, Some(21345));
        assert_eq!(row.power_cycles, Some(87));
        assert_eq!(row.reallocated_sectors, Some(0));
        assert_eq!(row.pending_sectors, Some(2));
        assert_eq!(row.uncorrectable_sectors, Some(0));
        assert_eq!(row.udma_crc_errors, Some(13));
        // NVMe-only fields stay None on ATA.
        assert_eq!(row.percentage_used, None);
        assert_eq!(row.available_spare_percent, None);
        assert_eq!(row.media_errors, None);
    }

    #[test]
    fn nvme_device_parses() {
        let v: Value = serde_json::from_str(
            r#"{
                "model_name": "Samsung SSD 980 PRO 1TB",
                "serial_number": "S5GXNX0R123456",
                "smart_status": {"passed": true, "nvme": {"value": 0}},
                "temperature": {"current": 41},
                "power_on_time": {"hours": 1234},
                "power_cycle_count": 456,
                "nvme_smart_health_information_log": {
                    "critical_warning": 0,
                    "temperature": 41,
                    "available_spare": 100,
                    "percentage_used": 3,
                    "media_errors": 0,
                    "power_cycles": 456,
                    "power_on_hours": 1234
                }
            }"#,
        )
        .unwrap();
        let row = parse_device("/dev/nvme0", &v).expect("should parse");
        assert_eq!(row.health_passed, Some(true));
        assert_eq!(row.temperature_c, Some(41.0));
        assert_eq!(row.percentage_used, Some(3));
        assert_eq!(row.available_spare_percent, Some(100));
        assert_eq!(row.media_errors, Some(0));
        // ATA-only fields stay None on NVMe.
        assert_eq!(row.reallocated_sectors, None);
    }

    #[test]
    fn failing_disk_still_parses() {
        // smartctl exits nonzero (bit 3) for a failing disk but the JSON
        // is complete — exactly the reading we must not drop.
        let v: Value = serde_json::from_str(
            r#"{
                "smart_status": {"passed": false},
                "temperature": {"current": 52},
                "ata_smart_attributes": {"table": [
                    {"id": 5, "raw": {"value": 1532}}
                ]}
            }"#,
        )
        .unwrap();
        let row = parse_device("/dev/sdb", &v).expect("should parse");
        assert_eq!(row.health_passed, Some(false));
        assert_eq!(row.reallocated_sectors, Some(1532));
    }

    #[test]
    fn standby_device_skipped() {
        // `-n standby` answer: messages only, no smart_status block.
        let v: Value = serde_json::from_str(
            r#"{"smartctl": {"exit_status": 2, "messages": [
                {"string": "Device is in STANDBY mode, exit(2)", "severity": "information"}
            ]}}"#,
        )
        .unwrap();
        assert!(parse_device("/dev/sdc", &v).is_none());
    }
}
