//! supermemoryfs core library.
//!
//! This crate holds everything interesting about supermemoryfs:
//!
//! - [`vfs`] — the `FileSystem` trait, `MemFs` reference implementation, and
//!   supporting types (`FileAttr`, `VfsError`, path helpers, POSIX mode constants).
//! - [`mount`] — FUSE (Linux) and NFS (macOS) mount adapters wrapping vendored `fuser`/`nfsserve`.
//! - [`sync`] — background sync engine that reconciles the local cache with the Supermemory API.
//! - [`api`] — typed HTTP client over the Supermemory backend.
//! - [`daemon`] — long-running daemon lifecycle, fork dance, and unix-socket IPC control channel.
//! - [`config`] — XDG paths and runtime configuration.
//!
//! The `smfs` binary (in the sibling crate) is a thin CLI dispatch layer on top of this library.
//! All real behavior lives here.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod agent_hint;
pub mod api;
pub mod cache;
pub mod config;
pub mod daemon;
pub mod mount;
pub mod sync;
pub mod vfs;

/// Crate version, exposed for diagnostics.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
