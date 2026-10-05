//! The pairing window: at most one code at a time, kept in `runtime_state` so
//! the `pair` subcommand and the running server see the same one.
//!
//! Only the host (`remon-server pair`) or an already paired device can open a
//! window. Opening one replaces whatever was open, since both are trusted.

use std::collections::HashMap;

use colored::Colorize;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::auth::service::AuthService;
use crate::error::AppResult;
use crate::storage::repositories::RuntimeStateRepository;

const KEY: &str = "pairing_window";

/// Wrong codes one client address may try against a window.
pub const MAX_ATTEMPTS_PER_IP: u8 = 3;
/// Wrong codes a window takes in total before it closes. With 10^8 codes this
/// keeps a guess across many addresses at 2 in 10 million.
pub const MAX_ATTEMPTS: u8 = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    pub code: String,
    pub expires_at: i64,
}

/// Wrong-code counts for the current window, held by the server only.
#[derive(Debug, Default)]
pub struct Attempts {
    pub code: String,
    pub total: u8,
    pub per_ip: HashMap<String, u8>,
}

impl Attempts {
    /// Start counting afresh when the window has changed under us.
    pub fn track(&mut self, window: &Window) {
        if self.code != window.code {
            *self = Self {
                code: window.code.clone(),
                ..Default::default()
            };
        }
    }
}

/// Open a new window, replacing any open one.
pub async fn open(pool: &SqlitePool, ttl_secs: u64) -> AppResult<Window> {
    let window = Window {
        code: AuthService::generate_pairing_code(),
        expires_at: chrono::Utc::now().timestamp() + ttl_secs as i64,
    };
    let value = serde_json::to_string(&window).expect("window serializes");
    RuntimeStateRepository::new(pool.clone())
        .set(KEY, &value)
        .await?;
    Ok(window)
}

/// The open window, if any. An expired one is closed on the way.
pub async fn current(pool: &SqlitePool) -> AppResult<Option<Window>> {
    let repo = RuntimeStateRepository::new(pool.clone());
    let Some(value) = repo.get(KEY).await? else {
        return Ok(None);
    };
    match serde_json::from_str::<Window>(&value) {
        Ok(w) if w.expires_at > chrono::Utc::now().timestamp() => Ok(Some(w)),
        _ => {
            repo.delete(KEY).await?;
            Ok(None)
        }
    }
}

pub async fn close(pool: &SqlitePool) -> AppResult<()> {
    RuntimeStateRepository::new(pool.clone()).delete(KEY).await
}

/// The code block shown to whoever opened the window on the host.
pub fn print(window: &Window) {
    let ttl = (window.expires_at - chrono::Utc::now().timestamp()).max(0);
    let ttl_human = if ttl % 60 == 0 {
        format!("{}m", ttl / 60)
    } else {
        format!("{}s", ttl)
    };
    println!();
    println!("  {}", "Device pairing".bold().cyan());
    println!("  {} {}", "code   ".dimmed(), window.code.bold().yellow());
    println!("  {} {}", "expires".dimmed(), ttl_human.dimmed());
    println!("  {}", "enter this code in your client to pair".dimmed());
    println!();
}
