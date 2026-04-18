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

        // Create tables. (Safe on fresh DB; no-op on an existing one.)
        conn.execute_batch(include_str!("schema.sql"))?;

        // Apply additive migrations for databases that existed before a column
        // was added to CREATE TABLE. SQLite has no `ADD COLUMN IF NOT EXISTS`,
        // so we attempt each ALTER and ignore duplicate-column errors.
        let migrations = [
            "ALTER TABLE fs_inode  ADD COLUMN dirty_since         INTEGER",
            "ALTER TABLE fs_remote ADD COLUMN mirrored_updated_at INTEGER",
            "ALTER TABLE fs_remote ADD COLUMN last_status         TEXT",
            "ALTER TABLE fs_remote ADD COLUMN last_status_at      INTEGER",
        ];
        for sql in migrations {
            if let Err(e) = conn.execute(sql, []) {
                let msg = e.to_string();
                if !msg.contains("duplicate column") {
                    return Err(e.into());
                }
            }
        }

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

    /// Mark an inode as locally dirty (user wrote to it) at the given epoch-ms.
    /// The pull reconciler will not clobber this inode if its `dirty_since` is
    /// newer than the remote `updatedAt`.
    pub(crate) fn set_dirty_since(&self, ino: u64, epoch_ms: Option<i64>) {
        let conn = self.conn.lock();
        let _ = conn.execute(
            "UPDATE fs_inode SET dirty_since = ?2 WHERE ino = ?1",
            rusqlite::params![ino as i64, epoch_ms],
        );
    }

    /// Get the dirty watermark for an inode, if any.
    #[allow(dead_code)]
    pub(crate) fn get_dirty_since(&self, ino: u64) -> Option<i64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT dirty_since FROM fs_inode WHERE ino = ?1",
            [ino as i64],
            |row| row.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
    }

    /// Update the remote sync state for an inode's fs_remote row. No-op if
    /// the row doesn't exist — call `set_remote_id` first.
    pub(crate) fn set_mirrored_state(
        &self,
        ino: u64,
        mirrored_updated_at: Option<i64>,
        last_status: Option<&str>,
        last_status_at: Option<i64>,
    ) {
        let conn = self.conn.lock();
        let _ = conn.execute(
            "UPDATE fs_remote
                SET mirrored_updated_at = COALESCE(?2, mirrored_updated_at),
                    last_status         = COALESCE(?3, last_status),
                    last_status_at      = COALESCE(?4, last_status_at)
              WHERE ino = ?1",
            rusqlite::params![
                ino as i64,
                mirrored_updated_at,
                last_status,
                last_status_at,
            ],
        );
    }

    /// Read the mirrored remote state for an inode.
    #[allow(dead_code)]
    pub(crate) fn get_mirrored_state(
        &self,
        ino: u64,
    ) -> Option<(Option<i64>, Option<String>, Option<i64>)> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT mirrored_updated_at, last_status, last_status_at
               FROM fs_remote WHERE ino = ?1",
            [ino as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok()
    }

    /// Find the inode mapped to a remote document ID, if any.
    pub(crate) fn ino_by_remote_id(&self, remote_id: &str) -> Option<u64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT ino FROM fs_remote WHERE remote_id = ?1",
            [remote_id],
            |row| row.get::<_, i64>(0),
        )
        .ok()
        .map(|n| n as u64)
    }

    /// Read a sync_meta value by key.
    pub(crate) fn sync_meta_get(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT value FROM sync_meta WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .ok()
    }

    /// Write (or overwrite) a sync_meta value.
    pub(crate) fn sync_meta_set(&self, key: &str, value: &str) {
        let conn = self.conn.lock();
        let _ = conn.execute(
            "INSERT OR REPLACE INTO sync_meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        );
    }

    /// Enqueue a push-queue op for the given filepath with latest-wins
    /// coalescing semantics:
    ///
    /// - No row exists → INSERT fresh.
    /// - Row exists, not yet inflight → REPLACE op/content/rename_to
    ///   (intermediate write is dropped on the floor).
    /// - Row exists AND inflight → write to the `pending_*` slot (if the
    ///   pending slot is also filled, the newest write wins there too).
    pub(crate) fn push_queue_upsert(
        &self,
        filepath: &str,
        op: PushOp,
        content_ino: Option<u64>,
        rename_to: Option<&str>,
        now_ms: i64,
    ) {
        let conn = self.conn.lock();
        let op_str = op.as_str();

        let inflight_started: Option<i64> = conn
            .query_row(
                "SELECT inflight_started_at FROM push_queue WHERE filepath = ?1",
                [filepath],
                |r| r.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten();

        let content_i64 = content_ino.map(|n| n as i64);

        if let Some(_started) = inflight_started {
            // Something is in flight; park the write in the pending slot.
            let _ = conn.execute(
                "UPDATE push_queue
                    SET pending_op           = ?2,
                        pending_content_ino  = ?3,
                        pending_rename_to    = ?4,
                        updated_at           = ?5
                  WHERE filepath = ?1",
                rusqlite::params![filepath, op_str, content_i64, rename_to, now_ms],
            );
        } else {
            // No inflight; coalesce into the primary slot.
            let _ = conn.execute(
                "INSERT INTO push_queue
                    (filepath, op, content_ino, rename_to, attempt, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5)
                 ON CONFLICT(filepath) DO UPDATE SET
                    op           = excluded.op,
                    content_ino  = excluded.content_ino,
                    rename_to    = excluded.rename_to,
                    attempt      = 0,
                    last_error   = NULL,
                    updated_at   = excluded.updated_at",
                rusqlite::params![filepath, op_str, content_i64, rename_to, now_ms],
            );
        }
    }

    /// Atomically claim the next queued job whose backoff has elapsed, marking
    /// it inflight by stamping `inflight_started_at`. Returns None if the
    /// queue is empty or everything is either inflight or backing off.
    pub(crate) fn push_queue_claim_next(&self, now_ms: i64) -> Option<PushJob> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT filepath, op, content_ino, rename_to, attempt
                   FROM push_queue
                  WHERE inflight_started_at IS NULL
                    AND updated_at <= ?1
                  ORDER BY updated_at ASC
                  LIMIT 1",
                [now_ms],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .ok()?;

        let (filepath, op_str, content_ino, rename_to, attempt) = row;
        let op = PushOp::parse(&op_str)?;

        let remote_id: Option<String> = conn
            .query_row(
                "SELECT remote_id FROM fs_remote
                   JOIN fs_dentry ON fs_dentry.ino = fs_remote.ino
                  WHERE fs_dentry.name = (
                    CASE WHEN instr(?1, '/') > 0
                         THEN substr(?1, length(?1) - instr(reverse(?1 || '/'), '/') + 2)
                         ELSE ?1
                    END)",
                [&filepath],
                |r| r.get::<_, String>(0),
            )
            .ok();

        let updated = conn.execute(
            "UPDATE push_queue
                SET inflight_started_at = ?2,
                    inflight_remote_id  = ?3
              WHERE filepath = ?1
                AND inflight_started_at IS NULL",
            rusqlite::params![filepath, now_ms, remote_id],
        );
        if matches!(updated, Ok(0) | Err(_)) {
            return None;
        }

        Some(PushJob {
            filepath,
            op,
            content_ino: content_ino.map(|n| n as u64),
            rename_to,
            attempt,
            inflight_remote_id: remote_id,
        })
    }

    /// Stamp the remote_id that came back from a create. Used so the inflight
    /// poller can GET /v3/documents/:id while this job is still processing.
    pub(crate) fn push_queue_set_remote_id(&self, filepath: &str, remote_id: &str) {
        let conn = self.conn.lock();
        let _ = conn.execute(
            "UPDATE push_queue SET inflight_remote_id = ?2 WHERE filepath = ?1",
            rusqlite::params![filepath, remote_id],
        );
    }

    /// Mark a successful push. If a pending op is queued, promote it into the
    /// primary slot; otherwise delete the row.
    pub(crate) fn push_queue_finalize_success(&self, filepath: &str, now_ms: i64) {
        let conn = self.conn.lock();
        let pending: Option<(String, Option<i64>, Option<String>)> = conn
            .query_row(
                "SELECT pending_op, pending_content_ino, pending_rename_to
                   FROM push_queue WHERE filepath = ?1",
                [filepath],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok()
            .and_then(|(op, c, r)| op.map(|o| (o, c, r)));

        if let Some((op, content_ino, rename_to)) = pending {
            let _ = conn.execute(
                "UPDATE push_queue
                    SET op                  = ?2,
                        content_ino         = ?3,
                        rename_to           = ?4,
                        pending_op          = NULL,
                        pending_content_ino = NULL,
                        pending_rename_to   = NULL,
                        inflight_started_at = NULL,
                        inflight_remote_id  = NULL,
                        attempt             = 0,
                        last_error          = NULL,
                        updated_at          = ?5
                  WHERE filepath = ?1",
                rusqlite::params![filepath, op, content_ino, rename_to, now_ms],
            );
        } else {
            let _ = conn.execute("DELETE FROM push_queue WHERE filepath = ?1", [filepath]);
        }
    }

    /// Mark a failed push. Clears the inflight marker, increments attempt,
    /// and pushes `updated_at` forward by `backoff_ms` so the worker waits.
    pub(crate) fn push_queue_finalize_failure(
        &self,
        filepath: &str,
        error: &str,
        now_ms: i64,
        backoff_ms: i64,
    ) {
        let conn = self.conn.lock();
        let _ = conn.execute(
            "UPDATE push_queue
                SET inflight_started_at = NULL,
                    inflight_remote_id  = NULL,
                    attempt             = attempt + 1,
                    last_error          = ?2,
                    updated_at          = ?3
              WHERE filepath = ?1",
            rusqlite::params![filepath, error, now_ms + backoff_ms],
        );
    }

    /// Return all rows currently inflight. Used by Loop B to poll status.
    pub(crate) fn push_queue_inflight(&self) -> Vec<InflightRow> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT filepath, inflight_remote_id, inflight_started_at, attempt
               FROM push_queue
              WHERE inflight_started_at IS NOT NULL",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let iter = stmt.query_map([], |r| {
            Ok(InflightRow {
                filepath: r.get(0)?,
                remote_id: r.get::<_, Option<String>>(1)?,
                started_at: r.get::<_, i64>(2)?,
                attempt: r.get::<_, i64>(3)?,
            })
        });
        match iter {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Count rows that still have work to do (either pending or inflight).
    pub(crate) fn push_queue_len(&self) -> usize {
        let conn = self.conn.lock();
        conn.query_row("SELECT COUNT(*) FROM push_queue", [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|n| n as usize)
        .unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushOp {
    Create,
    Update,
    Delete,
    Rename,
}

impl PushOp {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PushOp::Create => "create",
            PushOp::Update => "update",
            PushOp::Delete => "delete",
            PushOp::Rename => "rename",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "create" => Some(PushOp::Create),
            "update" => Some(PushOp::Update),
            "delete" => Some(PushOp::Delete),
            "rename" => Some(PushOp::Rename),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct PushJob {
    pub filepath: String,
    pub op: PushOp,
    pub content_ino: Option<u64>,
    pub rename_to: Option<String>,
    pub attempt: i64,
    pub inflight_remote_id: Option<String>,
}

#[derive(Debug)]
pub(crate) struct InflightRow {
    pub filepath: String,
    pub remote_id: Option<String>,
    pub started_at: i64,
    pub attempt: i64,
}

impl Db {

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
