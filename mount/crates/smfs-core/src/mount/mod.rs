//! Mount backend abstraction.
//!
//! Unifies the FUSE (Linux) and NFSv3-over-localhost (macOS) mount paths
//! behind a single API — [`MountOpts`], [`MountHandle`], [`mount_fs()`].
//! This module is the *only* place in the codebase that knows about FUSE or
//! NFS; everything else talks to the [`crate::vfs::FileSystem`] trait.
//!
//! ## Sub-modules
//!
//! - [`fuse`] — FUSE adapter, Linux only (via the `fuser` crate)
//! - [`nfs`] — NFSv3 adapter, unix-wide (via the `nfsserve` crate)
//!
//! ## Build status
//!
//! M3b: public API (types + dispatch) is in place. Calling [`mount_fs()`]
//! still errors with "not implemented" because the backend bodies arrive
//! in M3c–M3f.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use crate::vfs::FileSystem;

#[cfg(target_os = "linux")]
pub mod fuse;

#[cfg(unix)]
pub mod nfs;

// ─── Backend selector ──────────────────────────────────────────────────────

/// Which mount backend to use for a given mount.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum MountBackend {
    /// FUSE — Linux only. Uses the `fuser` crate. Not available on macOS
    /// because it would require installing macFUSE, which supermemoryfs
    /// deliberately avoids.
    Fuse,

    /// NFSv3 over localhost — works on both macOS and Linux.
    ///
    /// The daemon binds an in-process NFSv3 server on `127.0.0.1:<auto-port>`
    /// and asks the operating system's native NFS client to mount it. No
    /// kernel extension, no third-party driver — on macOS this is the trick
    /// that replaces FUSE entirely.
    Nfs,
}

impl Default for MountBackend {
    /// The sensible default backend for the current target OS.
    ///
    /// - Linux → [`MountBackend::Fuse`] (native Linux story)
    /// - Anything else (including macOS) → [`MountBackend::Nfs`]
    fn default() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::Fuse
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::Nfs
        }
    }
}

impl std::fmt::Display for MountBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fuse => write!(f, "fuse"),
            Self::Nfs => write!(f, "nfs"),
        }
    }
}

impl FromStr for MountBackend {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "fuse" => Ok(Self::Fuse),
            "nfs" => Ok(Self::Nfs),
            other => anyhow::bail!("unknown mount backend: '{other}' (expected 'fuse' or 'nfs')"),
        }
    }
}

// ─── Mount options ─────────────────────────────────────────────────────────

/// Configuration for a single mount.
///
/// Construct with [`MountOpts::new`] and modify fields in place, or use
/// the builder-style setters on a chained expression.
#[derive(Clone, Debug)]
pub struct MountOpts {
    /// Path where the filesystem should be mounted. Must exist before mounting.
    pub mountpoint: PathBuf,

    /// Which backend to use. Use [`MountBackend::default()`] for the
    /// OS-preferred choice.
    pub backend: MountBackend,

    /// Filesystem name reported in `mount`/`df` output. Usually
    /// `"supermemoryfs"`.
    pub fsname: String,

    /// UID to report for inodes whose owner is otherwise unspecified.
    /// `None` means "use the current process's effective UID."
    pub uid: Option<u32>,

    /// GID to report similarly. `None` means "use the current process's
    /// effective GID."
    pub gid: Option<u32>,

    /// Allow non-root users on the host to access the mount (FUSE option).
    /// Default: `false`.
    pub allow_other: bool,

    /// Allow root on the host to access the mount (FUSE option). Default:
    /// `false`.
    pub allow_root: bool,

    /// Automatically unmount when the daemon process exits. Default: `true`
    /// — safer for foreground mounts in M4.
    pub auto_unmount: bool,

    /// If the filesystem is busy at unmount time, unmount lazily. Default:
    /// `false`.
    pub lazy_unmount: bool,

    /// Maximum time to wait for the mount to become visible to the kernel
    /// after issuing the mount command. Default: 10 seconds.
    pub timeout: Duration,
}

impl MountOpts {
    /// Create a new [`MountOpts`] with sensible defaults for the given
    /// mountpoint and backend.
    pub fn new(mountpoint: PathBuf, backend: MountBackend) -> Self {
        Self {
            mountpoint,
            backend,
            fsname: "supermemoryfs".to_string(),
            uid: None,
            gid: None,
            allow_other: false,
            allow_root: false,
            auto_unmount: true,
            lazy_unmount: false,
            timeout: Duration::from_secs(10),
        }
    }

    /// Override the `fsname` field fluently.
    pub fn with_fsname(mut self, fsname: impl Into<String>) -> Self {
        self.fsname = fsname.into();
        self
    }

    /// Override the `uid`/`gid` pair fluently.
    pub fn with_ownership(mut self, uid: u32, gid: u32) -> Self {
        self.uid = Some(uid);
        self.gid = Some(gid);
        self
    }

    /// Override the `timeout` fluently.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

// ─── Mount handle (RAII guard) ─────────────────────────────────────────────

/// Handle to a live filesystem mount.
///
/// Dropping a `MountHandle` unmounts the filesystem cleanly. Callers should
/// keep the handle alive for the duration of the mount (e.g. in a variable
/// bound to the same scope as `tokio::signal::ctrl_c().await`).
#[derive(Debug)]
pub struct MountHandle {
    mountpoint: PathBuf,
    backend: MountBackend,
    #[allow(dead_code)] // used by the M3d/M3f drop paths
    lazy_unmount: bool,
    inner: MountHandleInner,
}

/// Backend-specific state kept alive while the mount exists.
///
/// The variants grow as backends land: `Stub` is a test-only placeholder.
/// M3d adds `Nfs { server_handle }`. M3f adds `Fuse { session }`.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum MountHandleInner {
    /// Test-only placeholder. Constructed by unit tests that need a
    /// `MountHandle` without an actual mount. Real backends use the
    /// variants below.
    #[allow(dead_code)]
    Stub,

    /// Live NFSv3 mount. `server_handle` owns the spawned task running
    /// `nfsserve::tcp::NFSTcpListener::handle_forever()`. Aborted on drop
    /// because nfsserve 0.11 has no graceful shutdown protocol.
    #[cfg(unix)]
    Nfs {
        server_handle: tokio::task::JoinHandle<()>,
    },
}

impl MountHandle {
    /// Path where this filesystem is currently mounted.
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Which backend is handling this mount.
    pub fn backend(&self) -> MountBackend {
        self.backend
    }

    /// Construct a `MountHandle` for a newly-created NFS mount.
    ///
    /// Private constructor used by [`nfs::mount_nfs`]. Outside callers should
    /// go through [`mount_fs`].
    #[cfg(unix)]
    pub(super) fn new_nfs(
        mountpoint: PathBuf,
        lazy_unmount: bool,
        server_handle: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            mountpoint,
            backend: MountBackend::Nfs,
            lazy_unmount,
            inner: MountHandleInner::Nfs { server_handle },
        }
    }
}

impl Drop for MountHandle {
    fn drop(&mut self) {
        // Move cwd away from the mountpoint so unmount doesn't hit EBUSY.
        let _ = std::env::set_current_dir("/");

        match &mut self.inner {
            MountHandleInner::Stub => {
                tracing::trace!(
                    mountpoint = %self.mountpoint.display(),
                    "dropping stub MountHandle"
                );
            }
            #[cfg(unix)]
            MountHandleInner::Nfs { server_handle } => {
                // Best-effort unmount. Drop impls can't return errors, so
                // we log and continue; the task abort below still runs.
                if let Err(e) = nfs::unmount_nfs(&self.mountpoint, self.lazy_unmount) {
                    tracing::error!(
                        error = %e,
                        mountpoint = %self.mountpoint.display(),
                        "failed to unmount NFS"
                    );
                }
                // Always abort the server task, even if unmount failed,
                // to avoid leaking a tokio task.
                server_handle.abort();
                tracing::debug!(
                    mountpoint = %self.mountpoint.display(),
                    "NFS mount handle dropped"
                );
            }
        }
    }
}

// ─── Dispatch ──────────────────────────────────────────────────────────────

/// Mount a filesystem at the configured path.
///
/// Dispatches to the backend named by `opts.backend`. Returns a
/// [`MountHandle`] whose `Drop` impl unmounts cleanly when the handle goes
/// out of scope.
///
/// # Errors
///
/// - The requested backend isn't supported on the current platform
///   (e.g. [`MountBackend::Fuse`] on macOS).
/// - The backend fails to start (mountpoint missing, port unavailable,
///   kernel rejection, etc.).
///
/// # Note (M3b)
///
/// The dispatch is wired up but both backends are still stubbed — calling
/// this function always returns "not implemented" for now. M3c/M3d fill in
/// the NFS path; M3e/M3f fill in the FUSE path.
pub async fn mount_fs<F>(fs: Arc<F>, opts: MountOpts) -> anyhow::Result<MountHandle>
where
    F: FileSystem + 'static,
{
    match opts.backend {
        #[cfg(target_os = "linux")]
        MountBackend::Fuse => fuse::mount_fuse(fs, opts).await,

        #[cfg(not(target_os = "linux"))]
        MountBackend::Fuse => {
            let _ = fs;
            anyhow::bail!(
                "FUSE backend is only supported on Linux; \
                 use MountBackend::Nfs on this operating system"
            )
        }

        MountBackend::Nfs => nfs::mount_nfs(fs, opts).await,
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::MemFs;

    #[test]
    fn mount_backend_default_matches_os() {
        let default = MountBackend::default();
        #[cfg(target_os = "linux")]
        assert_eq!(default, MountBackend::Fuse);
        #[cfg(not(target_os = "linux"))]
        assert_eq!(default, MountBackend::Nfs);
    }

    #[test]
    fn mount_backend_from_str_accepts_fuse() {
        assert_eq!("fuse".parse::<MountBackend>().unwrap(), MountBackend::Fuse);
        assert_eq!("FUSE".parse::<MountBackend>().unwrap(), MountBackend::Fuse);
        assert_eq!("Fuse".parse::<MountBackend>().unwrap(), MountBackend::Fuse);
    }

    #[test]
    fn mount_backend_from_str_accepts_nfs() {
        assert_eq!("nfs".parse::<MountBackend>().unwrap(), MountBackend::Nfs);
        assert_eq!("NFS".parse::<MountBackend>().unwrap(), MountBackend::Nfs);
    }

    #[test]
    fn mount_backend_from_str_rejects_unknown() {
        assert!("smb".parse::<MountBackend>().is_err());
        assert!("".parse::<MountBackend>().is_err());
        assert!("fuse3".parse::<MountBackend>().is_err());
    }

    #[test]
    fn mount_backend_display_roundtrips() {
        for backend in [MountBackend::Fuse, MountBackend::Nfs] {
            let rendered = backend.to_string();
            let parsed: MountBackend = rendered.parse().unwrap();
            assert_eq!(parsed, backend);
        }
    }

    #[test]
    fn mount_opts_new_uses_sane_defaults() {
        let opts = MountOpts::new(PathBuf::from("/tmp/mnt"), MountBackend::Nfs);
        assert_eq!(opts.mountpoint, PathBuf::from("/tmp/mnt"));
        assert_eq!(opts.backend, MountBackend::Nfs);
        assert_eq!(opts.fsname, "supermemoryfs");
        assert_eq!(opts.uid, None);
        assert_eq!(opts.gid, None);
        assert!(!opts.allow_other);
        assert!(!opts.allow_root);
        assert!(opts.auto_unmount);
        assert!(!opts.lazy_unmount);
        assert_eq!(opts.timeout, Duration::from_secs(10));
    }

    #[test]
    fn mount_opts_builder_style_overrides_work() {
        let opts = MountOpts::new(PathBuf::from("/tmp/mnt"), MountBackend::Nfs)
            .with_fsname("custom")
            .with_ownership(1000, 1000)
            .with_timeout(Duration::from_secs(30));
        assert_eq!(opts.fsname, "custom");
        assert_eq!(opts.uid, Some(1000));
        assert_eq!(opts.gid, Some(1000));
        assert_eq!(opts.timeout, Duration::from_secs(30));
    }

    #[tokio::test]
    async fn mount_fs_errors_on_missing_mountpoint() {
        // Post-M3d: mount_fs dispatches to the real mount_nfs, which validates
        // that the mountpoint exists before attempting anything else. A
        // nonexistent path should surface as a clean error.
        let fs = Arc::new(MemFs::new());
        let opts = MountOpts::new(
            PathBuf::from("/tmp/smfs-nonexistent-test-path-mfs-mod"),
            MountBackend::Nfs,
        );
        let result = mount_fs(fs, opts).await;
        assert!(
            result.is_err(),
            "mount_fs should fail when mountpoint does not exist"
        );
    }

    #[test]
    fn mount_handle_accessors_work() {
        let handle = MountHandle {
            mountpoint: PathBuf::from("/tmp/test"),
            backend: MountBackend::Nfs,
            lazy_unmount: false,
            inner: MountHandleInner::Stub,
        };
        assert_eq!(handle.mountpoint(), Path::new("/tmp/test"));
        assert_eq!(handle.backend(), MountBackend::Nfs);
    }

    #[test]
    fn mount_handle_stub_drop_does_not_panic() {
        let handle = MountHandle {
            mountpoint: PathBuf::from("/tmp/test"),
            backend: MountBackend::Nfs,
            lazy_unmount: false,
            inner: MountHandleInner::Stub,
        };
        drop(handle);
    }
}
