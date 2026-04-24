//! Error types for the [`FileSystem`](super::FileSystem) trait.
//!
//! [`VfsError`] enumerates every failure the trait can surface, and provides
//! [`VfsError::to_errno`] for mount adapters that need to translate back into
//! POSIX `errno` values for the kernel.

use thiserror::Error;

/// Errors produced by [`FileSystem`](super::FileSystem) operations.
#[derive(Error, Debug)]
pub enum VfsError {
    /// Requested inode or name does not exist.
    #[error("not found")]
    NotFound,

    /// A name that must be unique already exists in the target directory.
    #[error("already exists")]
    AlreadyExists,

    /// Operation expected a directory and received a non-directory.
    #[error("not a directory")]
    NotADirectory,

    /// Operation expected a non-directory and received a directory.
    #[error("is a directory")]
    IsADirectory,

    /// Operation expected a symbolic link and received something else.
    #[error("not a symbolic link")]
    NotASymlink,

    /// `rmdir` or `rename` over a non-empty directory.
    #[error("directory not empty")]
    NotEmpty,

    /// Path is malformed, has an embedded NUL byte, or escapes the root via `..`.
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// A single path component exceeds [`MAX_NAME_LEN`](super::mode::MAX_NAME_LEN).
    #[error("name too long: {0} bytes")]
    NameTooLong(usize),

    /// Operation targets the root inode in a way that is not permitted
    /// (e.g. removing or renaming `/`).
    #[error("cannot operate on root directory")]
    RootOperation,

    /// `rename` would make a directory a descendant of itself.
    #[error("rename would create a loop")]
    InvalidRename,

    /// Symbolic link chain exceeds the resolution limit.
    #[error("too many symbolic link levels")]
    SymlinkLoop,

    /// Caller does not have permission for this operation.
    #[error("permission denied")]
    PermissionDenied,

    /// Operation is defined in the trait but not supported by this backend
    /// (e.g. `mkfifo` against a backend that has no FIFO concept).
    #[error("operation not supported")]
    NotSupported,

    /// Wraps a lower-level I/O error.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

impl VfsError {
    /// Map this error to a POSIX `errno` value suitable for kernel replies.
    ///
    /// Used by the FUSE and NFS mount adapters in `crate::mount` to translate
    /// [`VfsResult`] failures into the integer codes the kernel expects.
    pub fn to_errno(&self) -> i32 {
        match self {
            Self::NotFound => libc::ENOENT,
            Self::AlreadyExists => libc::EEXIST,
            Self::NotADirectory => libc::ENOTDIR,
            Self::IsADirectory => libc::EISDIR,
            Self::NotASymlink => libc::EINVAL,
            Self::NotEmpty => libc::ENOTEMPTY,
            Self::InvalidPath(_) => libc::EINVAL,
            Self::NameTooLong(_) => libc::ENAMETOOLONG,
            Self::RootOperation => libc::EPERM,
            Self::PermissionDenied => libc::EACCES,
            Self::InvalidRename => libc::EINVAL,
            Self::SymlinkLoop => libc::ELOOP,
            Self::NotSupported => libc::ENOTSUP,
            Self::Io(_) => libc::EIO,
        }
    }
}

/// Result alias used throughout the [`vfs`](super) module.
pub type VfsResult<T> = Result<T, VfsError>;
