pub mod actions;
pub mod admin;
pub mod alerts;
pub mod auth;
pub mod cron;
pub mod docker;
pub mod events;
pub mod heartbeats;
pub mod logs;
pub mod metrics;
pub mod notifications;
pub mod probes;
pub mod process;
pub mod services;
pub mod sessions;
pub mod system;

use serde::{Deserialize, Deserializer};

/// Present-vs-absent for nullable update fields. Plain serde folds an
/// explicit JSON `null` into the same outer `None` as an absent field,
/// making the documented "null clears" form unreachable. With
/// `#[serde(default, deserialize_with = "double_option")]`: absent =
/// `None` (leave as-is), `null` = `Some(None)` (clear), value =
/// `Some(Some(v))`.
pub(crate) fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}
