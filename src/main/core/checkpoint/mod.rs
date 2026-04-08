//! Checkpoint/restore infrastructure for Shadow simulations.
//!
//! This module provides types and functions for serializing and deserializing
//! the state of a running simulation, enabling save/restore at window
//! boundaries.
//!
//! The approach uses parallel *snapshot* types (`*Snapshot`) that are fully
//! `serde`-compatible. Conversion between live simulation types and snapshot
//! types is performed through `From`/`TryFrom` implementations.

pub mod criu;
pub mod event_conversion;
pub mod ipc_rebuild;
pub mod reconstruct;
pub mod shmem_backup;
pub mod snapshot_types;
pub mod store;
