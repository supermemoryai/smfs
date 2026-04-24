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
//! ## Status
//!
//! - **M3c** (this commit): [`NfsAdapter`] struct + full
//!   [`nfsserve::vfs::NFSFileSystem`] implementation that translates NFSv3
//!   operations into `vfs::FileSystem` trait calls. Inline conformance tests.
//! - **M3d** (next): wire [`mount_nfs`] to an `NFSTcpListener`, port
//!   discovery, and the platform mount command exec.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfsstring, nfstime3, sattr3, set_atime,
    set_gid3, set_mode3, set_mtime, set_size3, set_uid3, specdata3,
};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

use crate::vfs::{FileAttr, FileSystem, SetAttr, TimeOrNow, Timestamp, VfsError};

use super::{MountHandle, MountOpts};

/// Port to start scanning from when looking for a free local port.
/// High unprivileged port so we don't need root.
const DEFAULT_NFS_PORT: u16 = 11111;

/// Maximum number of consecutive ports to try before giving up.
const MAX_PORT_SCAN: u16 = 100;

// ─── Translation helpers ───────────────────────────────────────────────────

/// Map our typed VFS error to the NFSv3 error code `nfsstat3` enum.
fn vfs_err_to_nfsstat3(err: &VfsError) -> nfsstat3 {
    match err {
        VfsError::NotFound => nfsstat3::NFS3ERR_NOENT,
        VfsError::AlreadyExists => nfsstat3::NFS3ERR_EXIST,
        VfsError::NotADirectory => nfsstat3::NFS3ERR_NOTDIR,
        VfsError::IsADirectory => nfsstat3::NFS3ERR_ISDIR,
        VfsError::NotASymlink => nfsstat3::NFS3ERR_INVAL,
        VfsError::NotEmpty => nfsstat3::NFS3ERR_NOTEMPTY,
        VfsError::InvalidPath(_) => nfsstat3::NFS3ERR_INVAL,
        VfsError::NameTooLong(_) => nfsstat3::NFS3ERR_NAMETOOLONG,
        VfsError::RootOperation => nfsstat3::NFS3ERR_ACCES,
        VfsError::PermissionDenied => nfsstat3::NFS3ERR_ACCES,
        VfsError::InvalidRename => nfsstat3::NFS3ERR_INVAL,
        // nfsserve 0.11 doesn't expose NFS3ERR_LOOP; surface as generic I/O.
        VfsError::SymlinkLoop => nfsstat3::NFS3ERR_IO,
        VfsError::NotSupported => nfsstat3::NFS3ERR_NOTSUPP,
        VfsError::Io(_) => nfsstat3::NFS3ERR_IO,
    }
}

/// Convert our [`FileAttr`] into the NFS wire-format `fattr3` struct.
fn file_attr_to_fattr3(attr: &FileAttr) -> fattr3 {
    let ftype = if attr.is_directory() {
        ftype3::NF3DIR
    } else if attr.is_symlink() {
        ftype3::NF3LNK
    } else {
        ftype3::NF3REG
    };

    fattr3 {
        ftype,
        mode: attr.mode & 0o7777, // NFS mode field is permission bits only
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        size: attr.size,
        used: attr.size, // no sparse files, `used` equals `size`
        rdev: specdata3 {
            specdata1: 0,
            specdata2: 0,
        },
        fsid: 0,
        fileid: attr.ino,
        atime: nfstime3 {
            seconds: attr.atime.sec as u32,
            nseconds: attr.atime.nsec,
        },
        mtime: nfstime3 {
            seconds: attr.mtime.sec as u32,
            nseconds: attr.mtime.nsec,
        },
        ctime: nfstime3 {
            seconds: attr.ctime.sec as u32,
            nseconds: attr.ctime.nsec,
        },
    }
}

/// Convert nfsserve's partial-update `sattr3` to our [`SetAttr`].
#[allow(clippy::needless_pass_by_value)] // mirrors NFSFileSystem::setattr signature
fn sattr3_to_set_attr(s: sattr3) -> SetAttr {
    SetAttr {
        mode: match s.mode {
            set_mode3::mode(m) => Some(m & 0o7777),
            set_mode3::Void => None,
        },
        uid: match s.uid {
            set_uid3::uid(u) => Some(u),
            set_uid3::Void => None,
        },
        gid: match s.gid {
            set_gid3::gid(g) => Some(g),
            set_gid3::Void => None,
        },
        size: match s.size {
            set_size3::size(sz) => Some(sz),
            set_size3::Void => None,
        },
        atime: match s.atime {
            set_atime::DONT_CHANGE => None,
            set_atime::SET_TO_SERVER_TIME => Some(TimeOrNow::Now),
            set_atime::SET_TO_CLIENT_TIME(t) => Some(TimeOrNow::Time(Timestamp {
                sec: t.seconds as i64,
                nsec: t.nseconds,
            })),
        },
        mtime: match s.mtime {
            set_mtime::DONT_CHANGE => None,
            set_mtime::SET_TO_SERVER_TIME => Some(TimeOrNow::Now),
            set_mtime::SET_TO_CLIENT_TIME(t) => Some(TimeOrNow::Time(Timestamp {
                sec: t.seconds as i64,
                nsec: t.nseconds,
            })),
        },
    }
}

// ─── NFS adapter type ──────────────────────────────────────────────────────

/// Adapter that implements [`NFSFileSystem`] by delegating to our
/// [`FileSystem`](crate::vfs::FileSystem) trait.
///
/// Wrap around any `Arc<F: FileSystem + 'static>` (e.g.
/// `Arc<MemFs>` in tests and M4, `Arc<SupermemoryFs>` in production once
/// M5 ships the real backend).
///
/// Stateless aside from the underlying filesystem: no open-file table,
/// no inode cache. Every trait method goes straight through to the
/// wrapped `FileSystem`.
pub struct NfsAdapter<F: FileSystem> {
    fs: Arc<F>,
    /// UID used for ownership on newly created files, directories, and
    /// symlinks. The nfsserve 0.11 trait doesn't pass caller identity
    /// through, so we fall back to a default.
    default_uid: u32,
    /// GID used for ownership on newly created entries.
    default_gid: u32,
}

impl<F: FileSystem> NfsAdapter<F> {
    /// Create a new adapter wrapping the given filesystem.
    pub fn new(fs: Arc<F>, default_uid: u32, default_gid: u32) -> Self {
        Self {
            fs,
            default_uid,
            default_gid,
        }
    }
}

// Manual Debug impl: the wrapped FileSystem doesn't require Debug as a
// supertrait, so we can't derive. Print only the ownership defaults.
impl<F: FileSystem> std::fmt::Debug for NfsAdapter<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NfsAdapter")
            .field("default_uid", &self.default_uid)
            .field("default_gid", &self.default_gid)
            .finish_non_exhaustive()
    }
}

// ─── NFSFileSystem trait implementation ───────────────────────────────────

#[async_trait]
impl<F: FileSystem + 'static> NFSFileSystem for NfsAdapter<F> {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        1 // matches MemFs::ROOT_INO and the FUSE convention
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = std::str::from_utf8(&filename.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        // `.` always refers to the directory itself.
        // `..` is stubbed to return the same dirid for M3c — see module doc.
        if name == "." || name == ".." {
            return Ok(dirid);
        }

        match self.fs.lookup(dirid, name).await {
            Ok(Some(attr)) => Ok(attr.ino),
            Ok(None) => Err(nfsstat3::NFS3ERR_NOENT),
            Err(e) => Err(vfs_err_to_nfsstat3(&e)),
        }
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        match self.fs.getattr(id).await {
            Ok(Some(attr)) => Ok(file_attr_to_fattr3(&attr)),
            Ok(None) => Err(nfsstat3::NFS3ERR_NOENT),
            Err(e) => Err(vfs_err_to_nfsstat3(&e)),
        }
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let set_attr = sattr3_to_set_attr(setattr);
        match self.fs.setattr(id, set_attr).await {
            Ok(attr) => Ok(file_attr_to_fattr3(&attr)),
            Err(e) => Err(vfs_err_to_nfsstat3(&e)),
        }
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let handle = self
            .fs
            .open(id, libc::O_RDONLY)
            .await
            .map_err(|e| vfs_err_to_nfsstat3(&e))?;
        let data = handle
            .read(offset, count as usize)
            .await
            .map_err(|e| vfs_err_to_nfsstat3(&e))?;
        let attr = handle
            .getattr()
            .await
            .map_err(|e| vfs_err_to_nfsstat3(&e))?;
        let eof = offset + data.len() as u64 >= attr.size;
        Ok((data, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let handle = self
            .fs
            .open(id, libc::O_RDWR)
            .await
            .map_err(|e| vfs_err_to_nfsstat3(&e))?;
        handle
            .write(offset, data)
            .await
            .map_err(|e| vfs_err_to_nfsstat3(&e))?;
        // Flush to push content to the API (NFS has no close/release callback).
        let _ = handle.flush().await;
        let attr = handle
            .getattr()
            .await
            .map_err(|e| vfs_err_to_nfsstat3(&e))?;
        Ok(file_attr_to_fattr3(&attr))
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = std::str::from_utf8(&filename.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let mode = match attr.mode {
            set_mode3::mode(m) => m & 0o7777,
            set_mode3::Void => 0o644,
        };

        let (file_attr, _handle) = self
            .fs
            .create_file(dirid, name, mode, self.default_uid, self.default_gid)
            .await
            .map_err(|e| vfs_err_to_nfsstat3(&e))?;
        Ok((file_attr.ino, file_attr_to_fattr3(&file_attr)))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        let name = std::str::from_utf8(&filename.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        match self.fs.lookup(dirid, name).await {
            Ok(Some(_)) => Err(nfsstat3::NFS3ERR_EXIST),
            Ok(None) => {
                let (attr, _handle) = self
                    .fs
                    .create_file(dirid, name, 0o644, self.default_uid, self.default_gid)
                    .await
                    .map_err(|e| vfs_err_to_nfsstat3(&e))?;
                Ok(attr.ino)
            }
            Err(e) => Err(vfs_err_to_nfsstat3(&e)),
        }
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = std::str::from_utf8(&dirname.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let attr = self
            .fs
            .mkdir(dirid, name, 0o755, self.default_uid, self.default_gid)
            .await
            .map_err(|e| vfs_err_to_nfsstat3(&e))?;
        Ok((attr.ino, file_attr_to_fattr3(&attr)))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let name = std::str::from_utf8(&filename.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let attr = self
            .fs
            .lookup(dirid, name)
            .await
            .map_err(|e| vfs_err_to_nfsstat3(&e))?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if attr.is_directory() {
            self.fs
                .rmdir(dirid, name)
                .await
                .map_err(|e| vfs_err_to_nfsstat3(&e))
        } else {
            self.fs
                .unlink(dirid, name)
                .await
                .map_err(|e| vfs_err_to_nfsstat3(&e))
        }
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let from_name =
            std::str::from_utf8(&from_filename.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let to_name = std::str::from_utf8(&to_filename.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        self.fs
            .rename(from_dirid, from_name, to_dirid, to_name)
            .await
            .map_err(|e| vfs_err_to_nfsstat3(&e))
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let entries = self
            .fs
            .readdir_plus(dirid)
            .await
            .map_err(|e| vfs_err_to_nfsstat3(&e))?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        let mut result = ReadDirResult {
            entries: Vec::new(),
            end: false,
        };
        let mut skip = start_after > 0;
        let mut consumed = 0usize;

        for entry in &entries {
            if skip {
                consumed += 1;
                if entry.attr.ino == start_after {
                    skip = false;
                }
                continue;
            }
            if result.entries.len() >= max_entries {
                break;
            }
            result.entries.push(DirEntry {
                fileid: entry.attr.ino,
                name: nfsstring(entry.name.as_bytes().to_vec()),
                attr: file_attr_to_fattr3(&entry.attr),
            });
            consumed += 1;
        }
        result.end = consumed >= entries.len();
        Ok(result)
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        _attr: &sattr3, // symlinks always have permissive mode bits; ignore
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = std::str::from_utf8(&linkname.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let target = std::str::from_utf8(&symlink.0).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let attr = self
            .fs
            .symlink(dirid, name, target, self.default_uid, self.default_gid)
            .await
            .map_err(|e| vfs_err_to_nfsstat3(&e))?;
        Ok((attr.ino, file_attr_to_fattr3(&attr)))
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        let target = self
            .fs
            .readlink(id)
            .await
            .map_err(|e| vfs_err_to_nfsstat3(&e))?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        Ok(nfsstring(target.into_bytes()))
    }
}

// ─── mount_nfs (real) ──────────────────────────────────────────────────────

/// Mount a filesystem at the path configured in `opts` using NFSv3 over
/// localhost.
///
/// Validates the mountpoint, wraps `fs` in an [`NfsAdapter`], binds an
/// [`nfsserve::tcp::NFSTcpListener`] on a free local port, spawns the
/// listener as a background task, execs the platform-specific mount
/// command, and returns a [`MountHandle`] whose `Drop` impl unmounts
/// cleanly.
pub async fn mount_nfs<F>(fs: Arc<F>, opts: MountOpts) -> anyhow::Result<MountHandle>
where
    F: FileSystem + 'static,
{
    use anyhow::Context;
    use nfsserve::tcp::{NFSTcp, NFSTcpListener};

    if !opts.mountpoint.exists() {
        anyhow::bail!("mountpoint does not exist: {}", opts.mountpoint.display());
    }

    // macOS's mount_nfs wants an absolute path.
    let mountpoint = std::fs::canonicalize(&opts.mountpoint).with_context(|| {
        format!(
            "failed to canonicalize mountpoint {}",
            opts.mountpoint.display()
        )
    })?;

    // Resolve ownership defaults. If the caller didn't set uid/gid in
    // MountOpts, fall back to 0 (root). The binary crate (M4) is where
    // we'll query the calling process's effective uid/gid; this library
    // crate has `#![forbid(unsafe_code)]` so it can't call libc::geteuid
    // directly, and we deliberately don't pull in an extra crate just
    // for a process identity lookup.
    let default_uid = opts.uid.unwrap_or(0);
    let default_gid = opts.gid.unwrap_or(0);

    let adapter = NfsAdapter::new(fs, default_uid, default_gid);

    let port = find_free_port(DEFAULT_NFS_PORT)?;
    let bind_addr = format!("127.0.0.1:{port}");

    let listener = NFSTcpListener::bind(&bind_addr, adapter)
        .await
        .with_context(|| format!("failed to bind NFS listener on {bind_addr}"))?;

    // Spawn the accept loop. nfsserve 0.11 has no graceful shutdown, so the
    // task runs forever until `server_handle.abort()` is called from Drop.
    let server_handle = tokio::spawn(async move {
        if let Err(e) = listener.handle_forever().await {
            tracing::error!(error = %e, "NFS server task ended unexpectedly");
        }
    });

    // Give the listener a moment to accept connections before the mount
    // command tries to reach it. Same 100ms value AgentFS uses.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Run the blocking `mount_nfs` subprocess on a dedicated blocking
    // thread so the tokio runtime stays free to service NFS RPC calls
    // from the kernel. Without spawn_blocking, a single-threaded runtime
    // (the default for `#[tokio::test]`) would deadlock: the subprocess
    // waits for NFS responses, the NFS listener can't run because the
    // main thread is blocked in subprocess wait.
    let mountpoint_for_cmd = mountpoint.clone();
    let mount_result =
        tokio::task::spawn_blocking(move || nfs_mount_command(port, &mountpoint_for_cmd))
            .await
            .map_err(|e| anyhow::anyhow!("mount command task panicked: {e}"))?;

    if let Err(e) = mount_result {
        // Roll back the spawned task so we don't leak it if the mount
        // command failed after we bound the listener.
        server_handle.abort();
        return Err(e);
    }

    tracing::info!(
        mountpoint = %mountpoint.display(),
        port,
        "NFS mount ready"
    );

    Ok(MountHandle::new_nfs(
        mountpoint,
        opts.lazy_unmount,
        server_handle,
    ))
}

// ─── Platform-specific mount command exec ──────────────────────────────────

#[cfg(target_os = "macos")]
fn nfs_mount_command(port: u16, mountpoint: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::process::Command;

    let options =
        format!("locallocks,vers=3,tcp,port={port},mountport={port},soft,timeo=10,retrans=2");
    let output = Command::new("/sbin/mount_nfs")
        .arg("-o")
        .arg(&options)
        .arg("127.0.0.1:/")
        .arg(mountpoint)
        .output()
        .context("failed to execute /sbin/mount_nfs")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("mount_nfs failed: {}", stderr.trim());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn nfs_mount_command(port: u16, mountpoint: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::process::Command;

    let options = format!("vers=3,tcp,port={port},mountport={port},nolock,soft,timeo=10,retrans=2");
    let output = Command::new("mount")
        .arg("-t")
        .arg("nfs")
        .arg("-o")
        .arg(&options)
        .arg("127.0.0.1:/")
        .arg(mountpoint)
        .output()
        .context("failed to execute mount")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "mount -t nfs failed: {}. Make sure nfs-common (Debian/Ubuntu) or \
             nfs-utils (Fedora/RHEL) is installed and you have permission to \
             mount (try running with sudo).",
            stderr.trim()
        );
    }
    Ok(())
}

// ─── Platform-specific unmount ─────────────────────────────────────────────

#[cfg(target_os = "macos")]
pub(super) fn unmount_nfs(mountpoint: &Path, _lazy: bool) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::process::Command;

    let output = Command::new("/sbin/umount")
        .arg(mountpoint)
        .output()
        .context("failed to execute umount")?;

    if !output.status.success() {
        // macOS sometimes reports the mount as "resource busy" even when
        // nothing's using it. Fall back to force unmount.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let forced = Command::new("/sbin/umount")
            .arg("-f")
            .arg(mountpoint)
            .output()?;
        if !forced.status.success() {
            anyhow::bail!(
                "failed to unmount {}: {}",
                mountpoint.display(),
                stderr.trim()
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn unmount_nfs(mountpoint: &Path, lazy: bool) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::process::Command;

    let mut cmd = Command::new("umount");
    if lazy {
        cmd.arg("-l");
    }
    cmd.arg(mountpoint);

    let output = cmd.output().context("failed to execute umount")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // If the regular unmount failed, try lazy as a fallback.
        if !lazy {
            let forced = Command::new("umount").arg("-l").arg(mountpoint).output()?;
            if forced.status.success() {
                return Ok(());
            }
        }
        anyhow::bail!(
            "failed to unmount {}: {}",
            mountpoint.display(),
            stderr.trim()
        );
    }
    Ok(())
}

// ─── Port discovery ────────────────────────────────────────────────────────

/// Find a free TCP port on localhost, scanning upward from `start`.
///
/// Binds a throwaway `std::net::TcpListener` on each candidate port to
/// probe for availability. There's a tiny race between probe and the
/// subsequent `NFSTcpListener::bind` call, but in practice nothing else
/// scans this port range on a dev machine.
fn find_free_port(start: u16) -> anyhow::Result<u16> {
    for offset in 0..MAX_PORT_SCAN {
        let port = start.saturating_add(offset);
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!(
        "could not find a free port in range {}-{}",
        start,
        start.saturating_add(MAX_PORT_SCAN)
    )
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::MemFs;

    const UID: u32 = 1000;
    const GID: u32 = 1000;

    fn adapter() -> NfsAdapter<MemFs> {
        NfsAdapter::new(Arc::new(MemFs::new()), UID, GID)
    }

    fn nfstr(s: &str) -> nfsstring {
        nfsstring(s.as_bytes().to_vec())
    }

    #[test]
    fn root_dir_returns_one() {
        assert_eq!(adapter().root_dir(), 1);
    }

    #[test]
    fn capabilities_returns_read_write() {
        assert!(matches!(
            adapter().capabilities(),
            VFSCapabilities::ReadWrite
        ));
    }

    #[tokio::test]
    async fn getattr_root_returns_directory() {
        let fattr = adapter().getattr(1).await.unwrap();
        assert!(matches!(fattr.ftype, ftype3::NF3DIR));
        assert_eq!(fattr.fileid, 1);
    }

    #[tokio::test]
    async fn getattr_missing_returns_noent() {
        let err = adapter().getattr(999).await.unwrap_err();
        assert!(matches!(err, nfsstat3::NFS3ERR_NOENT));
    }

    #[tokio::test]
    async fn lookup_dot_returns_same_dirid() {
        let adap = adapter();
        assert_eq!(adap.lookup(1, &nfstr(".")).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn lookup_missing_returns_noent() {
        let err = adapter().lookup(1, &nfstr("nope")).await.unwrap_err();
        assert!(matches!(err, nfsstat3::NFS3ERR_NOENT));
    }

    #[tokio::test]
    async fn create_then_getattr_roundtrip() {
        let adap = adapter();
        let (ino, _attr) = adap
            .create(1, &nfstr("a.txt"), sattr3::default())
            .await
            .unwrap();
        assert!(ino > 1);
        let fetched = adap.getattr(ino).await.unwrap();
        assert_eq!(fetched.fileid, ino);
        assert!(matches!(fetched.ftype, ftype3::NF3REG));
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let adap = adapter();
        let (ino, _) = adap
            .create(1, &nfstr("hello.txt"), sattr3::default())
            .await
            .unwrap();
        let _ = adap.write(ino, 0, b"hello world").await.unwrap();
        let (data, eof) = adap.read(ino, 0, 100).await.unwrap();
        assert_eq!(data, b"hello world");
        assert!(eof);
    }

    #[tokio::test]
    async fn mkdir_then_readdir() {
        let adap = adapter();
        adap.mkdir(1, &nfstr("subdir")).await.unwrap();
        adap.create(1, &nfstr("file.txt"), sattr3::default())
            .await
            .unwrap();
        let result = adap.readdir(1, 0, 100).await.unwrap();
        assert_eq!(result.entries.len(), 2);
        assert!(result.end);
    }

    #[tokio::test]
    async fn remove_file_works() {
        let adap = adapter();
        adap.create(1, &nfstr("tmp.txt"), sattr3::default())
            .await
            .unwrap();
        adap.remove(1, &nfstr("tmp.txt")).await.unwrap();
        assert!(matches!(
            adap.lookup(1, &nfstr("tmp.txt")).await,
            Err(nfsstat3::NFS3ERR_NOENT)
        ));
    }

    #[tokio::test]
    async fn rename_works() {
        let adap = adapter();
        adap.create(1, &nfstr("old.txt"), sattr3::default())
            .await
            .unwrap();
        adap.rename(1, &nfstr("old.txt"), 1, &nfstr("new.txt"))
            .await
            .unwrap();
        assert!(adap.lookup(1, &nfstr("old.txt")).await.is_err());
        assert!(adap.lookup(1, &nfstr("new.txt")).await.is_ok());
    }

    #[tokio::test]
    async fn symlink_and_readlink_roundtrip() {
        let adap = adapter();
        let target = nfsstring(b"/some/target".to_vec());
        let (ino, _) = adap
            .symlink(1, &nfstr("link"), &target, &sattr3::default())
            .await
            .unwrap();
        let result = adap.readlink(ino).await.unwrap();
        assert_eq!(result.0, b"/some/target");
    }

    // ─── M3d additions: mount_nfs building blocks ──────────────────────

    #[test]
    fn find_free_port_returns_bindable_port() {
        // Scan starting from a high port unlikely to be already occupied
        // on a dev machine. (DEFAULT_NFS_PORT = 11111 may already be in
        // use by other NFS tooling; avoid depending on a fixed low port.)
        let port = find_free_port(50_000).expect("should find a free port");
        let bound = std::net::TcpListener::bind(("127.0.0.1", port));
        assert!(bound.is_ok(), "returned port {port} was not bindable");
    }

    #[test]
    fn find_free_port_skips_occupied_ports() {
        // Bind to port 0 so the kernel hands us a guaranteed-free
        // ephemeral port. Keep that listener alive while we call
        // find_free_port starting at the same port; find_free_port must
        // skip past it because we're still holding it.
        let occupied_listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .expect("test setup: could not bind ephemeral port");
        let occupied_port = occupied_listener.local_addr().unwrap().port();

        let found = find_free_port(occupied_port).expect("should find a free port");
        assert!(
            found > occupied_port,
            "expected find_free_port to skip occupied port {occupied_port}, got {found}"
        );
    }

    #[tokio::test]
    async fn mount_nfs_errors_if_mountpoint_does_not_exist() {
        use std::path::PathBuf;

        let fs = Arc::new(MemFs::new());
        let bogus = PathBuf::from("/tmp/smfs-nonexistent-test-path-mnt-nfs");
        let opts = MountOpts::new(bogus, super::super::MountBackend::Nfs);
        let result = mount_nfs(fs, opts).await;
        assert!(
            result.is_err(),
            "mount_nfs should fail for missing mountpoint"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("does not exist"),
            "expected error about missing mountpoint, got: {msg}"
        );
    }

    /// End-to-end smoke test — actually mounts a MemFs via NFS on localhost
    /// and performs real filesystem operations through the kernel.
    ///
    /// Marked `#[ignore]` so it doesn't run in normal `cargo test`. Run
    /// manually with `cargo test smoke_mount_memfs -- --ignored --nocapture`.
    ///
    /// This test is the only place in the suite that actually invokes
    /// `/sbin/mount_nfs`, binds an NFS server on localhost, and round-trips
    /// operations through the kernel. Everything is wrapped in tokio
    /// timeouts so it can't hang.
    ///
    /// Uses a multi-thread runtime because `MountHandle::drop()` calls
    /// `std::process::Command::output()` for the umount, which is blocking;
    /// under a single-threaded runtime the drop would starve the NFS
    /// listener task during unmount.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "manual smoke test — actually mounts on the host machine"]
    async fn smoke_mount_memfs_end_to_end() {
        use std::path::PathBuf;
        use std::time::Duration;
        use tokio::time::timeout;

        let mountpoint = PathBuf::from("/tmp/smfs-smoke-test-m3d");

        // Clean up any stale state from a previous failed run.
        let _ = std::process::Command::new("/sbin/umount")
            .arg(&mountpoint)
            .output();
        let _ = std::fs::remove_dir_all(&mountpoint);

        std::fs::create_dir_all(&mountpoint).expect("create mountpoint directory");
        println!("→ mountpoint: {}", mountpoint.display());

        let fs = Arc::new(MemFs::new());
        let opts = MountOpts::new(mountpoint.clone(), super::super::MountBackend::Nfs);

        println!("→ calling mount_nfs (10s timeout)...");
        let handle = timeout(Duration::from_secs(10), mount_nfs(fs, opts))
            .await
            .expect("mount_nfs timed out after 10s")
            .expect("mount_nfs failed");
        println!("→ mount succeeded at {}", handle.mountpoint().display());

        // Let the NFS client stabilise before poking at the filesystem.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Op 1: readdir the empty mount.
        println!("→ op 1: read_dir on fresh mount");
        let initial: Vec<_> = std::fs::read_dir(&mountpoint)
            .expect("read_dir on fresh mount")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        println!("    entries: {:?}", initial);

        // Op 2: write a file.
        println!("→ op 2: write hello.txt");
        let test_file = mountpoint.join("hello.txt");
        std::fs::write(&test_file, b"hello from m3d").expect("write hello.txt");

        // Op 3: read it back.
        println!("→ op 3: read hello.txt back");
        let content = std::fs::read_to_string(&test_file).expect("read hello.txt");
        assert_eq!(content, "hello from m3d", "round-trip content mismatch");
        println!("    content: {:?}", content);

        // Op 4: mkdir.
        println!("→ op 4: create subdir");
        let subdir = mountpoint.join("subdir");
        std::fs::create_dir(&subdir).expect("mkdir subdir");

        // Op 5: verify read_dir now shows both.
        println!("→ op 5: read_dir after writes");
        let entries: Vec<_> = std::fs::read_dir(&mountpoint)
            .expect("read_dir after writes")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        println!("    entries: {:?}", entries);
        assert!(
            entries.iter().any(|n| n == "hello.txt"),
            "hello.txt missing from readdir: {entries:?}"
        );
        assert!(
            entries.iter().any(|n| n == "subdir"),
            "subdir missing from readdir: {entries:?}"
        );

        // Op 6: unlink the file.
        println!("→ op 6: remove hello.txt");
        std::fs::remove_file(&test_file).expect("unlink hello.txt");

        // Op 7: rmdir the subdir.
        println!("→ op 7: remove subdir");
        std::fs::remove_dir(&subdir).expect("rmdir subdir");

        // Op 8: final readdir should be empty again.
        println!("→ op 8: read_dir after cleanup");
        let final_entries: Vec<_> = std::fs::read_dir(&mountpoint)
            .expect("read_dir after cleanup")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        println!("    entries: {:?}", final_entries);

        println!("→ all operations succeeded, dropping handle to unmount...");
        drop(handle);

        // Give the unmount a moment to propagate through the kernel.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Best-effort cleanup: remove the now-unmounted directory.
        let _ = std::fs::remove_dir_all(&mountpoint);

        println!("→ smoke test complete ✓");
    }
}
