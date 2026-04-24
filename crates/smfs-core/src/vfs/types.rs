//! Data types used by the [`FileSystem`](super::FileSystem) trait.
//!
//! All types here are pure data: no I/O, no locks, no async. They flow into
//! trait methods as parameters and back out as return values.

use std::time::{SystemTime, UNIX_EPOCH};

use super::mode::{
    DEFAULT_DIR_MODE, DEFAULT_FILE_MODE, DEFAULT_SYMLINK_MODE, PREFERRED_BLOCK_SIZE, S_IFMT,
};

/// A Unix-style timestamp with nanosecond precision.
///
/// Uses the `(seconds, nanoseconds)` representation that matches both the FUSE
/// kernel protocol and the eventual SQLite cache schema, avoiding conversions
/// in hot paths.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Timestamp {
    /// Seconds since the Unix epoch (may be negative for pre-1970 dates).
    pub sec: i64,
    /// Nanoseconds within the current second, `0..=999_999_999`.
    pub nsec: u32,
}

impl Timestamp {
    /// The Unix epoch: `1970-01-01 00:00:00 UTC`.
    pub const ZERO: Self = Self { sec: 0, nsec: 0 };

    /// Capture the current wall-clock time.
    pub fn now() -> Self {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => Self {
                sec: d.as_secs() as i64,
                nsec: d.subsec_nanos(),
            },
            Err(_) => Self::ZERO,
        }
    }

    /// Construct from whole seconds (nanoseconds set to zero).
    pub const fn from_secs(sec: i64) -> Self {
        Self { sec, nsec: 0 }
    }
}

/// A classification of an inode's file type.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FileType {
    /// A regular file containing bytes.
    Regular,
    /// A directory containing named entries.
    Directory,
    /// A symbolic link referring to another path.
    Symlink,
}

/// Complete metadata for an inode — the supermemoryfs equivalent of POSIX `struct stat`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileAttr {
    /// Inode number. Root is always `1`.
    pub ino: u64,
    /// File type (upper bits) combined with permission bits (lower 12 bits).
    pub mode: u32,
    /// Number of hard links referencing this inode.
    pub nlink: u32,
    /// Owner user ID.
    pub uid: u32,
    /// Owner group ID.
    pub gid: u32,
    /// Logical size in bytes (file contents, or length of symlink target).
    pub size: u64,
    /// Number of 512-byte blocks allocated (derived from `size`).
    pub blocks: u64,
    /// Last access time.
    pub atime: Timestamp,
    /// Last modification time (content changes).
    pub mtime: Timestamp,
    /// Last status change time (metadata changes).
    pub ctime: Timestamp,
    /// Device ID for special files — always `0` in supermemoryfs.
    pub rdev: u32,
    /// Preferred I/O block size.
    pub blksize: u32,
}

impl FileAttr {
    /// Construct attributes for a newly created regular file.
    pub fn new_file(ino: u64, uid: u32, gid: u32) -> Self {
        let now = Timestamp::now();
        Self {
            ino,
            mode: DEFAULT_FILE_MODE,
            nlink: 1,
            uid,
            gid,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            rdev: 0,
            blksize: PREFERRED_BLOCK_SIZE,
        }
    }

    /// Construct attributes for a newly created directory.
    ///
    /// A fresh directory has `nlink = 2` (one for the entry in its parent,
    /// one for its own `.`).
    pub fn new_dir(ino: u64, uid: u32, gid: u32) -> Self {
        let now = Timestamp::now();
        Self {
            ino,
            mode: DEFAULT_DIR_MODE,
            nlink: 2,
            uid,
            gid,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            rdev: 0,
            blksize: PREFERRED_BLOCK_SIZE,
        }
    }

    /// Construct attributes for a newly created symbolic link.
    ///
    /// `target_len` is the byte length of the symlink target, stored in `size`
    /// per POSIX convention.
    pub fn new_symlink(ino: u64, target_len: u64, uid: u32, gid: u32) -> Self {
        let now = Timestamp::now();
        Self {
            ino,
            mode: DEFAULT_SYMLINK_MODE,
            nlink: 1,
            uid,
            gid,
            size: target_len,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            rdev: 0,
            blksize: PREFERRED_BLOCK_SIZE,
        }
    }

    /// Construct a file attr with explicit mode and timestamp (used by SQLite backend).
    pub fn new_file_with(ino: u64, mode: u32, uid: u32, gid: u32, ts: Timestamp) -> Self {
        Self {
            ino,
            mode,
            nlink: 1,
            uid,
            gid,
            size: 0,
            blocks: 0,
            atime: ts,
            mtime: ts,
            ctime: ts,
            rdev: 0,
            blksize: PREFERRED_BLOCK_SIZE,
        }
    }

    /// Construct a directory attr with explicit mode and timestamp (used by SQLite backend).
    pub fn new_dir_with(ino: u64, mode: u32, uid: u32, gid: u32, ts: Timestamp) -> Self {
        Self {
            ino,
            mode,
            nlink: 2,
            uid,
            gid,
            size: 0,
            blocks: 0,
            atime: ts,
            mtime: ts,
            ctime: ts,
            rdev: 0,
            blksize: PREFERRED_BLOCK_SIZE,
        }
    }

    /// Derive the file type from the mode bits.
    pub fn file_type(&self) -> FileType {
        match self.mode & S_IFMT {
            m if m == super::mode::S_IFREG => FileType::Regular,
            m if m == super::mode::S_IFDIR => FileType::Directory,
            m if m == super::mode::S_IFLNK => FileType::Symlink,
            _ => FileType::Regular,
        }
    }

    /// Returns `true` if this inode is a regular file.
    pub fn is_file(&self) -> bool {
        matches!(self.file_type(), FileType::Regular)
    }

    /// Returns `true` if this inode is a directory.
    pub fn is_directory(&self) -> bool {
        matches!(self.file_type(), FileType::Directory)
    }

    /// Returns `true` if this inode is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        matches!(self.file_type(), FileType::Symlink)
    }
}

/// A directory entry with its full attributes.
///
/// Returned by [`FileSystem::readdir_plus`](super::FileSystem::readdir_plus) as
/// an optimization over calling `readdir` + `getattr` per entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// Entry name (single path component, not a full path).
    pub name: String,
    /// Attributes of the entry's inode.
    pub attr: FileAttr,
}

/// Filesystem-wide statistics returned by [`FileSystem::statfs`](super::FileSystem::statfs).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FilesystemStats {
    /// Total number of inodes currently allocated.
    pub inodes: u64,
    /// Total bytes occupied by file and symlink contents.
    pub bytes_used: u64,
}

/// Request for a timestamp update, as used in [`SetAttr`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TimeOrNow {
    /// Set to the current wall-clock time at the moment of the call.
    Now,
    /// Set to the specified timestamp.
    Time(Timestamp),
}

/// Partial attribute update passed to [`FileSystem::setattr`](super::FileSystem::setattr).
///
/// Every field is optional — only fields set to `Some` are modified on the
/// inode. This mirrors FUSE's single `setattr` callback with a field mask.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct SetAttr {
    /// New permission bits (the file-type bits in `S_IFMT` are preserved).
    pub mode: Option<u32>,
    /// New owner user ID.
    pub uid: Option<u32>,
    /// New owner group ID.
    pub gid: Option<u32>,
    /// New size — truncates or zero-extends the file.
    pub size: Option<u64>,
    /// New access time.
    pub atime: Option<TimeOrNow>,
    /// New modification time.
    pub mtime: Option<TimeOrNow>,
}
