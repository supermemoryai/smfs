//! [`MemFs`] — an in-memory reference implementation of the [`FileSystem`] trait.
//!
//! This module exists for two purposes:
//!
//! 1. **Test fixture**: every unit test that exercises code built on top of
//!    the `FileSystem` trait runs against `MemFs` for speed and isolation.
//! 2. **M4 mount target**: the first real `smfs mount` in M4 mounts a `MemFs`
//!    to prove the FUSE/NFS plumbing works before the SQLite-backed
//!    implementation lands in M5.
//!
//! `MemFs` is not a production storage layer. It has no persistence, no
//! journaling, no eviction, and no concurrency-safe transactions beyond a
//! single mutex guarding the whole state. It's optimised for being obviously
//! correct, not fast.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use super::error::{VfsError, VfsResult};
use super::mode::{MAX_NAME_LEN, S_IFDIR, S_IFMT, S_IFREG};
use super::traits::{BoxedFile, File, FileSystem};
use super::types::{DirEntry, FileAttr, FilesystemStats, SetAttr, TimeOrNow, Timestamp};

/// The root inode number. POSIX and FUSE both use 1.
const ROOT_INO: u64 = 1;

/// An in-memory filesystem implementing the [`FileSystem`] trait.
///
/// Every operation takes a `parking_lot::Mutex` guard for the entire
/// state, does synchronous work, and releases the guard before returning.
/// Methods are `async fn` only to satisfy the trait contract — no awaits
/// happen inside.
#[derive(Debug, Clone)]
pub struct MemFs {
    inner: Arc<Mutex<Inner>>,
}

impl Default for MemFs {
    fn default() -> Self {
        Self::new()
    }
}

impl MemFs {
    /// Create a new empty filesystem containing only the root directory.
    pub fn new() -> Self {
        let mut nodes = HashMap::new();
        let root = Node::Directory {
            attr: FileAttr::new_dir(ROOT_INO, 0, 0),
            children: HashMap::new(),
        };
        nodes.insert(ROOT_INO, root);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                nodes,
                next_ino: ROOT_INO + 1,
            })),
        }
    }
}

#[derive(Debug)]
struct Inner {
    nodes: HashMap<u64, Node>,
    next_ino: u64,
}

#[derive(Debug)]
enum Node {
    File {
        attr: FileAttr,
        data: Vec<u8>,
    },
    Directory {
        attr: FileAttr,
        children: HashMap<String, u64>,
    },
    Symlink {
        attr: FileAttr,
        target: String,
    },
}

impl Node {
    fn attr(&self) -> &FileAttr {
        match self {
            Self::File { attr, .. } => attr,
            Self::Directory { attr, .. } => attr,
            Self::Symlink { attr, .. } => attr,
        }
    }

    fn attr_mut(&mut self) -> &mut FileAttr {
        match self {
            Self::File { attr, .. } => attr,
            Self::Directory { attr, .. } => attr,
            Self::Symlink { attr, .. } => attr,
        }
    }

    fn is_directory(&self) -> bool {
        matches!(self, Self::Directory { .. })
    }
}

/// Reject names that are empty, too long, contain a path separator, or contain a NUL byte.
fn validate_name(name: &str) -> VfsResult<()> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(VfsError::InvalidPath(format!("invalid name: {name:?}")));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(VfsError::NameTooLong(name.len()));
    }
    if name.contains('/') || name.contains('\0') {
        return Err(VfsError::InvalidPath(format!("invalid name: {name:?}")));
    }
    Ok(())
}

/// Compute the 512-byte block count for a byte size, rounded up.
fn blocks_for(size: u64) -> u64 {
    size.div_ceil(512)
}

#[async_trait]
impl FileSystem for MemFs {
    async fn lookup(&self, parent_ino: u64, name: &str) -> VfsResult<Option<FileAttr>> {
        validate_name(name)?;
        let guard = self.inner.lock();
        let parent = guard.nodes.get(&parent_ino).ok_or(VfsError::NotFound)?;
        let Node::Directory { children, .. } = parent else {
            return Err(VfsError::NotADirectory);
        };
        let Some(&child_ino) = children.get(name) else {
            return Ok(None);
        };
        Ok(guard.nodes.get(&child_ino).map(|n| n.attr().clone()))
    }

    async fn getattr(&self, ino: u64) -> VfsResult<Option<FileAttr>> {
        let guard = self.inner.lock();
        Ok(guard.nodes.get(&ino).map(|n| n.attr().clone()))
    }

    async fn setattr(&self, ino: u64, attr: SetAttr) -> VfsResult<FileAttr> {
        let mut guard = self.inner.lock();
        let node = guard.nodes.get_mut(&ino).ok_or(VfsError::NotFound)?;

        // Size change only applies to regular files and touches data+attr together.
        if let Some(new_size) = attr.size {
            match node {
                Node::File {
                    attr: file_attr,
                    data,
                } => {
                    data.resize(new_size as usize, 0);
                    file_attr.size = new_size;
                    file_attr.blocks = blocks_for(new_size);
                }
                Node::Directory { .. } => return Err(VfsError::IsADirectory),
                Node::Symlink { .. } => return Err(VfsError::NotSupported),
            }
        }

        let node_attr = node.attr_mut();
        if let Some(mode) = attr.mode {
            // Preserve the file type bits (upper), update permission bits (lower 12).
            node_attr.mode = (node_attr.mode & S_IFMT) | (mode & !S_IFMT);
        }
        if let Some(uid) = attr.uid {
            node_attr.uid = uid;
        }
        if let Some(gid) = attr.gid {
            node_attr.gid = gid;
        }
        if let Some(time) = attr.atime {
            node_attr.atime = match time {
                TimeOrNow::Now => Timestamp::now(),
                TimeOrNow::Time(t) => t,
            };
        }
        if let Some(time) = attr.mtime {
            node_attr.mtime = match time {
                TimeOrNow::Now => Timestamp::now(),
                TimeOrNow::Time(t) => t,
            };
        }
        node_attr.ctime = Timestamp::now();

        Ok(node_attr.clone())
    }

    async fn readdir(&self, ino: u64) -> VfsResult<Option<Vec<String>>> {
        let guard = self.inner.lock();
        let Some(node) = guard.nodes.get(&ino) else {
            return Ok(None);
        };
        match node {
            Node::Directory { children, .. } => {
                let mut names: Vec<String> = children.keys().cloned().collect();
                names.sort();
                Ok(Some(names))
            }
            _ => Ok(None),
        }
    }

    async fn readdir_plus(&self, ino: u64) -> VfsResult<Option<Vec<DirEntry>>> {
        let guard = self.inner.lock();
        let Some(node) = guard.nodes.get(&ino) else {
            return Ok(None);
        };
        let Node::Directory { children, .. } = node else {
            return Ok(None);
        };
        let mut entries: Vec<DirEntry> = children
            .iter()
            .filter_map(|(name, child_ino)| {
                guard.nodes.get(child_ino).map(|child| DirEntry {
                    name: name.clone(),
                    attr: child.attr().clone(),
                })
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Some(entries))
    }

    async fn mkdir(
        &self,
        parent_ino: u64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> VfsResult<FileAttr> {
        validate_name(name)?;
        let mut guard = self.inner.lock();

        // Validate parent: must exist, must be a directory, must not already contain `name`.
        match guard.nodes.get(&parent_ino) {
            Some(Node::Directory { children, .. }) => {
                if children.contains_key(name) {
                    return Err(VfsError::AlreadyExists);
                }
            }
            Some(_) => return Err(VfsError::NotADirectory),
            None => return Err(VfsError::NotFound),
        }

        let ino = guard.next_ino;
        guard.next_ino += 1;

        let mut new_attr = FileAttr::new_dir(ino, uid, gid);
        // Preserve file-type bits (S_IFDIR) and take permission bits from the caller.
        new_attr.mode = S_IFDIR | (mode & 0o7777);
        let new_node = Node::Directory {
            attr: new_attr.clone(),
            children: HashMap::new(),
        };
        guard.nodes.insert(ino, new_node);

        // Link into parent and bump parent nlink (`..` counts for us).
        let now = Timestamp::now();
        if let Some(Node::Directory { children, attr }) = guard.nodes.get_mut(&parent_ino) {
            children.insert(name.to_string(), ino);
            attr.mtime = now;
            attr.ctime = now;
            attr.nlink += 1;
        }

        Ok(new_attr)
    }

    async fn rmdir(&self, parent_ino: u64, name: &str) -> VfsResult<()> {
        validate_name(name)?;
        let mut guard = self.inner.lock();

        let child_ino = match guard.nodes.get(&parent_ino) {
            Some(Node::Directory { children, .. }) => {
                *children.get(name).ok_or(VfsError::NotFound)?
            }
            Some(_) => return Err(VfsError::NotADirectory),
            None => return Err(VfsError::NotFound),
        };

        match guard.nodes.get(&child_ino) {
            Some(Node::Directory { children, .. }) => {
                if !children.is_empty() {
                    return Err(VfsError::NotEmpty);
                }
            }
            Some(_) => return Err(VfsError::NotADirectory),
            None => return Err(VfsError::NotFound),
        }

        let now = Timestamp::now();
        if let Some(Node::Directory { children, attr }) = guard.nodes.get_mut(&parent_ino) {
            children.remove(name);
            attr.mtime = now;
            attr.ctime = now;
            if attr.nlink > 2 {
                attr.nlink -= 1;
            }
        }
        guard.nodes.remove(&child_ino);
        Ok(())
    }

    async fn open(&self, ino: u64, flags: i32) -> VfsResult<BoxedFile> {
        let guard = self.inner.lock();
        match guard.nodes.get(&ino) {
            Some(Node::File { .. }) => {}
            Some(Node::Directory { .. }) => return Err(VfsError::IsADirectory),
            Some(Node::Symlink { .. }) => return Err(VfsError::NotSupported),
            None => return Err(VfsError::NotFound),
        }
        drop(guard);
        Ok(Arc::new(MemFile {
            inner: self.inner.clone(),
            ino,
            flags,
        }))
    }

    async fn create_file(
        &self,
        parent_ino: u64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> VfsResult<(FileAttr, BoxedFile)> {
        validate_name(name)?;
        let mut guard = self.inner.lock();

        match guard.nodes.get(&parent_ino) {
            Some(Node::Directory { children, .. }) => {
                if children.contains_key(name) {
                    return Err(VfsError::AlreadyExists);
                }
            }
            Some(_) => return Err(VfsError::NotADirectory),
            None => return Err(VfsError::NotFound),
        }

        let ino = guard.next_ino;
        guard.next_ino += 1;

        let mut new_attr = FileAttr::new_file(ino, uid, gid);
        new_attr.mode = S_IFREG | (mode & 0o7777);
        guard.nodes.insert(
            ino,
            Node::File {
                attr: new_attr.clone(),
                data: Vec::new(),
            },
        );

        let now = Timestamp::now();
        if let Some(Node::Directory { children, attr }) = guard.nodes.get_mut(&parent_ino) {
            children.insert(name.to_string(), ino);
            attr.mtime = now;
            attr.ctime = now;
        }
        drop(guard);

        let handle: BoxedFile = Arc::new(MemFile {
            inner: self.inner.clone(),
            ino,
            flags: 0,
        });
        Ok((new_attr, handle))
    }

    async fn unlink(&self, parent_ino: u64, name: &str) -> VfsResult<()> {
        validate_name(name)?;
        let mut guard = self.inner.lock();

        let child_ino = match guard.nodes.get(&parent_ino) {
            Some(Node::Directory { children, .. }) => {
                *children.get(name).ok_or(VfsError::NotFound)?
            }
            Some(_) => return Err(VfsError::NotADirectory),
            None => return Err(VfsError::NotFound),
        };

        // Unlink is for non-directories only.
        match guard.nodes.get(&child_ino) {
            Some(Node::Directory { .. }) => return Err(VfsError::IsADirectory),
            Some(_) => {}
            None => return Err(VfsError::NotFound),
        }

        let now = Timestamp::now();
        if let Some(Node::Directory { children, attr }) = guard.nodes.get_mut(&parent_ino) {
            children.remove(name);
            attr.mtime = now;
            attr.ctime = now;
        }

        if let Some(node) = guard.nodes.get_mut(&child_ino) {
            let nlink = &mut node.attr_mut().nlink;
            if *nlink > 1 {
                *nlink -= 1;
                node.attr_mut().ctime = now;
            } else {
                guard.nodes.remove(&child_ino);
            }
        }
        Ok(())
    }

    async fn readlink(&self, ino: u64) -> VfsResult<Option<String>> {
        let guard = self.inner.lock();
        let Some(node) = guard.nodes.get(&ino) else {
            return Ok(None);
        };
        match node {
            Node::Symlink { target, .. } => Ok(Some(target.clone())),
            _ => Err(VfsError::NotASymlink),
        }
    }

    async fn symlink(
        &self,
        parent_ino: u64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> VfsResult<FileAttr> {
        validate_name(name)?;
        let mut guard = self.inner.lock();

        match guard.nodes.get(&parent_ino) {
            Some(Node::Directory { children, .. }) => {
                if children.contains_key(name) {
                    return Err(VfsError::AlreadyExists);
                }
            }
            Some(_) => return Err(VfsError::NotADirectory),
            None => return Err(VfsError::NotFound),
        }

        let ino = guard.next_ino;
        guard.next_ino += 1;

        let new_attr = FileAttr::new_symlink(ino, target.len() as u64, uid, gid);
        guard.nodes.insert(
            ino,
            Node::Symlink {
                attr: new_attr.clone(),
                target: target.to_string(),
            },
        );

        let now = Timestamp::now();
        if let Some(Node::Directory { children, attr }) = guard.nodes.get_mut(&parent_ino) {
            children.insert(name.to_string(), ino);
            attr.mtime = now;
            attr.ctime = now;
        }
        Ok(new_attr)
    }

    async fn link(&self, ino: u64, new_parent_ino: u64, new_name: &str) -> VfsResult<FileAttr> {
        validate_name(new_name)?;
        let mut guard = self.inner.lock();

        // Source must exist and must not be a directory.
        match guard.nodes.get(&ino) {
            Some(Node::Directory { .. }) => return Err(VfsError::IsADirectory),
            Some(_) => {}
            None => return Err(VfsError::NotFound),
        }

        // New parent must be a directory without a name collision.
        match guard.nodes.get(&new_parent_ino) {
            Some(Node::Directory { children, .. }) => {
                if children.contains_key(new_name) {
                    return Err(VfsError::AlreadyExists);
                }
            }
            Some(_) => return Err(VfsError::NotADirectory),
            None => return Err(VfsError::NotFound),
        }

        let now = Timestamp::now();
        if let Some(node) = guard.nodes.get_mut(&ino) {
            node.attr_mut().nlink += 1;
            node.attr_mut().ctime = now;
        }
        if let Some(Node::Directory { children, attr }) = guard.nodes.get_mut(&new_parent_ino) {
            children.insert(new_name.to_string(), ino);
            attr.mtime = now;
            attr.ctime = now;
        }

        Ok(guard.nodes.get(&ino).unwrap().attr().clone())
    }

    async fn rename(
        &self,
        old_parent_ino: u64,
        old_name: &str,
        new_parent_ino: u64,
        new_name: &str,
    ) -> VfsResult<()> {
        validate_name(old_name)?;
        validate_name(new_name)?;
        let mut guard = self.inner.lock();

        // Find source.
        let src_ino = match guard.nodes.get(&old_parent_ino) {
            Some(Node::Directory { children, .. }) => {
                *children.get(old_name).ok_or(VfsError::NotFound)?
            }
            Some(_) => return Err(VfsError::NotADirectory),
            None => return Err(VfsError::NotFound),
        };

        // Validate destination parent and check if destination exists.
        let dst_existing = match guard.nodes.get(&new_parent_ino) {
            Some(Node::Directory { children, .. }) => children.get(new_name).copied(),
            Some(_) => return Err(VfsError::NotADirectory),
            None => return Err(VfsError::NotFound),
        };

        // Handle destination already existing.
        if let Some(dst_ino) = dst_existing {
            if dst_ino == src_ino {
                return Ok(()); // rename-to-same — no-op
            }
            let src_is_dir = guard.nodes.get(&src_ino).is_some_and(Node::is_directory);
            let dst_is_dir = guard.nodes.get(&dst_ino).is_some_and(Node::is_directory);
            match (src_is_dir, dst_is_dir) {
                (true, false) => return Err(VfsError::NotADirectory),
                (false, true) => return Err(VfsError::IsADirectory),
                (true, true) => {
                    if let Some(Node::Directory { children, .. }) = guard.nodes.get(&dst_ino) {
                        if !children.is_empty() {
                            return Err(VfsError::NotEmpty);
                        }
                    }
                }
                (false, false) => {}
            }
            guard.nodes.remove(&dst_ino);
        }

        let now = Timestamp::now();

        // Remove from old parent.
        if let Some(Node::Directory { children, attr }) = guard.nodes.get_mut(&old_parent_ino) {
            children.remove(old_name);
            attr.mtime = now;
            attr.ctime = now;
        }

        // Insert into new parent.
        if let Some(Node::Directory { children, attr }) = guard.nodes.get_mut(&new_parent_ino) {
            children.insert(new_name.to_string(), src_ino);
            attr.mtime = now;
            attr.ctime = now;
        }

        // Touch ctime on the moved entry.
        if let Some(node) = guard.nodes.get_mut(&src_ino) {
            node.attr_mut().ctime = now;
        }

        Ok(())
    }

    async fn statfs(&self) -> VfsResult<FilesystemStats> {
        let guard = self.inner.lock();
        let inodes = guard.nodes.len() as u64;
        let bytes_used: u64 = guard
            .nodes
            .values()
            .map(|n| match n {
                Node::File { data, .. } => data.len() as u64,
                Node::Symlink { target, .. } => target.len() as u64,
                Node::Directory { .. } => 0,
            })
            .sum();
        Ok(FilesystemStats { inodes, bytes_used })
    }
}

/// Handle returned by [`MemFs::open`] and [`MemFs::create_file`].
#[derive(Debug)]
pub struct MemFile {
    inner: Arc<Mutex<Inner>>,
    ino: u64,
    #[allow(dead_code)]
    flags: i32, // stored for future permission checks, not enforced yet
}

#[async_trait]
impl File for MemFile {
    async fn read(&self, offset: u64, size: usize) -> VfsResult<Vec<u8>> {
        let guard = self.inner.lock();
        let node = guard.nodes.get(&self.ino).ok_or(VfsError::NotFound)?;
        match node {
            Node::File { data, .. } => {
                let start = (offset as usize).min(data.len());
                let end = start.saturating_add(size).min(data.len());
                Ok(data[start..end].to_vec())
            }
            Node::Directory { .. } => Err(VfsError::IsADirectory),
            Node::Symlink { .. } => Err(VfsError::NotSupported),
        }
    }

    async fn write(&self, offset: u64, data: &[u8]) -> VfsResult<u32> {
        let mut guard = self.inner.lock();
        let node = guard.nodes.get_mut(&self.ino).ok_or(VfsError::NotFound)?;
        let (file_data, attr) = match node {
            Node::File { data: d, attr: a } => (d, a),
            Node::Directory { .. } => return Err(VfsError::IsADirectory),
            Node::Symlink { .. } => return Err(VfsError::NotSupported),
        };
        let start = offset as usize;
        let end = start.saturating_add(data.len());
        if end > file_data.len() {
            file_data.resize(end, 0);
        }
        file_data[start..end].copy_from_slice(data);
        attr.size = file_data.len() as u64;
        attr.blocks = blocks_for(attr.size);
        attr.mtime = Timestamp::now();
        attr.ctime = Timestamp::now();
        Ok(data.len() as u32)
    }

    async fn truncate(&self, size: u64) -> VfsResult<()> {
        let mut guard = self.inner.lock();
        let node = guard.nodes.get_mut(&self.ino).ok_or(VfsError::NotFound)?;
        match node {
            Node::File { data, attr } => {
                data.resize(size as usize, 0);
                attr.size = size;
                attr.blocks = blocks_for(size);
                attr.mtime = Timestamp::now();
                attr.ctime = Timestamp::now();
                Ok(())
            }
            Node::Directory { .. } => Err(VfsError::IsADirectory),
            Node::Symlink { .. } => Err(VfsError::NotSupported),
        }
    }

    async fn flush(&self) -> VfsResult<()> {
        Ok(())
    }

    async fn fsync(&self) -> VfsResult<()> {
        Ok(())
    }

    async fn getattr(&self) -> VfsResult<FileAttr> {
        let guard = self.inner.lock();
        let node = guard.nodes.get(&self.ino).ok_or(VfsError::NotFound)?;
        Ok(node.attr().clone())
    }
}

// ─── Conformance test suite ─────────────────────────────────────────────────
//
// These tests pin down the expected behaviour of every `FileSystem` and
// `File` trait method. They run against `MemFs` in M2, and the same module
// will be re-used against `SupermemoryFs` in M5 to prove behavioural parity.

#[cfg(test)]
mod tests {
    use super::*;

    const UID: u32 = 1000;
    const GID: u32 = 1000;
    const ROOT: u64 = ROOT_INO;

    fn fs() -> MemFs {
        MemFs::new()
    }

    // ─── Root and sanity ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_root_exists() {
        let fs = fs();
        let attr = fs.getattr(ROOT).await.unwrap().expect("root must exist");
        assert!(attr.is_directory());
        assert_eq!(attr.ino, ROOT);
    }

    #[tokio::test]
    async fn test_root_readdir_empty() {
        let fs = fs();
        let names = fs.readdir(ROOT).await.unwrap().unwrap();
        assert!(names.is_empty());
    }

    #[tokio::test]
    async fn test_getattr_nonexistent_returns_none() {
        let fs = fs();
        assert!(fs.getattr(999).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_lookup_in_empty_root_returns_none() {
        let fs = fs();
        assert!(fs.lookup(ROOT, "nope").await.unwrap().is_none());
    }

    // ─── Directory creation and removal ─────────────────────────────────

    #[tokio::test]
    async fn test_mkdir_creates_entry() {
        let fs = fs();
        let dir = fs.mkdir(ROOT, "foo", 0o755, UID, GID).await.unwrap();
        assert!(dir.is_directory());

        let names = fs.readdir(ROOT).await.unwrap().unwrap();
        assert_eq!(names, vec!["foo".to_string()]);
    }

    #[tokio::test]
    async fn test_mkdir_returns_correct_attr() {
        let fs = fs();
        let dir = fs.mkdir(ROOT, "foo", 0o755, UID, GID).await.unwrap();
        assert_eq!(dir.mode & S_IFMT, S_IFDIR);
        assert_eq!(dir.mode & 0o777, 0o755);
        assert_eq!(dir.uid, UID);
        assert_eq!(dir.gid, GID);
        assert_eq!(dir.nlink, 2);
    }

    #[tokio::test]
    async fn test_mkdir_same_name_twice_fails() {
        let fs = fs();
        fs.mkdir(ROOT, "foo", 0o755, UID, GID).await.unwrap();
        let err = fs.mkdir(ROOT, "foo", 0o755, UID, GID).await.unwrap_err();
        assert!(matches!(err, VfsError::AlreadyExists));
    }

    #[tokio::test]
    async fn test_rmdir_empty_works() {
        let fs = fs();
        fs.mkdir(ROOT, "tmp", 0o755, UID, GID).await.unwrap();
        fs.rmdir(ROOT, "tmp").await.unwrap();
        assert!(fs.lookup(ROOT, "tmp").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_rmdir_nonempty_returns_not_empty() {
        let fs = fs();
        let dir = fs.mkdir(ROOT, "d", 0o755, UID, GID).await.unwrap();
        fs.create_file(dir.ino, "inside", 0o644, UID, GID)
            .await
            .unwrap();
        let err = fs.rmdir(ROOT, "d").await.unwrap_err();
        assert!(matches!(err, VfsError::NotEmpty));
    }

    #[tokio::test]
    async fn test_rmdir_nonexistent_returns_not_found() {
        let fs = fs();
        let err = fs.rmdir(ROOT, "nope").await.unwrap_err();
        assert!(matches!(err, VfsError::NotFound));
    }

    // ─── Regular files ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_create_file_returns_handle_and_attr() {
        let fs = fs();
        let (attr, handle) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        assert!(attr.is_file());
        assert_eq!(attr.mode & 0o777, 0o644);
        assert_eq!(attr.size, 0);
        // Handle should be able to read 0 bytes immediately.
        let empty = handle.read(0, 100).await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn test_write_then_read_round_trip() {
        let fs = fs();
        let (_, handle) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        let n = handle.write(0, b"hello world").await.unwrap();
        assert_eq!(n, 11);
        let data = handle.read(0, 100).await.unwrap();
        assert_eq!(data, b"hello world");
    }

    #[tokio::test]
    async fn test_write_at_offset_extends_file() {
        let fs = fs();
        let (_, handle) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        handle.write(10, b"hi").await.unwrap();
        let attr = handle.getattr().await.unwrap();
        assert_eq!(attr.size, 12);
        let data = handle.read(0, 100).await.unwrap();
        assert_eq!(&data[10..12], b"hi");
        // Gap should be zero-filled.
        assert_eq!(&data[0..10], &[0; 10]);
    }

    #[tokio::test]
    async fn test_read_past_eof_returns_empty() {
        let fs = fs();
        let (_, handle) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        handle.write(0, b"abc").await.unwrap();
        let data = handle.read(100, 10).await.unwrap();
        assert!(data.is_empty());
    }

    #[tokio::test]
    async fn test_read_empty_file_returns_empty() {
        let fs = fs();
        let (_, handle) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        let data = handle.read(0, 100).await.unwrap();
        assert!(data.is_empty());
    }

    #[tokio::test]
    async fn test_create_file_same_name_twice_fails() {
        let fs = fs();
        fs.create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        let err = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap_err();
        assert!(matches!(err, VfsError::AlreadyExists));
    }

    #[tokio::test]
    async fn test_unlink_removes_entry() {
        let fs = fs();
        fs.create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        fs.unlink(ROOT, "a.txt").await.unwrap();
        assert!(fs.lookup(ROOT, "a.txt").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_unlink_nonexistent_returns_not_found() {
        let fs = fs();
        let err = fs.unlink(ROOT, "nope").await.unwrap_err();
        assert!(matches!(err, VfsError::NotFound));
    }

    #[tokio::test]
    async fn test_unlink_directory_returns_is_a_directory() {
        let fs = fs();
        fs.mkdir(ROOT, "d", 0o755, UID, GID).await.unwrap();
        let err = fs.unlink(ROOT, "d").await.unwrap_err();
        assert!(matches!(err, VfsError::IsADirectory));
    }

    // ─── Readdir variants ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_readdir_lists_all_children_sorted() {
        let fs = fs();
        fs.create_file(ROOT, "b.txt", 0o644, UID, GID)
            .await
            .unwrap();
        fs.create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        fs.mkdir(ROOT, "c", 0o755, UID, GID).await.unwrap();

        let names = fs.readdir(ROOT).await.unwrap().unwrap();
        assert_eq!(names, vec!["a.txt", "b.txt", "c"]);
    }

    #[tokio::test]
    async fn test_readdir_on_file_returns_none() {
        let fs = fs();
        let (attr, _) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        assert!(fs.readdir(attr.ino).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_readdir_plus_includes_attrs() {
        let fs = fs();
        let (file_attr, _) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        let entries = fs.readdir_plus(ROOT).await.unwrap().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(entries[0].attr.ino, file_attr.ino);
    }

    // ─── Rename ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_rename_within_same_directory() {
        let fs = fs();
        let (_, handle) = fs
            .create_file(ROOT, "old.txt", 0o644, UID, GID)
            .await
            .unwrap();
        handle.write(0, b"hi").await.unwrap();

        fs.rename(ROOT, "old.txt", ROOT, "new.txt").await.unwrap();
        assert!(fs.lookup(ROOT, "old.txt").await.unwrap().is_none());
        let moved = fs.lookup(ROOT, "new.txt").await.unwrap().unwrap();
        assert_eq!(moved.size, 2);
    }

    #[tokio::test]
    async fn test_rename_across_directories() {
        let fs = fs();
        let src_dir = fs.mkdir(ROOT, "src", 0o755, UID, GID).await.unwrap();
        let dst_dir = fs.mkdir(ROOT, "dst", 0o755, UID, GID).await.unwrap();
        fs.create_file(src_dir.ino, "f", 0o644, UID, GID)
            .await
            .unwrap();

        fs.rename(src_dir.ino, "f", dst_dir.ino, "f").await.unwrap();
        assert!(fs.lookup(src_dir.ino, "f").await.unwrap().is_none());
        assert!(fs.lookup(dst_dir.ino, "f").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_rename_nonexistent_returns_not_found() {
        let fs = fs();
        let err = fs.rename(ROOT, "nope", ROOT, "whatever").await.unwrap_err();
        assert!(matches!(err, VfsError::NotFound));
    }

    #[tokio::test]
    async fn test_rename_over_existing_file_replaces() {
        let fs = fs();
        let (_, src_handle) = fs.create_file(ROOT, "src", 0o644, UID, GID).await.unwrap();
        src_handle.write(0, b"new").await.unwrap();
        fs.create_file(ROOT, "dst", 0o644, UID, GID).await.unwrap();

        fs.rename(ROOT, "src", ROOT, "dst").await.unwrap();
        assert!(fs.lookup(ROOT, "src").await.unwrap().is_none());
        let dst = fs.lookup(ROOT, "dst").await.unwrap().unwrap();
        assert_eq!(dst.size, 3);
    }

    // ─── Setattr ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_setattr_truncate_via_size() {
        let fs = fs();
        let (attr, handle) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        handle.write(0, b"hello world").await.unwrap();
        let updated = fs
            .setattr(
                attr.ino,
                SetAttr {
                    size: Some(5),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.size, 5);
        let data = handle.read(0, 100).await.unwrap();
        assert_eq!(data, b"hello");
    }

    #[tokio::test]
    async fn test_setattr_chmod_via_mode() {
        let fs = fs();
        let (attr, _) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        let updated = fs
            .setattr(
                attr.ino,
                SetAttr {
                    mode: Some(0o600),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.mode & 0o777, 0o600);
        // File type bits must be preserved.
        assert_eq!(updated.mode & S_IFMT, S_IFREG);
    }

    #[tokio::test]
    async fn test_setattr_chown_via_uid_gid() {
        let fs = fs();
        let (attr, _) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        let updated = fs
            .setattr(
                attr.ino,
                SetAttr {
                    uid: Some(42),
                    gid: Some(99),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.uid, 42);
        assert_eq!(updated.gid, 99);
    }

    #[tokio::test]
    async fn test_setattr_utimens_via_mtime() {
        let fs = fs();
        let (attr, _) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        let target = Timestamp {
            sec: 1_700_000_000,
            nsec: 500,
        };
        let updated = fs
            .setattr(
                attr.ino,
                SetAttr {
                    mtime: Some(TimeOrNow::Time(target)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.mtime, target);
    }

    // ─── Symlinks ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_symlink_create_and_readlink() {
        let fs = fs();
        let attr = fs
            .symlink(ROOT, "link", "/some/target", UID, GID)
            .await
            .unwrap();
        assert!(attr.is_symlink());
        assert_eq!(attr.size, "/some/target".len() as u64);
        let target = fs.readlink(attr.ino).await.unwrap().unwrap();
        assert_eq!(target, "/some/target");
    }

    #[tokio::test]
    async fn test_readlink_on_regular_file_returns_error() {
        let fs = fs();
        let (attr, _) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        let err = fs.readlink(attr.ino).await.unwrap_err();
        assert!(matches!(err, VfsError::NotASymlink));
    }

    // ─── Hard links ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_link_creates_second_name() {
        let fs = fs();
        let (attr, handle) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        handle.write(0, b"data").await.unwrap();

        let linked = fs.link(attr.ino, ROOT, "b.txt").await.unwrap();
        assert_eq!(linked.nlink, 2);

        // Both names should map to the same inode.
        let via_a = fs.lookup(ROOT, "a.txt").await.unwrap().unwrap();
        let via_b = fs.lookup(ROOT, "b.txt").await.unwrap().unwrap();
        assert_eq!(via_a.ino, via_b.ino);
    }

    #[tokio::test]
    async fn test_unlink_one_name_keeps_other() {
        let fs = fs();
        let (attr, handle) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        handle.write(0, b"shared").await.unwrap();
        fs.link(attr.ino, ROOT, "b.txt").await.unwrap();

        fs.unlink(ROOT, "a.txt").await.unwrap();
        assert!(fs.lookup(ROOT, "a.txt").await.unwrap().is_none());

        // Other name and data should still be present.
        let remaining = fs.lookup(ROOT, "b.txt").await.unwrap().unwrap();
        assert_eq!(remaining.size, 6);
        assert_eq!(remaining.nlink, 1);
    }

    // ─── statfs ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_statfs_counts_inodes_and_bytes() {
        let fs = fs();
        let (_, handle) = fs
            .create_file(ROOT, "a.txt", 0o644, UID, GID)
            .await
            .unwrap();
        handle.write(0, b"12345").await.unwrap();

        let stats = fs.statfs().await.unwrap();
        assert!(stats.inodes >= 2); // root + file
        assert_eq!(stats.bytes_used, 5);
    }
}
