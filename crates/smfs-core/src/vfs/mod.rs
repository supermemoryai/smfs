//! Virtual filesystem trait and in-memory reference implementation.
//!
//! This is the single source of truth for what a filesystem operation looks
//! like in supermemoryfs. The [`FileSystem`] trait is implemented by every
//! backend ([`MemFs`] in this crate, `SupermemoryFs` in M5) and called by
//! every frontend (FUSE and NFS mount adapters in M3).
//!
//! ## Module layout
//!
//! - [`traits`] — the [`FileSystem`] and [`File`] traits
//! - [`types`] — [`FileAttr`], [`DirEntry`], [`FilesystemStats`], [`SetAttr`],
//!   [`TimeOrNow`], [`Timestamp`], [`FileType`]
//! - [`mode`] — POSIX mode constants ([`S_IFMT`], [`S_IFREG`], etc.)
//! - [`error`] — [`VfsError`] with `to_errno()` and [`VfsResult<T>`]
//! - [`path`] — path normalization helpers
//! - [`mem`] — [`MemFs`], the in-memory reference implementation, plus its
//!   conformance test suite
//!
//! Backends depend on `vfs`, never the other way around. Keeping this module
//! free of kernel, SQLite, and network concerns is deliberate — it means the
//! trait is the single definition point for filesystem semantics.

pub mod error;
pub mod mem;
pub mod mode;
pub mod path;
pub mod traits;
pub mod types;

pub use error::{VfsError, VfsResult};
pub use mem::MemFs;
pub use mode::{
    DEFAULT_DIR_MODE, DEFAULT_FILE_MODE, DEFAULT_SYMLINK_MODE, MAX_NAME_LEN, PREFERRED_BLOCK_SIZE,
    S_IFDIR, S_IFLNK, S_IFMT, S_IFREG,
};
pub use traits::{BoxedFile, File, FileSystem};
pub use types::{DirEntry, FileAttr, FileType, FilesystemStats, SetAttr, TimeOrNow, Timestamp};
