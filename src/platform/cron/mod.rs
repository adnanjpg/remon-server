use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    /// Schedule expression: standard 5-field "m h dom mon dow" or a @special
    /// shorthand (@hourly, @daily, @weekly, @monthly, @reboot, etc.).
    pub schedule: String,
    /// User field — present in system crontabs (/etc/crontab, /etc/cron.d/*),
    /// absent in user crontabs (/var/spool/cron/crontabs/*).
    pub user: Option<String>,
    pub command: String,
    /// Source file path, e.g. "/etc/crontab" or "/etc/cron.d/logrotate".
    pub source: String,
}

/// List all discoverable cron jobs.
///
/// Sources checked (read errors are silently skipped):
/// - `/etc/crontab`              — system crontab (6-field: schedule + user)
/// - `/etc/cron.d/*`             — drop-in system crontabs (same 6-field format)
/// - `/var/spool/cron/crontabs/*`— user crontabs (5-field: no user column)
///
/// Only available on Unix. Returns an empty vec on Windows.
pub async fn list() -> Vec<CronJob> {
    #[cfg(not(unix))]
    {
        return Vec::new();
    }

    #[cfg(unix)]
    {
        let mut jobs = Vec::new();

        // System crontab
        parse_system_file("/etc/crontab", &mut jobs).await;

        // Drop-in system crontabs
        if let Ok(mut entries) = fs::read_dir("/etc/cron.d").await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_file() {
                    let path_str = path.to_string_lossy().into_owned();
                    parse_system_file(&path_str, &mut jobs).await;
                }
            }
        }

        // User crontabs (may require root to read)
        if let Ok(mut entries) = fs::read_dir("/var/spool/cron/crontabs").await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_file() {
                    let username = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let path_str = path.to_string_lossy().into_owned();
                    parse_user_file(&path_str, &username, &mut jobs).await;
                }
            }
        }

        jobs
    }
}

// ===== Parsers =====

#[cfg(unix)]
async fn parse_system_file(path: &str, out: &mut Vec<CronJob>) {
    let Ok(content) = fs::read_to_string(path).await else {
        return;
    };
    for line in content.lines() {
        if let Some(job) = parse_system_line(line, path) {
            out.push(job);
        }
    }
}

#[cfg(unix)]
async fn parse_user_file(path: &str, username: &str, out: &mut Vec<CronJob>) {
    let Ok(content) = fs::read_to_string(path).await else {
        return;
    };
    for line in content.lines() {
        if let Some(job) = parse_user_line(line, username, path) {
            out.push(job);
        }
    }
}

#[cfg(unix)]
fn parse_system_line(line: &str, source: &str) -> Option<CronJob> {
    let line = preprocess(line)?;
    let mut parts = line
        .splitn(8, char::is_whitespace)
        .filter(|s| !s.is_empty());

    let first = parts.next()?;

    if let Some(schedule) = expand_special(first) {
        // @special format: @daily user command…
        let user = parts.next()?.to_string();
        let command = parts.collect::<Vec<_>>().join(" ");
        if command.is_empty() {
            return None;
        }
        return Some(CronJob {
            schedule,
            user: Some(user),
            command,
            source: source.to_string(),
        });
    }

    // Standard: min hour dom mon dow user command…
    let mut tokens = vec![first];
    for _ in 0..4 {
        tokens.push(parts.next()?);
    }
    let schedule = tokens.join(" ");
    let user = parts.next()?.to_string();
    let command = parts.collect::<Vec<_>>().join(" ");
    if command.is_empty() {
        return None;
    }

    Some(CronJob {
        schedule,
        user: Some(user),
        command,
        source: source.to_string(),
    })
}

#[cfg(unix)]
fn parse_user_line(line: &str, username: &str, source: &str) -> Option<CronJob> {
    let line = preprocess(line)?;
    let mut parts = line
        .splitn(7, char::is_whitespace)
        .filter(|s| !s.is_empty());

    let first = parts.next()?;

    if let Some(schedule) = expand_special(first) {
        // @special format: @daily command…
        let command = parts.collect::<Vec<_>>().join(" ");
        if command.is_empty() {
            return None;
        }
        return Some(CronJob {
            schedule,
            user: Some(username.to_string()),
            command,
            source: source.to_string(),
        });
    }

    // Standard: min hour dom mon dow command…
    let mut tokens = vec![first];
    for _ in 0..4 {
        tokens.push(parts.next()?);
    }
    let schedule = tokens.join(" ");
    let command = parts.collect::<Vec<_>>().join(" ");
    if command.is_empty() {
        return None;
    }

    Some(CronJob {
        schedule,
        user: Some(username.to_string()),
        command,
        source: source.to_string(),
    })
}

#[cfg(unix)]
fn preprocess(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    // Skip environment variable assignments (KEY=VALUE with no spaces before '=').
    if let Some(eq) = line.find('=') {
        let key = &line[..eq];
        if !key.contains(' ') && !key.contains('\t') {
            return None;
        }
    }
    Some(line)
}

#[cfg(unix)]
fn expand_special(token: &str) -> Option<String> {
    match token {
        "@reboot" => Some("@reboot".into()),
        "@yearly" | "@annually" => Some("0 0 1 1 *".into()),
        "@monthly" => Some("0 0 1 * *".into()),
        "@weekly" => Some("0 0 * * 0".into()),
        "@daily" | "@midnight" => Some("0 0 * * *".into()),
        "@hourly" => Some("0 * * * *".into()),
        _ => None,
    }
}
