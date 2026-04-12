//! NFSv3 mount adapter (unix-wide).
//!
//! Bridges [`nfsserve::vfs::NFSFileSystem`] callbacks to our
//! [`vfs::FileSystem`](crate::vfs::FileSystem) trait methods. Used as the
//! primary mount backend on macOS (since macOS has no kernel-level FUSE
//! without installing macFUSE) and as an optional alternate backend on
//! Linux.
//!
//! ## How it works
//!
//! The NFS "mount" on macOS is a trick: we bind an `nfsserve::NFSTcpListener`
//! on `127.0.0.1:<free-port>`, exec `/sbin/mount_nfs -o vers=3,tcp,...
//! 127.0.0.1:/ <mountpoint>`, and macOS mounts the localhost NFS server as
//! if it were a remote NFS share. No kernel extension, no security prompts,
//! nothing to install — pure userspace + native macOS tooling.
//!
//! ## Planned contents
//!
//! **M3c** — adapter implementation:
//! - `NfsAdapter<F: FileSystem>` struct wrapping `Arc<F>`
//! - `impl nfsserve::vfs::NFSFileSystem for NfsAdapter<F>` covering the
//!   ~13 NFSv3 methods: `root_dir`, `lookup`, `getattr`, `setattr`,
//!   `readdir`, `read`, `write`, `create`, `remove`, `rename`, `mkdir`,
//!   `symlink`, `readlink`
//! - `VfsError::to_nfsstat3` mapping (separate from `to_errno` because
//!   NFS uses its own error codes)
//!
//! **M3d** — mount command integration:
//! - `mount_nfs(fs, opts)` — finds a free TCP port, binds the listener,
//!   execs the platform-specific mount command, returns a `MountHandle`
//! - `unmount_nfs(path, lazy)` — execs `umount` (with `-l` on Linux for
//!   lazy unmount)
//! - Port discovery starting at 11111

use std::sync::Arc;

use crate::vfs::FileSystem;

use super::{MountHandle, MountOpts};

/// Mount a filesystem using the NFSv3 backend.
///
/// Stub for M3b — the real implementation lands in M3d (adapter wiring +
/// mount command exec). Currently always returns "not implemented".
#[allow(clippy::needless_pass_by_value)] // signature matches the eventual real one
pub async fn mount_nfs<F>(fs: Arc<F>, opts: MountOpts) -> anyhow::Result<MountHandle>
where
    F: FileSystem + 'static,
{
    let _ = (fs, opts);
    anyhow::bail!("NFS mount not implemented yet — lands in M3d")
}

// TODO(M3c): NfsAdapter struct + nfsserve::vfs::NFSFileSystem implementation
// TODO(M3d): mount_nfs / unmount_nfs helpers + port discovery
