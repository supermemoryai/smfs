//! Local SQLite cache.
//!
//! Persists inodes, dentries, file chunks, and sync state between daemon
//! restarts.
//!
//! The cache is a *passive store*: it never calls the API or spawns
//! background tasks. The sync engine (in [`crate::sync`]) is the only
//! thing that mutates sync-state fields (added in M7–M8).

pub(crate) mod db;
mod file;
mod fs;
pub mod hydration;
pub mod profile;

pub use db::{is_macos_noise_path, Db, DEFAULT_CHUNK_SIZE, DENTRY_CACHE_MAX, ROOT_INO};
pub use fs::{ReconcileOutcome, SupermemoryFs};
pub use hydration::{HydrationKey, HydrationScheduler};

#[cfg(test)]
mod tests;
