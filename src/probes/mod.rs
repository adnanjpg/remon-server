//! Probe engine: filesystem-defined external scripts that report status
//! + metrics on a schedule. See `migrations/0001_schema.sql` for the
//! storage shape and `manifest.rs` for the on-disk YAML schema.

pub mod manifest;
pub mod registry;
pub mod runner;
pub mod scheduler;
