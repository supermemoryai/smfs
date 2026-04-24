//! POSIX file-mode constants and defaults.
//!
//! These are standard Unix values, not project-specific. The upper bits of a
//! `mode` field identify the file type; the lower 12 bits are permission bits.
//! supermemoryfs only uses three file types — regular files, directories, and
//! symbolic links — so the FIFO/device/socket masks are deliberately omitted.

/// Mask extracting the file type bits from a `mode` field.
pub const S_IFMT: u32 = 0o170000;

/// File type: regular file.
pub const S_IFREG: u32 = 0o100000;

/// File type: directory.
pub const S_IFDIR: u32 = 0o040000;

/// File type: symbolic link.
pub const S_IFLNK: u32 = 0o120000;

/// Default mode for a newly created regular file: `S_IFREG | 0o644`.
pub const DEFAULT_FILE_MODE: u32 = S_IFREG | 0o644;

/// Default mode for a newly created directory: `S_IFDIR | 0o755`.
pub const DEFAULT_DIR_MODE: u32 = S_IFDIR | 0o755;

/// Default mode for a newly created symbolic link: `S_IFLNK | 0o777`.
pub const DEFAULT_SYMLINK_MODE: u32 = S_IFLNK | 0o777;

/// Maximum length in bytes for a single path component (POSIX `NAME_MAX`).
pub const MAX_NAME_LEN: usize = 255;

/// Preferred I/O block size reported in `FileAttr::blksize`. Mirrors typical
/// Linux/Unix filesystems.
pub const PREFERRED_BLOCK_SIZE: u32 = 4096;
