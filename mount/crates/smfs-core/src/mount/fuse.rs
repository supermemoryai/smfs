//! FUSE mount adapter (Linux only).
//!
//! Bridges [`fuser::Filesystem`] callbacks to our
//! [`vfs::FileSystem`](crate::vfs::FileSystem) trait methods. When the FUSE
//! kernel sends a `read`/`write`/`lookup`/etc. callback, this adapter
//! spawns a tokio task that calls the corresponding async trait method,
//! maps the result back to the FUSE reply shape, and translates errors
//! through [`VfsError::to_errno`](crate::vfs::VfsError::to_errno).
//!
//! ## Build gating
//!
//! This file is only compiled when `target_os = "linux"`. On macOS, the
//! `pub mod fuse` declaration in the parent module is `#[cfg]`-gated out,
//! and `fuser` isn't in the dependency tree at all.
//!
//! ## Planned contents
//!
//! **M3e** — adapter implementation:
//! - `FuseAdapter<F: FileSystem>` struct wrapping `Arc<F>` + tokio handle
//! - `impl fuser::Filesystem for FuseAdapter<F>` covering all ~20
//!   callbacks: `lookup`, `getattr`, `setattr`, `readdir`, `read`,
//!   `write`, `create`, `mknod` (stub to ENOTSUP), `unlink`, `mkdir`,
//!   `rmdir`, `rename`, `open`, `release`, `opendir`, `readdir`,
//!   `releasedir`, `flush`, `fsync`, `statfs`, `symlink`, `readlink`,
//!   `link`
//! - File handle tracking via `DashMap<u64, BoxedFile>` so repeated
//!   reads/writes on the same FUSE `fh` map to the same `BoxedFile`
//!
//! **M3f** — mount command integration:
//! - `mount_fuse(fs, opts)` — spawns `fuser::spawn_mount2` in a blocking
//!   thread, returns a `MountHandle` whose `Drop` unmounts cleanly
//! - `unmount_fuse(path, lazy)` — tries `fusermount3 -u` first, falls
//!   back to `fusermount -u`, adds `-z` for lazy

use std::sync::Arc;

use crate::vfs::FileSystem;

use super::{MountHandle, MountOpts};

/// Mount a filesystem using the FUSE backend (Linux only).
///
/// Stub for M3b — the real implementation lands in M3f (adapter wiring +
/// `fuser::spawn_mount2` invocation). Currently always returns "not implemented".
#[allow(clippy::needless_pass_by_value)] // signature matches the eventual real one
pub async fn mount_fuse<F>(fs: Arc<F>, opts: MountOpts) -> anyhow::Result<MountHandle>
where
    F: FileSystem + 'static,
{
    let _ = (fs, opts);
    anyhow::bail!("FUSE mount not implemented yet — lands in M3f")
}

// TODO(M3e): FuseAdapter struct + fuser::Filesystem implementation
// TODO(M3f): mount_fuse / unmount_fuse helpers
