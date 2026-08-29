//! Action definitions: the operator-authored scripts an alert transition can
//! run. See `services/actions.rs` for the executor that decides *whether* to
//! run one, and `models/action.rs` for the binding + run types.
//!
//! The built-in catalog (service/container lifecycle) has no manifest — it is
//! routed straight to the same managers the REST API uses, so nothing about
//! it lives on disk.

pub mod manifest;
pub mod registry;
