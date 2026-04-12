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

use std::sync::Arc;

use async_trait::async_trait;
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfsstring, nfstime3, sattr3, set_atime,
    set_gid3, set_mode3, set_mtime, set_size3, set_uid3, specdata3,
};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

use crate::vfs::{FileAttr, FileSystem, SetAttr, TimeOrNow, Timestamp, VfsError};

use super::{MountHandle, MountOpts};

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

// ─── mount_nfs — still stubbed until M3d ───────────────────────────────────

/// Mount a filesystem using the NFSv3 backend.
///
/// Stub for M3b/M3c — the real implementation lands in M3d (adapter wiring
/// + mount command exec). Currently always returns "not implemented".
#[allow(clippy::needless_pass_by_value)] // signature matches the eventual real one
pub async fn mount_nfs<F>(fs: Arc<F>, opts: MountOpts) -> anyhow::Result<MountHandle>
where
    F: FileSystem + 'static,
{
    let _ = (fs, opts);
    anyhow::bail!("NFS mount not implemented yet — lands in M3d")
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
}
