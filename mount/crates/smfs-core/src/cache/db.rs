//! SQLite database wrapper for the local filesystem cache.

use std::path::Path;

use parking_lot::Mutex;
use rusqlite::Connection;

use crate::vfs::{FileAttr, Timestamp, DEFAULT_DIR_MODE, PREFERRED_BLOCK_SIZE};

/// Default chunk size for file data storage (bytes).
pub const DEFAULT_CHUNK_SIZE: usize = 4096;

/// Root inode number. Always 1, matching POSIX convention.
pub const ROOT_INO: u64 = 1;

/// Maximum dentry cache entries.
pub const DENTRY_CACHE_MAX: usize = 10_000;

/// SQLite-backed persistent store for filesystem metadata and content.
///
/// Wraps a single `rusqlite::Connection` behind a `Mutex` for safe
/// concurrent access from async trait methods. SQLite doesn't benefit
/// from connection pooling, so a single serialized connection is the
/// correct approach.
pub struct Db {
    pub(crate) conn: Mutex<Connection>,
    pub(crate) chunk_size: usize,
}

impl Db {
    /// Open (or create) a database at the given path.
    ///
    /// Sets WAL journal mode, configures pragmas for performance, creates
    /// tables if they don't exist, and ensures the root inode is present.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        Self::configure_and_init(conn)
    }

    /// Open an in-memory database (for tests).
    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::configure_and_init(conn)
    }

    fn configure_and_init(conn: Connection) -> anyhow::Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = OFF;",
        )?;

        // Create tables.
        conn.execute_batch(include_str!("schema.sql"))?;

        let db = Self {
            conn: Mutex::new(conn),
            chunk_size: DEFAULT_CHUNK_SIZE,
        };

        db.ensure_root()?;
        db.ensure_config()?;

        Ok(db)
    }

    /// Ensure the root directory inode (ino=1) exists.
    fn ensure_root(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM fs_inode WHERE ino = ?1",
            [ROOT_INO as i64],
            |row| row.get(0),
        )?;

        if !exists {
            let now = Timestamp::now();
            conn.execute(
                "INSERT INTO fs_inode (ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec)
                 VALUES (?1, ?2, 2, 0, 0, 0, ?3, ?4, ?5, 0, ?6, ?7, ?8)",
                rusqlite::params![
                    ROOT_INO as i64,
                    DEFAULT_DIR_MODE as i64,
                    now.sec,
                    now.sec,
                    now.sec,
                    now.nsec,
                    now.nsec,
                    now.nsec,
                ],
            )?;
        }
        Ok(())
    }

    /// Ensure default config values exist.
    fn ensure_config(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO fs_config (key, value) VALUES ('chunk_size', ?1)",
            [self.chunk_size.to_string()],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO fs_config (key, value) VALUES ('schema_version', '1')",
            [],
        )?;
        Ok(())
    }

    /// Look up the remote document ID for an inode, if one has been stored.
    pub(crate) fn get_remote_id(&self, ino: u64) -> Option<String> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT remote_id FROM fs_remote WHERE ino = ?1",
            [ino as i64],
            |row| row.get(0),
        )
        .ok()
    }

    /// Store or update the remote document ID for an inode.
    pub(crate) fn set_remote_id(&self, ino: u64, remote_id: &str) {
        let conn = self.conn.lock();
        let _ = conn.execute(
            "INSERT OR REPLACE INTO fs_remote (ino, remote_id) VALUES (?1, ?2)",
            rusqlite::params![ino as i64, remote_id],
        );
    }

    /// Remove the remote document ID mapping for an inode.
    pub(crate) fn delete_remote_id(&self, ino: u64) {
        let conn = self.conn.lock();
        let _ = conn.execute("DELETE FROM fs_remote WHERE ino = ?1", [ino as i64]);
    }

    /// Read a `FileAttr` from an fs_inode row.
    #[allow(dead_code)] // used by SupermemoryFs in M5b
    pub(crate) fn row_to_attr(row: &rusqlite::Row) -> rusqlite::Result<FileAttr> {
        let ino: i64 = row.get("ino")?;
        let mode: i64 = row.get("mode")?;
        let nlink: i64 = row.get("nlink")?;
        let uid: i64 = row.get("uid")?;
        let gid: i64 = row.get("gid")?;
        let size: i64 = row.get("size")?;
        let atime_sec: i64 = row.get("atime")?;
        let mtime_sec: i64 = row.get("mtime")?;
        let ctime_sec: i64 = row.get("ctime")?;
        let rdev: i64 = row.get("rdev")?;
        let atime_nsec: i64 = row.get("atime_nsec")?;
        let mtime_nsec: i64 = row.get("mtime_nsec")?;
        let ctime_nsec: i64 = row.get("ctime_nsec")?;

        Ok(FileAttr {
            ino: ino as u64,
            mode: mode as u32,
            nlink: nlink as u32,
            uid: uid as u32,
            gid: gid as u32,
            size: size as u64,
            blocks: (size as u64).div_ceil(512),
            atime: Timestamp {
                sec: atime_sec,
                nsec: atime_nsec as u32,
            },
            mtime: Timestamp {
                sec: mtime_sec,
                nsec: mtime_nsec as u32,
            },
            ctime: Timestamp {
                sec: ctime_sec,
                nsec: ctime_nsec as u32,
            },
            rdev: rdev as u32,
            blksize: PREFERRED_BLOCK_SIZE,
        })
    }
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("chunk_size", &self.chunk_size)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_creates_root() {
        let db = Db::open_in_memory().unwrap();
        let conn = db.conn.lock();
        let ino: i64 = conn
            .query_row("SELECT ino FROM fs_inode WHERE ino = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ino, 1);
    }

    #[test]
    fn open_in_memory_creates_config() {
        let db = Db::open_in_memory().unwrap();
        let conn = db.conn.lock();
        let chunk: String = conn
            .query_row(
                "SELECT value FROM fs_config WHERE key = 'chunk_size'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(chunk, "4096");
    }

    #[test]
    fn get_remote_id_returns_none_for_missing() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.get_remote_id(42), None);
    }

    #[test]
    fn set_then_get_remote_id_round_trips() {
        let db = Db::open_in_memory().unwrap();
        db.set_remote_id(42, "doc-abc-123");
        assert_eq!(db.get_remote_id(42), Some("doc-abc-123".to_string()));
    }

    #[test]
    fn delete_remote_id_clears_mapping() {
        let db = Db::open_in_memory().unwrap();
        db.set_remote_id(42, "doc-abc-123");
        db.delete_remote_id(42);
        assert_eq!(db.get_remote_id(42), None);
    }

    #[test]
    fn set_remote_id_overwrites_existing() {
        let db = Db::open_in_memory().unwrap();
        db.set_remote_id(42, "old-id");
        db.set_remote_id(42, "new-id");
        assert_eq!(db.get_remote_id(42), Some("new-id".to_string()));
    }

    #[test]
    fn root_inode_is_directory_with_nlink_2() {
        let db = Db::open_in_memory().unwrap();
        let conn = db.conn.lock();
        let (mode, nlink): (i64, i64) = conn
            .query_row("SELECT mode, nlink FROM fs_inode WHERE ino = 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(mode as u32, DEFAULT_DIR_MODE);
        assert_eq!(nlink, 2);
    }
}
