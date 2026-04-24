//! FUSE mount adapter (Linux only).
//!
//! Bridges [`fuser::Filesystem`] callbacks to our
//! [`vfs::FileSystem`](crate::vfs::FileSystem) trait methods. Each FUSE
//! callback is synchronous; we bridge to our async trait via
//! [`tokio::runtime::Handle::block_on`] on the fuser-managed callback
//! thread (which is not itself a tokio worker, so `block_on` is legal).
//!
//! ## Build gating
//!
//! This file is only compiled when `target_os = "linux"`. On macOS, the
//! `pub mod fuse;` declaration in the parent module is `#[cfg]`-gated out
//! and `fuser` isn't in the dependency tree at all.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    Config, Errno, FileHandle, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, OpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, SessionACL,
    TimeOrNow as FuserTimeOrNow,
};
use parking_lot::Mutex;

use crate::vfs::{BoxedFile, FileAttr, FileSystem, FileType, SetAttr, TimeOrNow, Timestamp};

use super::{MountHandle, MountOpts};

/// Attribute cache TTL. We use a long duration because the daemon is the
/// only writer — there's no outside process that can invalidate the
/// kernel's dcache, so entries never expire on their own.
const TTL: Duration = Duration::from_secs(60 * 60 * 24 * 365);

// ─── Type conversion helpers ───────────────────────────────────────────────

/// Convert our [`FileType`] into fuser's wider `FileType` enum.
fn file_type_to_fuser(ft: FileType) -> fuser::FileType {
    match ft {
        FileType::Regular => fuser::FileType::RegularFile,
        FileType::Directory => fuser::FileType::Directory,
        FileType::Symlink => fuser::FileType::Symlink,
    }
}

/// Convert a VFS [`Timestamp`] into a `SystemTime` for fuser.
fn timestamp_to_system_time(ts: Timestamp) -> SystemTime {
    UNIX_EPOCH + Duration::new(ts.sec.max(0) as u64, ts.nsec)
}

/// Convert our [`FileAttr`] into fuser's wire-format `FileAttr` struct.
fn file_attr_to_fuser_attr(attr: &FileAttr) -> fuser::FileAttr {
    fuser::FileAttr {
        ino: INodeNo(attr.ino),
        size: attr.size,
        blocks: attr.blocks,
        atime: timestamp_to_system_time(attr.atime),
        mtime: timestamp_to_system_time(attr.mtime),
        ctime: timestamp_to_system_time(attr.ctime),
        crtime: timestamp_to_system_time(attr.ctime),
        kind: file_type_to_fuser(attr.file_type()),
        perm: (attr.mode & 0o7777) as u16,
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        rdev: attr.rdev,
        blksize: attr.blksize,
        flags: 0,
    }
}

/// Convert fuser's `TimeOrNow` into ours.
fn fuser_time_to_vfs_time(t: FuserTimeOrNow) -> TimeOrNow {
    match t {
        FuserTimeOrNow::Now => TimeOrNow::Now,
        FuserTimeOrNow::SpecificTime(st) => {
            let d = st.duration_since(UNIX_EPOCH).unwrap_or_default();
            TimeOrNow::Time(Timestamp {
                sec: d.as_secs() as i64,
                nsec: d.subsec_nanos(),
            })
        }
    }
}

/// Convert a `VfsError`'s `i32` errno into fuser's `Errno` type.
fn vfs_errno(e: &crate::vfs::VfsError) -> Errno {
    let code = e.to_errno();
    // Errno::from_i32 defaults to EIO for non-positive values.
    Errno::from_i32(code)
}

// ─── FUSE adapter type ────────────────────────────────────────────────────

/// Adapter that implements [`fuser::Filesystem`] by delegating to our
/// [`crate::vfs::FileSystem`] trait.
///
/// FUSE callbacks run on fuser-managed threads (outside tokio), so we
/// carry a [`tokio::runtime::Handle`] and use `rt.block_on(async { ... })`
/// to call async trait methods synchronously from those threads. This
/// requires the caller of `mount_fuse` to be inside a tokio runtime
/// context when constructing the adapter.
pub struct FuseAdapter<F: FileSystem + 'static> {
    fs: Arc<F>,
    rt: tokio::runtime::Handle,
    open_files: Arc<Mutex<HashMap<u64, BoxedFile>>>,
    next_fh: Arc<AtomicU64>,
    default_uid: u32,
    default_gid: u32,
}

impl<F: FileSystem + 'static> FuseAdapter<F> {
    /// Construct a new adapter wrapping the given filesystem.
    ///
    /// `rt` must be a handle to a tokio runtime that will be alive for
    /// the lifetime of this adapter. `default_uid`/`default_gid` are used
    /// for ownership of files/dirs/symlinks created through the mount
    /// when the caller doesn't specify otherwise.
    pub fn new(fs: Arc<F>, rt: tokio::runtime::Handle, default_uid: u32, default_gid: u32) -> Self {
        Self {
            fs,
            rt,
            open_files: Arc::new(Mutex::new(HashMap::new())),
            next_fh: Arc::new(AtomicU64::new(1)),
            default_uid,
            default_gid,
        }
    }

    /// Allocate a new unique file handle and store the backing
    /// [`BoxedFile`].
    fn register_handle(&self, file: BoxedFile) -> FileHandle {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.open_files.lock().insert(fh, file);
        FileHandle(fh)
    }

    /// Look up the [`BoxedFile`] for an open handle.
    fn get_handle(&self, fh: FileHandle) -> Option<BoxedFile> {
        self.open_files.lock().get(&fh.0).cloned()
    }

    /// Remove and drop the [`BoxedFile`] for a handle (on release).
    fn release_handle(&self, fh: FileHandle) {
        self.open_files.lock().remove(&fh.0);
    }
}

// Manual `Debug` impl because `F: FileSystem` doesn't require `Debug`
// as a supertrait. Print only the fields we can safely show.
impl<F: FileSystem + 'static> std::fmt::Debug for FuseAdapter<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuseAdapter")
            .field("default_uid", &self.default_uid)
            .field("default_gid", &self.default_gid)
            .field("open_files_len", &self.open_files.lock().len())
            .finish_non_exhaustive()
    }
}

// ─── fuser::Filesystem implementation ─────────────────────────────────────

impl<F: FileSystem + 'static> Filesystem for FuseAdapter<F> {
    // ─── Lifecycle ─────────────────────────────────────────────────────

    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // Enable the same performance capabilities AgentFS enables.
        let _ = config.add_capabilities(
            InitFlags::FUSE_ASYNC_READ
                | InitFlags::FUSE_WRITEBACK_CACHE
                | InitFlags::FUSE_PARALLEL_DIROPS
                | InitFlags::FUSE_CACHE_SYMLINKS
                | InitFlags::FUSE_NO_OPENDIR_SUPPORT,
        );
        Ok(())
    }

    fn destroy(&mut self) {
        // Drop the whole open-file table on unmount. Each `BoxedFile`'s
        // Drop releases its underlying handle.
        self.open_files.lock().clear();
    }

    // ─── Name resolution + metadata ───────────────────────────────────

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let parent_ino = parent.0;
        let result = self
            .rt
            .block_on(async move { fs.lookup(parent_ino, &name_owned).await });
        match result {
            Ok(Some(attr)) => reply.entry(&TTL, &file_attr_to_fuser_attr(&attr), Generation(0)),
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    fn forget(&self, _req: &Request, _ino: INodeNo, _nlookup: u64) {
        // No-op: we don't reference-count kernel inode lookups.
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let fs = self.fs.clone();
        let result = self.rt.block_on(async move { fs.getattr(ino.0).await });
        match result {
            Ok(Some(attr)) => reply.attr(&TTL, &file_attr_to_fuser_attr(&attr)),
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    #[allow(clippy::too_many_arguments)] // dictated by fuser's trait signature
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<FuserTimeOrNow>,
        mtime: Option<FuserTimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let set_attr = SetAttr {
            mode,
            uid,
            gid,
            size,
            atime: atime.map(fuser_time_to_vfs_time),
            mtime: mtime.map(fuser_time_to_vfs_time),
        };
        let fs = self.fs.clone();
        let result = self
            .rt
            .block_on(async move { fs.setattr(ino.0, set_attr).await });
        match result {
            Ok(attr) => reply.attr(&TTL, &file_attr_to_fuser_attr(&attr)),
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let fs = self.fs.clone();
        let result = self.rt.block_on(async move { fs.readlink(ino.0).await });
        match result {
            Ok(Some(target)) => reply.data(target.as_bytes()),
            Ok(None) => reply.error(Errno::ENOENT),
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    // ─── Directory operations ─────────────────────────────────────────

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let name_owned = name_str.to_string();
        let fs = self.fs.clone();
        let uid = self.default_uid;
        let gid = self.default_gid;
        let result = self
            .rt
            .block_on(async move { fs.mkdir(parent.0, &name_owned, mode, uid, gid).await });
        match result {
            Ok(attr) => reply.entry(&TTL, &file_attr_to_fuser_attr(&attr), Generation(0)),
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let name_owned = name_str.to_string();
        let fs = self.fs.clone();
        let result = self
            .rt
            .block_on(async move { fs.rmdir(parent.0, &name_owned).await });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        // FUSE_NO_OPENDIR_SUPPORT is enabled in init, but some kernels
        // may still call this. We don't track dir handles, so return
        // fh=0 and zero flags.
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let fs = self.fs.clone();
        let result = self
            .rt
            .block_on(async move { fs.readdir_plus(ino.0).await });
        let entries = match result {
            Ok(Some(entries)) => entries,
            Ok(None) => {
                reply.error(Errno::ENOTDIR);
                return;
            }
            Err(e) => {
                reply.error(vfs_errno(&e));
                return;
            }
        };

        // FUSE readdir is offset-based. `offset` is the cursor the kernel
        // wants us to resume from; we return entries starting at that
        // offset. `reply.add` returns `true` when the reply buffer is
        // full, which is our signal to stop.
        for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
            let next_offset = (i + 1) as u64;
            let full = reply.add(
                INodeNo(entry.attr.ino),
                next_offset,
                file_type_to_fuser(entry.attr.file_type()),
                &entry.name,
            );
            if full {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    // ─── File operations (handle-based) ───────────────────────────────

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let fs = self.fs.clone();
        // Our VFS open takes i32 flags; pass 0 for now (read-write).
        let result = self.rt.block_on(async move { fs.open(ino.0, 0).await });
        match result {
            Ok(file) => {
                let fh = self.register_handle(file);
                reply.opened(fh, FopenFlags::empty());
            }
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    #[allow(clippy::too_many_arguments)] // fuser trait shape
    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let name_owned = name_str.to_string();
        let fs = self.fs.clone();
        let uid = self.default_uid;
        let gid = self.default_gid;
        let result = self
            .rt
            .block_on(async move { fs.create_file(parent.0, &name_owned, mode, uid, gid).await });
        match result {
            Ok((attr, file)) => {
                let fh = self.register_handle(file);
                reply.created(
                    &TTL,
                    &file_attr_to_fuser_attr(&attr),
                    Generation(0),
                    fh,
                    FopenFlags::empty(),
                );
            }
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let Some(file) = self.get_handle(fh) else {
            reply.error(Errno::EBADF);
            return;
        };
        let result = self
            .rt
            .block_on(async move { file.read(offset, size as usize).await });
        match result {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let Some(file) = self.get_handle(fh) else {
            reply.error(Errno::EBADF);
            return;
        };
        let data_owned = data.to_vec();
        let result = self
            .rt
            .block_on(async move { file.write(offset, &data_owned).await });
        match result {
            Ok(written) => reply.written(written),
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        let Some(file) = self.get_handle(fh) else {
            reply.error(Errno::EBADF);
            return;
        };
        let result = self.rt.block_on(async move { file.flush().await });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.release_handle(fh);
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let Some(file) = self.get_handle(fh) else {
            reply.error(Errno::EBADF);
            return;
        };
        let result = self.rt.block_on(async move { file.fsync().await });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    // ─── Remove + rename ─────────────────────────────────────────────

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let name_owned = name_str.to_string();
        let fs = self.fs.clone();
        let result = self
            .rt
            .block_on(async move { fs.unlink(parent.0, &name_owned).await });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (Some(old), Some(new)) = (name.to_str(), newname.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let old_owned = old.to_string();
        let new_owned = new.to_string();
        let fs = self.fs.clone();
        let result = self.rt.block_on(async move {
            fs.rename(parent.0, &old_owned, newparent.0, &new_owned)
                .await
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    // ─── Symbolic + hard links ────────────────────────────────────────

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let Some(name_str) = link_name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let Some(target_str) = target.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let name_owned = name_str.to_string();
        let target_owned = target_str.to_string();
        let fs = self.fs.clone();
        let uid = self.default_uid;
        let gid = self.default_gid;
        let result = self.rt.block_on(async move {
            fs.symlink(parent.0, &name_owned, &target_owned, uid, gid)
                .await
        });
        match result {
            Ok(attr) => reply.entry(&TTL, &file_attr_to_fuser_attr(&attr), Generation(0)),
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let Some(name_str) = newname.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let name_owned = name_str.to_string();
        let fs = self.fs.clone();
        let result = self
            .rt
            .block_on(async move { fs.link(ino.0, newparent.0, &name_owned).await });
        match result {
            Ok(attr) => reply.entry(&TTL, &file_attr_to_fuser_attr(&attr), Generation(0)),
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    // ─── Filesystem-wide ──────────────────────────────────────────────

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let fs = self.fs.clone();
        let result = self.rt.block_on(async move { fs.statfs().await });
        match result {
            Ok(stats) => {
                reply.statfs(
                    stats.bytes_used / 4096, // blocks
                    u64::MAX / 2,            // bfree
                    u64::MAX / 2,            // bavail
                    stats.inodes,            // files
                    u64::MAX / 2,            // ffree
                    4096,                    // bsize
                    255,                     // namelen
                    4096,                    // frsize
                );
            }
            Err(e) => reply.error(vfs_errno(&e)),
        }
    }

    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        // Supermemory has no FIFOs, character devices, block devices,
        // or sockets. Return ENOSYS for any such attempt.
        reply.error(Errno::ENOSYS);
    }
}

// ─── mount_fuse ───────────────────────────────────────────────────────────

/// Mount a filesystem using the FUSE backend (Linux only).
///
/// Creates a [`FuseAdapter`] wrapping the given filesystem, builds a
/// [`Config`] from `opts`, and calls `fuser::spawn_mount2` on a blocking
/// thread (the mount syscall is blocking). Returns a [`MountHandle`]
/// whose `Drop` unmounts automatically via `BackgroundSession::Drop`.
pub async fn mount_fuse<F>(fs: Arc<F>, opts: MountOpts) -> anyhow::Result<MountHandle>
where
    F: FileSystem + 'static,
{
    use anyhow::Context;
    use fuser::MountOption;

    // 1. Validate mountpoint exists.
    if !opts.mountpoint.exists() {
        anyhow::bail!("mountpoint does not exist: {}", opts.mountpoint.display());
    }

    // 2. Canonicalize the path.
    let mountpoint = std::fs::canonicalize(&opts.mountpoint).with_context(|| {
        format!(
            "failed to canonicalize mountpoint {}",
            opts.mountpoint.display()
        )
    })?;

    // 3. Resolve uid/gid defaults (0 if not specified — the binary
    //    crate should populate MountOpts with real values).
    let default_uid = opts.uid.unwrap_or(0);
    let default_gid = opts.gid.unwrap_or(0);

    // 4. Build fuser Config from MountOpts.
    let mut mount_options = vec![
        MountOption::FSName(opts.fsname.clone()),
        MountOption::DefaultPermissions,
        MountOption::RW,
    ];
    if opts.auto_unmount {
        mount_options.push(MountOption::AutoUnmount);
    }

    // Determine the session ACL from allow_other / allow_root flags.
    // SessionACL replaces the old MountOption::AllowOther / AllowRoot
    // in fuser 0.17.
    //
    // fuser 0.17 requires `acl != Owner` when AutoUnmount is enabled
    // (AutoUnmount uses fusermount3 which needs allow_other). If the
    // caller asked for auto_unmount without explicitly setting allow_other
    // or allow_root, we default to All so the mount doesn't fail.
    let acl = if opts.allow_other {
        SessionACL::All
    } else if opts.allow_root {
        SessionACL::RootAndOwner
    } else if opts.auto_unmount {
        SessionACL::All
    } else {
        SessionACL::Owner
    };

    let mut config = Config::default();
    config.mount_options = mount_options;
    config.acl = acl;

    // 5. Create the adapter with a handle to the current tokio runtime.
    let rt = tokio::runtime::Handle::current();
    let adapter = FuseAdapter::new(fs, rt, default_uid, default_gid);

    // 6. Call fuser::spawn_mount2 inside spawn_blocking — it performs a
    //    blocking mount syscall (opens /dev/fuse, calls mount(2)).
    let mp = mountpoint.clone();
    let timeout_dur = opts.timeout;
    let session = tokio::time::timeout(
        timeout_dur,
        tokio::task::spawn_blocking(move || fuser::spawn_mount2(adapter, &mp, &config)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("FUSE mount timed out after {timeout_dur:?}"))?
    .map_err(|e| anyhow::anyhow!("spawn_mount2 task panicked: {e}"))?
    .with_context(|| format!("fuser::spawn_mount2 failed for {}", mountpoint.display()))?;

    tracing::info!(mountpoint = %mountpoint.display(), "FUSE mount ready");

    Ok(MountHandle::new_fuse(
        mountpoint,
        opts.lazy_unmount,
        session,
    ))
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::MemFs;

    /// End-to-end smoke test — actually mounts a MemFs via FUSE and
    /// performs real filesystem operations through the kernel.
    ///
    /// Marked `#[ignore]` so it doesn't run in normal `cargo test`. Run
    /// manually on a Linux machine with:
    ///
    ///   cargo test -p smfs-core smoke_fuse -- --ignored --nocapture
    ///
    /// Requires `/dev/fuse` to exist and `fusermount3` to be installed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "manual smoke test — requires /dev/fuse on Linux"]
    async fn smoke_fuse_mount_memfs_end_to_end() {
        use std::path::PathBuf;
        use std::time::Duration;

        let mountpoint = PathBuf::from("/tmp/smfs-fuse-smoke-test-m3f");

        // Clean up stale state from a previous failed run.
        let _ = std::process::Command::new("fusermount3")
            .arg("-u")
            .arg(&mountpoint)
            .output();
        let _ = std::fs::remove_dir_all(&mountpoint);

        std::fs::create_dir_all(&mountpoint).expect("create mountpoint directory");
        println!("→ mountpoint: {}", mountpoint.display());

        let fs = Arc::new(MemFs::new());
        let opts = MountOpts::new(mountpoint.clone(), super::super::MountBackend::Fuse);

        println!("→ calling mount_fuse (10s timeout)...");
        let handle = tokio::time::timeout(Duration::from_secs(10), mount_fuse(fs, opts))
            .await
            .expect("mount_fuse timed out after 10s")
            .expect("mount_fuse failed");
        println!("→ mount succeeded at {}", handle.mountpoint().display());

        // Brief pause for kernel VFS caches to settle.
        tokio::time::sleep(Duration::from_millis(200)).await;

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
        std::fs::write(&test_file, b"hello from m3f").expect("write hello.txt");

        // Op 3: read it back.
        println!("→ op 3: read hello.txt back");
        let content = std::fs::read_to_string(&test_file).expect("read hello.txt");
        assert_eq!(content, "hello from m3f", "round-trip content mismatch");
        println!("    content: {:?}", content);

        // Op 4: mkdir.
        println!("→ op 4: create subdir");
        let subdir = mountpoint.join("subdir");
        std::fs::create_dir(&subdir).expect("mkdir subdir");

        // Op 5: verify read_dir now shows both.
        println!("→ op 5: read_dir after writes");
        let mut entries: Vec<_> = std::fs::read_dir(&mountpoint)
            .expect("read_dir after writes")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        entries.sort();
        println!("    entries: {:?}", entries);
        assert_eq!(entries, vec!["hello.txt", "subdir"]);

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

        // Give the unmount a moment to propagate.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let _ = std::fs::remove_dir_all(&mountpoint);
        println!("→ FUSE smoke test complete ✓");
    }
}
