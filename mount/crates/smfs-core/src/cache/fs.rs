//! [`SupermemoryFs`] — SQLite-backed implementation of the [`FileSystem`] trait.

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use lru::LruCache;
use parking_lot::Mutex;

use super::db::{Db, DENTRY_CACHE_MAX, ROOT_INO};
use super::file::SqliteFile;
use super::profile::{ProfileFile, PROFILE_INO, PROFILE_NAME};
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::mode::{MAX_NAME_LEN, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use crate::vfs::traits::File as _; // bring truncate() into scope
use crate::vfs::traits::{BoxedFile, FileSystem};
use crate::vfs::types::{DirEntry, FileAttr, FilesystemStats, SetAttr, TimeOrNow, Timestamp};

/// A persistent filesystem backed by SQLite.
///
/// Implements the same [`FileSystem`] trait as `MemFs`, but data lives in
/// a SQLite database on disk and survives process restarts.
pub struct SupermemoryFs {
    db: Arc<Db>,
    api: Option<Arc<crate::api::ApiClient>>,
    dentry_cache: Mutex<LruCache<(u64, String), u64>>,
}

impl SupermemoryFs {
    /// Create a new `SupermemoryFs` wrapping an already-opened database (offline mode).
    pub fn new(db: Arc<Db>) -> Self {
        Self {
            db,
            api: None,
            dentry_cache: Mutex::new(LruCache::new(NonZeroUsize::new(DENTRY_CACHE_MAX).unwrap())),
        }
    }

    /// Create a `SupermemoryFs` with an API client for cloud sync.
    pub fn with_api(db: Arc<Db>, api: Arc<crate::api::ApiClient>) -> Self {
        Self {
            db,
            api: Some(api),
            dentry_cache: Mutex::new(LruCache::new(NonZeroUsize::new(DENTRY_CACHE_MAX).unwrap())),
        }
    }

    /// Reconstruct the full filepath for an inode by walking dentries to root.
    fn resolve_filepath(&self, ino: u64) -> Option<String> {
        if ino == super::db::ROOT_INO {
            return Some("/".to_string());
        }

        let conn = self.db.conn.lock();
        let mut parts = Vec::new();
        let mut current = ino;

        loop {
            if current == super::db::ROOT_INO {
                break;
            }
            let row: Option<(String, i64)> = conn
                .query_row(
                    "SELECT name, parent_ino FROM fs_dentry WHERE ino = ?1",
                    [current as i64],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            match row {
                Some((name, parent_ino)) => {
                    parts.push(name);
                    current = parent_ino as u64;
                }
                None => return None,
            }
        }

        parts.reverse();
        Some(format!("/{}", parts.join("/")))
    }

    /// Ensure a directory path exists in the cache, creating intermediate dirs as needed.
    /// Returns the inode of the deepest directory.
    fn ensure_dirs(&self, path: &str) -> VfsResult<u64> {
        let conn = self.db.conn.lock();
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut parent_ino = super::db::ROOT_INO;

        for part in &parts {
            let existing: Option<i64> = conn
                .query_row(
                    "SELECT ino FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                    rusqlite::params![parent_ino as i64, part],
                    |r| r.get(0),
                )
                .ok();

            if let Some(ino) = existing {
                parent_ino = ino as u64;
            } else {
                let now = Timestamp::now();
                conn.execute(
                    "INSERT INTO fs_inode (mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec)
                     VALUES (?1, 2, 0, 0, 0, ?2, ?2, ?2, 0, ?3, ?3, ?3)",
                    rusqlite::params![
                        (S_IFDIR | 0o755) as i64,
                        now.sec,
                        now.nsec as i64,
                    ],
                )
                .map_err(sql_err)?;
                let new_ino = conn.last_insert_rowid() as u64;

                conn.execute(
                    "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?1, ?2, ?3)",
                    rusqlite::params![part, parent_ino as i64, new_ino as i64],
                )
                .map_err(sql_err)?;

                // Update parent nlink
                conn.execute(
                    "UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?1",
                    [parent_ino as i64],
                )
                .map_err(sql_err)?;

                self.dentry_cache
                    .lock()
                    .put((parent_ino, part.to_string()), new_ino);

                parent_ino = new_ino;
            }
        }

        Ok(parent_ino)
    }

    /// Query directory entries from the database.
    fn query_dir_entries(
        &self,
        conn: &rusqlite::Connection,
        parent_ino: u64,
    ) -> VfsResult<Vec<DirEntry>> {
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT d.name, i.{INODE_COLS}
                 FROM fs_dentry d JOIN fs_inode i ON d.ino = i.ino
                 WHERE d.parent_ino = ?1
                 ORDER BY d.name"
            ))
            .map_err(sql_err)?;

        let entries: Vec<DirEntry> = stmt
            .query_map([parent_ino as i64], |row| {
                let name: String = row.get(0)?;
                let attr = FileAttr {
                    ino: row.get::<_, i64>(1)? as u64,
                    mode: row.get::<_, i64>(2)? as u32,
                    nlink: row.get::<_, i64>(3)? as u32,
                    uid: row.get::<_, i64>(4)? as u32,
                    gid: row.get::<_, i64>(5)? as u32,
                    size: row.get::<_, i64>(6)? as u64,
                    blocks: (row.get::<_, i64>(6)? as u64).div_ceil(512),
                    atime: Timestamp {
                        sec: row.get(7)?,
                        nsec: row.get::<_, i64>(11)? as u32,
                    },
                    mtime: Timestamp {
                        sec: row.get(8)?,
                        nsec: row.get::<_, i64>(12)? as u32,
                    },
                    ctime: Timestamp {
                        sec: row.get(9)?,
                        nsec: row.get::<_, i64>(13)? as u32,
                    },
                    rdev: row.get::<_, i64>(10)? as u32,
                    blksize: crate::vfs::PREFERRED_BLOCK_SIZE,
                };
                Ok(DirEntry { name, attr })
            })
            .map_err(sql_err)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(entries)
    }

    /// Pull documents from the API and insert them into the local cache.
    async fn pull_documents(&self, filepath_prefix: &str) -> VfsResult<()> {
        let api = match &self.api {
            Some(a) => a,
            None => return Ok(()),
        };

        let docs = api
            .list_documents(Some(filepath_prefix))
            .await
            .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;

        for doc in &docs {
            let filepath = match &doc.filepath {
                Some(fp) => fp,
                None => continue,
            };

            // Split filepath into directory + filename
            let (dir, filename) = match filepath.rfind('/') {
                Some(pos) => {
                    let dir = if pos == 0 { "/" } else { &filepath[..pos] };
                    let name = &filepath[pos + 1..];
                    (dir, name)
                }
                None => continue,
            };

            if filename.is_empty() {
                continue;
            }

            // Ensure parent directories exist
            let parent_ino = self.ensure_dirs(dir)?;

            // Check if file already exists in cache
            {
                let conn = self.db.conn.lock();
                let exists: bool = conn
                    .query_row(
                        "SELECT 1 FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                        rusqlite::params![parent_ino as i64, filename],
                        |_| Ok(()),
                    )
                    .is_ok();
                if exists {
                    continue;
                }
            }

            // Create file inode
            let content = doc.content.as_deref().unwrap_or("");
            let size = content.len() as i64;
            let now = Timestamp::now();

            let conn = self.db.conn.lock();
            conn.execute(
                "INSERT INTO fs_inode (mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec)
                 VALUES (?1, 1, 0, 0, ?2, ?3, ?3, ?3, 0, ?4, ?4, ?4)",
                rusqlite::params![
                    (S_IFREG | 0o644) as i64,
                    size,
                    now.sec,
                    now.nsec as i64,
                ],
            )
            .map_err(sql_err)?;
            let file_ino = conn.last_insert_rowid() as u64;

            // Create dentry
            conn.execute(
                "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?1, ?2, ?3)",
                rusqlite::params![filename, parent_ino as i64, file_ino as i64],
            )
            .map_err(sql_err)?;

            // Store content as chunks
            if !content.is_empty() {
                let chunk_size = self.db.chunk_size;
                let bytes = content.as_bytes();
                for (i, chunk_data) in bytes.chunks(chunk_size).enumerate() {
                    conn.execute(
                        "INSERT OR REPLACE INTO fs_data (ino, chunk_index, data) VALUES (?1, ?2, ?3)",
                        rusqlite::params![file_ino as i64, i as i64, chunk_data],
                    )
                    .map_err(sql_err)?;
                }
            }

            self.dentry_cache
                .lock()
                .put((parent_ino, filename.to_string()), file_ino);
        }

        Ok(())
    }
}

impl std::fmt::Debug for SupermemoryFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupermemoryFs").finish_non_exhaustive()
    }
}

/// Reject names that are empty, too long, contain a path separator, or contain NUL.
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

fn sql_err(e: rusqlite::Error) -> VfsError {
    VfsError::Io(std::io::Error::other(e.to_string()))
}

const INODE_COLS: &str =
    "ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec";

#[async_trait]
impl FileSystem for SupermemoryFs {
    async fn lookup(&self, parent_ino: u64, name: &str) -> VfsResult<Option<FileAttr>> {
        validate_name(name)?;

        // Virtual profile.md at root.
        if parent_ino == ROOT_INO && name == PROFILE_NAME && self.api.is_some() {
            return Ok(Some(ProfileFile::profile_attr()));
        }

        // All DB work in a sync block — conn must be dropped before any .await.
        let result = {
            let conn = self.db.conn.lock();

            // Verify parent is a directory.
            let parent_mode: i64 = conn
                .query_row(
                    "SELECT mode FROM fs_inode WHERE ino = ?1",
                    [parent_ino as i64],
                    |r| r.get(0),
                )
                .map_err(|_| VfsError::NotFound)?;
            if (parent_mode as u32 & S_IFMT) != S_IFDIR {
                return Err(VfsError::NotADirectory);
            }

            // Check dentry cache.
            {
                let mut cache = self.dentry_cache.lock();
                if let Some(&child_ino) = cache.get(&(parent_ino, name.to_string())) {
                    drop(cache);
                    let attr = conn
                        .query_row(
                            &format!("SELECT {INODE_COLS} FROM fs_inode WHERE ino = ?1"),
                            [child_ino as i64],
                            Db::row_to_attr,
                        )
                        .ok();
                    return Ok(attr);
                }
            }

            // Cache miss — query dentry table.
            let child_ino: Option<i64> = conn
                .query_row(
                    "SELECT ino FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                    rusqlite::params![parent_ino as i64, name],
                    |r| r.get(0),
                )
                .ok();

            if let Some(child_ino) = child_ino {
                self.dentry_cache
                    .lock()
                    .put((parent_ino, name.to_string()), child_ino as u64);

                let attr = conn
                    .query_row(
                        &format!("SELECT {INODE_COLS} FROM fs_inode WHERE ino = ?1"),
                        [child_ino],
                        Db::row_to_attr,
                    )
                    .ok();

                return Ok(attr);
            }

            None::<FileAttr>
        }; // conn dropped here

        if result.is_some() {
            return Ok(result);
        }

        // Not in local cache — try API pull.
        if self.api.is_some() {
            if let Some(parent_path) = self.resolve_filepath(parent_ino) {
                let sep = if parent_path.ends_with('/') { "" } else { "/" };
                let file_path = format!("{parent_path}{sep}{name}");
                let _ = self.pull_documents(&file_path).await;

                // Retry from cache.
                let conn = self.db.conn.lock();
                let child_ino: Option<i64> = conn
                    .query_row(
                        "SELECT ino FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                        rusqlite::params![parent_ino as i64, name],
                        |r| r.get(0),
                    )
                    .ok();

                if let Some(child_ino) = child_ino {
                    self.dentry_cache
                        .lock()
                        .put((parent_ino, name.to_string()), child_ino as u64);

                    let attr = conn
                        .query_row(
                            &format!("SELECT {INODE_COLS} FROM fs_inode WHERE ino = ?1"),
                            [child_ino],
                            Db::row_to_attr,
                        )
                        .ok();

                    return Ok(attr);
                }
            }
        }

        Ok(None)
    }

    async fn getattr(&self, ino: u64) -> VfsResult<Option<FileAttr>> {
        if ino == PROFILE_INO && self.api.is_some() {
            return Ok(Some(ProfileFile::profile_attr()));
        }
        let conn = self.db.conn.lock();
        let attr = conn
            .query_row(
                &format!("SELECT {INODE_COLS} FROM fs_inode WHERE ino = ?1"),
                [ino as i64],
                Db::row_to_attr,
            )
            .ok();
        Ok(attr)
    }

    async fn setattr(&self, ino: u64, attr: SetAttr) -> VfsResult<FileAttr> {
        // Check mode and handle size change in a block that drops conn before any .await.
        let needs_truncate = {
            let conn = self.db.conn.lock();
            let current_mode: i64 = conn
                .query_row(
                    "SELECT mode FROM fs_inode WHERE ino = ?1",
                    [ino as i64],
                    |r| r.get(0),
                )
                .map_err(|_| VfsError::NotFound)?;

            if let Some(_new_size) = attr.size {
                let ftype = current_mode as u32 & S_IFMT;
                if ftype == S_IFDIR {
                    return Err(VfsError::IsADirectory);
                }
                if ftype == S_IFLNK {
                    return Err(VfsError::NotSupported);
                }
                true
            } else {
                false
            }
        }; // conn dropped here

        if needs_truncate {
            let file = SqliteFile {
                db: self.db.clone(),
                ino,
                api: None,
                filepath: None,
            };
            file.truncate(attr.size.unwrap()).await?;
        }

        let conn = self.db.conn.lock();
        self.apply_metadata_updates(&conn, ino, &attr)
    }

    async fn readdir(&self, ino: u64) -> VfsResult<Option<Vec<String>>> {
        // All DB work in a sync block so conn/stmt are dropped before any .await.
        let mut names = {
            let conn = self.db.conn.lock();

            let mode: Option<i64> = conn
                .query_row(
                    "SELECT mode FROM fs_inode WHERE ino = ?1",
                    [ino as i64],
                    |r| r.get(0),
                )
                .ok();
            let Some(mode) = mode else {
                return Ok(None);
            };
            if (mode as u32 & S_IFMT) != S_IFDIR {
                return Ok(None);
            }

            let mut stmt = conn
                .prepare_cached("SELECT name FROM fs_dentry WHERE parent_ino = ?1 ORDER BY name")
                .map_err(sql_err)?;
            let names: Vec<String> = stmt
                .query_map([ino as i64], |r| r.get(0))
                .map_err(sql_err)?
                .filter_map(|r| r.ok())
                .collect();
            names
        }; // conn + stmt dropped here

        if !names.is_empty() || self.api.is_none() {
            if ino == ROOT_INO && self.api.is_some() && !names.contains(&PROFILE_NAME.to_string()) {
                names.push(PROFILE_NAME.to_string());
            }
            return Ok(Some(names));
        }

        // Empty directory — try API pull.
        if let Some(dir_path) = self.resolve_filepath(ino) {
            let prefix = if dir_path.ends_with('/') {
                dir_path
            } else {
                format!("{dir_path}/")
            };
            let _ = self.pull_documents(&prefix).await;

            let conn = self.db.conn.lock();
            let mut stmt = conn
                .prepare_cached("SELECT name FROM fs_dentry WHERE parent_ino = ?1 ORDER BY name")
                .map_err(sql_err)?;
            let mut names: Vec<String> = stmt
                .query_map([ino as i64], |r| r.get(0))
                .map_err(sql_err)?
                .filter_map(|r| r.ok())
                .collect();
            if ino == ROOT_INO && self.api.is_some() && !names.contains(&PROFILE_NAME.to_string()) {
                names.push(PROFILE_NAME.to_string());
            }
            return Ok(Some(names));
        }

        Ok(Some(Vec::new()))
    }

    async fn readdir_plus(&self, ino: u64) -> VfsResult<Option<Vec<DirEntry>>> {
        // First pass: query from local cache. All DB work in one sync block.
        let entries = {
            let conn = self.db.conn.lock();

            let mode: Option<i64> = conn
                .query_row(
                    "SELECT mode FROM fs_inode WHERE ino = ?1",
                    [ino as i64],
                    |r| r.get(0),
                )
                .ok();
            let Some(mode) = mode else {
                return Ok(None);
            };
            if (mode as u32 & S_IFMT) != S_IFDIR {
                return Ok(None);
            }

            self.query_dir_entries(&conn, ino)?
        }; // conn dropped here

        let append_profile = |mut entries: Vec<DirEntry>| -> Vec<DirEntry> {
            if ino == ROOT_INO
                && self.api.is_some()
                && !entries.iter().any(|e| e.name == PROFILE_NAME)
            {
                entries.push(DirEntry {
                    name: PROFILE_NAME.to_string(),
                    attr: ProfileFile::profile_attr(),
                });
            }
            entries
        };

        if !entries.is_empty() || self.api.is_none() {
            return Ok(Some(append_profile(entries)));
        }

        // Empty directory — try API pull.
        if let Some(dir_path) = self.resolve_filepath(ino) {
            let prefix = if dir_path.ends_with('/') {
                dir_path
            } else {
                format!("{dir_path}/")
            };
            let _ = self.pull_documents(&prefix).await;

            let conn = self.db.conn.lock();
            let entries = self.query_dir_entries(&conn, ino)?;
            return Ok(Some(append_profile(entries)));
        }

        Ok(Some(Vec::new()))
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
        let conn = self.db.conn.lock();

        // Verify parent is a directory.
        let parent_mode: i64 = conn
            .query_row(
                "SELECT mode FROM fs_inode WHERE ino = ?1",
                [parent_ino as i64],
                |r| r.get(0),
            )
            .map_err(|_| VfsError::NotFound)?;
        if (parent_mode as u32 & S_IFMT) != S_IFDIR {
            return Err(VfsError::NotADirectory);
        }

        // Check name doesn't already exist.
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                rusqlite::params![parent_ino as i64, name],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if exists {
            return Err(VfsError::AlreadyExists);
        }

        let now = Timestamp::now();
        let dir_mode = S_IFDIR | (mode & 0o7777);

        let tx = conn.unchecked_transaction().map_err(sql_err)?;

        // Insert inode.
        tx.execute(
            "INSERT INTO fs_inode (mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec)
             VALUES (?1, 2, ?2, ?3, 0, ?4, ?5, ?6, 0, ?7, ?8, ?9)",
            rusqlite::params![dir_mode as i64, uid, gid, now.sec, now.sec, now.sec, now.nsec, now.nsec, now.nsec],
        ).map_err(sql_err)?;
        let ino = tx.last_insert_rowid() as u64;

        // Insert dentry.
        tx.execute(
            "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?1, ?2, ?3)",
            rusqlite::params![name, parent_ino as i64, ino as i64],
        )
        .map_err(sql_err)?;

        // Update parent: bump nlink, update timestamps.
        tx.execute(
            "UPDATE fs_inode SET nlink = nlink + 1, mtime = ?1, ctime = ?2, mtime_nsec = ?3, ctime_nsec = ?4
             WHERE ino = ?5",
            rusqlite::params![now.sec, now.sec, now.nsec, now.nsec, parent_ino as i64],
        )
        .map_err(sql_err)?;

        tx.commit().map_err(sql_err)?;

        self.dentry_cache
            .lock()
            .put((parent_ino, name.to_string()), ino);

        Ok(FileAttr::new_dir_with(ino, dir_mode, uid, gid, now))
    }

    async fn rmdir(&self, parent_ino: u64, name: &str) -> VfsResult<()> {
        validate_name(name)?;
        let conn = self.db.conn.lock();

        // Look up child.
        let child_ino: i64 = conn
            .query_row(
                "SELECT ino FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                rusqlite::params![parent_ino as i64, name],
                |r| r.get(0),
            )
            .map_err(|_| VfsError::NotFound)?;

        // Verify child is a directory.
        let child_mode: i64 = conn
            .query_row(
                "SELECT mode FROM fs_inode WHERE ino = ?1",
                [child_ino],
                |r| r.get(0),
            )
            .map_err(|_| VfsError::NotFound)?;
        if (child_mode as u32 & S_IFMT) != S_IFDIR {
            return Err(VfsError::NotADirectory);
        }

        // Verify empty.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fs_dentry WHERE parent_ino = ?1",
                [child_ino],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if count > 0 {
            return Err(VfsError::NotEmpty);
        }

        let now = Timestamp::now();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;

        // Delete dentry.
        tx.execute(
            "DELETE FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
            rusqlite::params![parent_ino as i64, name],
        )
        .map_err(sql_err)?;

        // Delete child inode.
        tx.execute("DELETE FROM fs_inode WHERE ino = ?1", [child_ino])
            .map_err(sql_err)?;

        // Update parent: decrement nlink, update timestamps.
        tx.execute(
            "UPDATE fs_inode SET nlink = MAX(nlink - 1, 2), mtime = ?1, ctime = ?2, mtime_nsec = ?3, ctime_nsec = ?4
             WHERE ino = ?5",
            rusqlite::params![now.sec, now.sec, now.nsec, now.nsec, parent_ino as i64],
        )
        .map_err(sql_err)?;

        tx.commit().map_err(sql_err)?;

        self.dentry_cache
            .lock()
            .pop(&(parent_ino, name.to_string()));

        Ok(())
    }

    async fn open(&self, ino: u64, _flags: i32) -> VfsResult<BoxedFile> {
        if ino == PROFILE_INO {
            if let Some(ref api) = self.api {
                return Ok(Arc::new(ProfileFile::new(api.clone())));
            }
            return Err(VfsError::NotFound);
        }
        {
            let conn = self.db.conn.lock();
            let mode: i64 = conn
                .query_row(
                    "SELECT mode FROM fs_inode WHERE ino = ?1",
                    [ino as i64],
                    |r| r.get(0),
                )
                .map_err(|_| VfsError::NotFound)?;

            let ftype = mode as u32 & S_IFMT;
            if ftype == S_IFDIR {
                return Err(VfsError::IsADirectory);
            }
            if ftype == S_IFLNK {
                return Err(VfsError::NotSupported);
            }
        } // conn dropped before resolve_filepath

        let filepath = self.resolve_filepath(ino);

        Ok(Arc::new(SqliteFile {
            db: self.db.clone(),
            ino,
            api: self.api.clone(),
            filepath,
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
        let conn = self.db.conn.lock();

        // Verify parent is a directory.
        let parent_mode: i64 = conn
            .query_row(
                "SELECT mode FROM fs_inode WHERE ino = ?1",
                [parent_ino as i64],
                |r| r.get(0),
            )
            .map_err(|_| VfsError::NotFound)?;
        if (parent_mode as u32 & S_IFMT) != S_IFDIR {
            return Err(VfsError::NotADirectory);
        }

        // Check name doesn't already exist.
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                rusqlite::params![parent_ino as i64, name],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if exists {
            return Err(VfsError::AlreadyExists);
        }

        let now = Timestamp::now();
        let file_mode = S_IFREG | (mode & 0o7777);

        let tx = conn.unchecked_transaction().map_err(sql_err)?;

        tx.execute(
            "INSERT INTO fs_inode (mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec)
             VALUES (?1, 1, ?2, ?3, 0, ?4, ?5, ?6, 0, ?7, ?8, ?9)",
            rusqlite::params![file_mode as i64, uid, gid, now.sec, now.sec, now.sec, now.nsec, now.nsec, now.nsec],
        ).map_err(sql_err)?;
        let ino = tx.last_insert_rowid() as u64;

        tx.execute(
            "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?1, ?2, ?3)",
            rusqlite::params![name, parent_ino as i64, ino as i64],
        )
        .map_err(sql_err)?;

        tx.execute(
            "UPDATE fs_inode SET mtime = ?1, ctime = ?2, mtime_nsec = ?3, ctime_nsec = ?4 WHERE ino = ?5",
            rusqlite::params![now.sec, now.sec, now.nsec, now.nsec, parent_ino as i64],
        )
        .map_err(sql_err)?;

        tx.commit().map_err(sql_err)?;
        drop(conn); // drop before resolve_filepath to avoid deadlock

        self.dentry_cache
            .lock()
            .put((parent_ino, name.to_string()), ino);

        let attr = FileAttr::new_file_with(ino, file_mode, uid, gid, now);

        let filepath = self.resolve_filepath(parent_ino).map(|p| {
            let sep = if p.ends_with('/') { "" } else { "/" };
            format!("{p}{sep}{name}")
        });

        let handle: BoxedFile = Arc::new(SqliteFile {
            db: self.db.clone(),
            ino,
            api: self.api.clone(),
            filepath,
        });

        Ok((attr, handle))
    }

    async fn unlink(&self, parent_ino: u64, name: &str) -> VfsResult<()> {
        validate_name(name)?;

        if parent_ino == ROOT_INO && name == PROFILE_NAME {
            return Err(VfsError::PermissionDenied);
        }

        // Resolve filepath BEFORE deleting dentry (needed for API sync).
        let filepath_for_api = self.resolve_filepath(parent_ino).map(|p| {
            let sep = if p.ends_with('/') { "" } else { "/" };
            format!("{p}{sep}{name}")
        });

        let conn = self.db.conn.lock();

        // Look up child.
        let child_ino: i64 = conn
            .query_row(
                "SELECT ino FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                rusqlite::params![parent_ino as i64, name],
                |r| r.get(0),
            )
            .map_err(|_| VfsError::NotFound)?;

        // Verify not a directory.
        let child_mode: i64 = conn
            .query_row(
                "SELECT mode FROM fs_inode WHERE ino = ?1",
                [child_ino],
                |r| r.get(0),
            )
            .map_err(|_| VfsError::NotFound)?;
        if (child_mode as u32 & S_IFMT) == S_IFDIR {
            return Err(VfsError::IsADirectory);
        }

        let now = Timestamp::now();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;

        // Delete dentry.
        tx.execute(
            "DELETE FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
            rusqlite::params![parent_ino as i64, name],
        )
        .map_err(sql_err)?;

        // Update parent timestamps.
        tx.execute(
            "UPDATE fs_inode SET mtime = ?1, ctime = ?2, mtime_nsec = ?3, ctime_nsec = ?4 WHERE ino = ?5",
            rusqlite::params![now.sec, now.sec, now.nsec, now.nsec, parent_ino as i64],
        )
        .map_err(sql_err)?;

        // Decrement nlink.
        tx.execute(
            "UPDATE fs_inode SET nlink = nlink - 1, ctime = ?1, ctime_nsec = ?2 WHERE ino = ?3",
            rusqlite::params![now.sec, now.nsec, child_ino],
        )
        .map_err(sql_err)?;

        // Check if we need to delete the inode.
        let nlink: i64 = tx
            .query_row(
                "SELECT nlink FROM fs_inode WHERE ino = ?1",
                [child_ino],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if nlink <= 0 {
            tx.execute("DELETE FROM fs_data WHERE ino = ?1", [child_ino])
                .map_err(sql_err)?;
            tx.execute("DELETE FROM fs_symlink WHERE ino = ?1", [child_ino])
                .map_err(sql_err)?;
            tx.execute("DELETE FROM fs_remote WHERE ino = ?1", [child_ino])
                .map_err(sql_err)?;
            tx.execute("DELETE FROM fs_inode WHERE ino = ?1", [child_ino])
                .map_err(sql_err)?;
        }

        tx.commit().map_err(sql_err)?;
        drop(conn);

        self.dentry_cache
            .lock()
            .pop(&(parent_ino, name.to_string()));

        // Push delete to API (fire-and-forget).
        if let Some(ref api) = self.api {
            if let Some(fp) = filepath_for_api {
                let api = api.clone();
                tokio::spawn(async move {
                    match api.delete_documents(&fp).await {
                        Ok(r) => {
                            tracing::debug!(filepath = %fp, deleted = r.deleted_count, "deleted from API")
                        }
                        Err(e) => {
                            tracing::warn!(filepath = %fp, error = %e, "failed to delete from API")
                        }
                    }
                });
            }
        }

        Ok(())
    }

    async fn readlink(&self, ino: u64) -> VfsResult<Option<String>> {
        let conn = self.db.conn.lock();

        let mode: Option<i64> = conn
            .query_row(
                "SELECT mode FROM fs_inode WHERE ino = ?1",
                [ino as i64],
                |r| r.get(0),
            )
            .ok();
        let Some(mode) = mode else {
            return Ok(None);
        };
        if (mode as u32 & S_IFMT) != S_IFLNK {
            return Err(VfsError::NotASymlink);
        }

        let target: String = conn
            .query_row(
                "SELECT target FROM fs_symlink WHERE ino = ?1",
                [ino as i64],
                |r| r.get(0),
            )
            .map_err(|_| VfsError::NotFound)?;

        Ok(Some(target))
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
        let conn = self.db.conn.lock();

        // Verify parent is a directory.
        let parent_mode: i64 = conn
            .query_row(
                "SELECT mode FROM fs_inode WHERE ino = ?1",
                [parent_ino as i64],
                |r| r.get(0),
            )
            .map_err(|_| VfsError::NotFound)?;
        if (parent_mode as u32 & S_IFMT) != S_IFDIR {
            return Err(VfsError::NotADirectory);
        }

        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                rusqlite::params![parent_ino as i64, name],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if exists {
            return Err(VfsError::AlreadyExists);
        }

        let now = Timestamp::now();
        let symlink_mode = S_IFLNK | 0o777;
        let size = target.len() as i64;

        let tx = conn.unchecked_transaction().map_err(sql_err)?;

        tx.execute(
            "INSERT INTO fs_inode (mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec)
             VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, ?10)",
            rusqlite::params![symlink_mode as i64, uid, gid, size, now.sec, now.sec, now.sec, now.nsec, now.nsec, now.nsec],
        ).map_err(sql_err)?;
        let ino = tx.last_insert_rowid() as u64;

        tx.execute(
            "INSERT INTO fs_symlink (ino, target) VALUES (?1, ?2)",
            rusqlite::params![ino as i64, target],
        )
        .map_err(sql_err)?;

        tx.execute(
            "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?1, ?2, ?3)",
            rusqlite::params![name, parent_ino as i64, ino as i64],
        )
        .map_err(sql_err)?;

        tx.execute(
            "UPDATE fs_inode SET mtime = ?1, ctime = ?2, mtime_nsec = ?3, ctime_nsec = ?4 WHERE ino = ?5",
            rusqlite::params![now.sec, now.sec, now.nsec, now.nsec, parent_ino as i64],
        )
        .map_err(sql_err)?;

        tx.commit().map_err(sql_err)?;

        self.dentry_cache
            .lock()
            .put((parent_ino, name.to_string()), ino);

        Ok(FileAttr::new_symlink(ino, target.len() as u64, uid, gid))
    }

    async fn link(&self, ino: u64, new_parent_ino: u64, new_name: &str) -> VfsResult<FileAttr> {
        validate_name(new_name)?;
        let conn = self.db.conn.lock();

        // Verify source exists and is not a directory.
        let src_mode: i64 = conn
            .query_row(
                "SELECT mode FROM fs_inode WHERE ino = ?1",
                [ino as i64],
                |r| r.get(0),
            )
            .map_err(|_| VfsError::NotFound)?;
        if (src_mode as u32 & S_IFMT) == S_IFDIR {
            return Err(VfsError::IsADirectory);
        }

        // Verify new parent is a directory.
        let parent_mode: i64 = conn
            .query_row(
                "SELECT mode FROM fs_inode WHERE ino = ?1",
                [new_parent_ino as i64],
                |r| r.get(0),
            )
            .map_err(|_| VfsError::NotFound)?;
        if (parent_mode as u32 & S_IFMT) != S_IFDIR {
            return Err(VfsError::NotADirectory);
        }

        // Check name doesn't already exist.
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                rusqlite::params![new_parent_ino as i64, new_name],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if exists {
            return Err(VfsError::AlreadyExists);
        }

        let now = Timestamp::now();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;

        // Increment nlink.
        tx.execute(
            "UPDATE fs_inode SET nlink = nlink + 1, ctime = ?1, ctime_nsec = ?2 WHERE ino = ?3",
            rusqlite::params![now.sec, now.nsec, ino as i64],
        )
        .map_err(sql_err)?;

        // Insert dentry.
        tx.execute(
            "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?1, ?2, ?3)",
            rusqlite::params![new_name, new_parent_ino as i64, ino as i64],
        )
        .map_err(sql_err)?;

        // Update parent timestamps.
        tx.execute(
            "UPDATE fs_inode SET mtime = ?1, ctime = ?2, mtime_nsec = ?3, ctime_nsec = ?4 WHERE ino = ?5",
            rusqlite::params![now.sec, now.sec, now.nsec, now.nsec, new_parent_ino as i64],
        )
        .map_err(sql_err)?;

        tx.commit().map_err(sql_err)?;

        self.dentry_cache
            .lock()
            .put((new_parent_ino, new_name.to_string()), ino);

        let attr = conn
            .query_row(
                &format!("SELECT {INODE_COLS} FROM fs_inode WHERE ino = ?1"),
                [ino as i64],
                Db::row_to_attr,
            )
            .map_err(|_| VfsError::NotFound)?;

        Ok(attr)
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

        // Resolve old filepath BEFORE rename (needed for API sync).
        let old_filepath = self.resolve_filepath(old_parent_ino).map(|p| {
            let sep = if p.ends_with('/') { "" } else { "/" };
            format!("{p}{sep}{old_name}")
        });

        // Resolve destination filepath BEFORE rename (needed to delete overwritten doc from API).
        let dst_filepath_for_delete = self.resolve_filepath(new_parent_ino).map(|p| {
            let sep = if p.ends_with('/') { "" } else { "/" };
            format!("{p}{sep}{new_name}")
        });

        // All DB work in a block so conn/tx are dropped before any .await.
        let (src_ino, did_overwrite) = {
            let conn = self.db.conn.lock();

            // Find source.
            let src_ino: i64 = conn
                .query_row(
                    "SELECT ino FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                    rusqlite::params![old_parent_ino as i64, old_name],
                    |r| r.get(0),
                )
                .map_err(|_| VfsError::NotFound)?;

            // Verify destination parent is a directory.
            let dst_parent_mode: i64 = conn
                .query_row(
                    "SELECT mode FROM fs_inode WHERE ino = ?1",
                    [new_parent_ino as i64],
                    |r| r.get(0),
                )
                .map_err(|_| VfsError::NotFound)?;
            if (dst_parent_mode as u32 & S_IFMT) != S_IFDIR {
                return Err(VfsError::NotADirectory);
            }

            // Check if destination exists.
            let dst_existing: Option<i64> = conn
                .query_row(
                    "SELECT ino FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                    rusqlite::params![new_parent_ino as i64, new_name],
                    |r| r.get(0),
                )
                .ok();
            let mut did_overwrite = false;

            let src_mode: i64 = conn
                .query_row("SELECT mode FROM fs_inode WHERE ino = ?1", [src_ino], |r| {
                    r.get(0)
                })
                .map_err(sql_err)?;
            let src_is_dir = (src_mode as u32 & S_IFMT) == S_IFDIR;

            let tx = conn.unchecked_transaction().map_err(sql_err)?;

            if let Some(dst_ino) = dst_existing {
                if dst_ino == src_ino {
                    return Ok(()); // rename-to-same — no-op
                }

                let dst_mode: i64 = tx
                    .query_row("SELECT mode FROM fs_inode WHERE ino = ?1", [dst_ino], |r| {
                        r.get(0)
                    })
                    .map_err(sql_err)?;
                let dst_is_dir = (dst_mode as u32 & S_IFMT) == S_IFDIR;

                match (src_is_dir, dst_is_dir) {
                    (true, false) => return Err(VfsError::NotADirectory),
                    (false, true) => return Err(VfsError::IsADirectory),
                    (true, true) => {
                        let count: i64 = tx
                            .query_row(
                                "SELECT COUNT(*) FROM fs_dentry WHERE parent_ino = ?1",
                                [dst_ino],
                                |r| r.get(0),
                            )
                            .map_err(sql_err)?;
                        if count > 0 {
                            return Err(VfsError::NotEmpty);
                        }
                    }
                    (false, false) => {}
                }

                // Remove destination.
                tx.execute(
                    "DELETE FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                    rusqlite::params![new_parent_ino as i64, new_name],
                )
                .map_err(sql_err)?;
                tx.execute("DELETE FROM fs_data WHERE ino = ?1", [dst_ino])
                    .map_err(sql_err)?;
                tx.execute("DELETE FROM fs_symlink WHERE ino = ?1", [dst_ino])
                    .map_err(sql_err)?;
                tx.execute("DELETE FROM fs_remote WHERE ino = ?1", [dst_ino])
                    .map_err(sql_err)?;
                tx.execute("DELETE FROM fs_inode WHERE ino = ?1", [dst_ino])
                    .map_err(sql_err)?;
                did_overwrite = true;
            }

            let now = Timestamp::now();

            // Atomic rename: update the dentry.
            tx.execute(
            "UPDATE fs_dentry SET parent_ino = ?1, name = ?2 WHERE parent_ino = ?3 AND name = ?4",
            rusqlite::params![
                new_parent_ino as i64,
                new_name,
                old_parent_ino as i64,
                old_name
            ],
        )
        .map_err(sql_err)?;

            // Update timestamps on source inode and both parents.
            tx.execute(
                "UPDATE fs_inode SET ctime = ?1, ctime_nsec = ?2 WHERE ino = ?3",
                rusqlite::params![now.sec, now.nsec, src_ino],
            )
            .map_err(sql_err)?;

            tx.execute(
            "UPDATE fs_inode SET mtime = ?1, ctime = ?2, mtime_nsec = ?3, ctime_nsec = ?4 WHERE ino = ?5",
            rusqlite::params![now.sec, now.sec, now.nsec, now.nsec, old_parent_ino as i64],
        )
        .map_err(sql_err)?;

            if new_parent_ino != old_parent_ino {
                tx.execute(
                "UPDATE fs_inode SET mtime = ?1, ctime = ?2, mtime_nsec = ?3, ctime_nsec = ?4 WHERE ino = ?5",
                rusqlite::params![now.sec, now.sec, now.nsec, now.nsec, new_parent_ino as i64],
            )
            .map_err(sql_err)?;
            }

            tx.commit().map_err(sql_err)?;

            (src_ino, did_overwrite)
        }; // conn + tx dropped here

        {
            let mut cache = self.dentry_cache.lock();
            cache.pop(&(old_parent_ino, old_name.to_string()));
            cache.pop(&(new_parent_ino, new_name.to_string()));
            cache.put((new_parent_ino, new_name.to_string()), src_ino as u64);
        }

        // Resolve new filepath after rename.
        let new_filepath = self.resolve_filepath(new_parent_ino).map(|p| {
            let sep = if p.ends_with('/') { "" } else { "/" };
            format!("{p}{sep}{new_name}")
        });

        // Delete overwritten destination from API.
        if did_overwrite {
            if let Some(ref api) = self.api {
                if let Some(ref dst_fp) = dst_filepath_for_delete {
                    let api = api.clone();
                    let dst_fp = dst_fp.clone();
                    tokio::spawn(async move {
                        match api.delete_documents(&dst_fp).await {
                            Ok(r) => {
                                tracing::debug!(filepath = %dst_fp, deleted = r.deleted_count, "deleted overwritten file from API")
                            }
                            Err(e) => {
                                tracing::warn!(filepath = %dst_fp, error = %e, "failed to delete overwritten file from API")
                            }
                        }
                    });
                }
            }
        }

        // Push rename to API.
        if let Some(ref api) = self.api {
            if let (Some(old_fp), Some(new_fp)) = (old_filepath, new_filepath) {
                match api.list_documents(Some(&old_fp)).await {
                    Ok(docs) => {
                        for doc in docs {
                            let req = crate::api::UpdateDocumentReq {
                                filepath: Some(new_fp.clone()),
                                content: None,
                            };
                            match api.update_document(&doc.id, &req).await {
                                Ok(_) => {
                                    tracing::debug!(old = %old_fp, new = %new_fp, "renamed in API")
                                }
                                Err(e) => {
                                    tracing::warn!(old = %old_fp, error = %e, "API rename failed")
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(old = %old_fp, error = %e, "API list for rename failed")
                    }
                }
            }
        }

        Ok(())
    }

    async fn statfs(&self) -> VfsResult<FilesystemStats> {
        let conn = self.db.conn.lock();
        let inodes: i64 = conn
            .query_row("SELECT COUNT(*) FROM fs_inode", [], |r| r.get(0))
            .map_err(sql_err)?;
        let bytes_used: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(size), 0) FROM fs_inode WHERE (mode & ?1) != ?2",
                rusqlite::params![S_IFMT as i64, S_IFDIR as i64],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        Ok(FilesystemStats {
            inodes: inodes as u64,
            bytes_used: bytes_used as u64,
        })
    }
}

impl SupermemoryFs {
    /// Apply metadata updates (mode, uid, gid, atime, mtime) from a SetAttr.
    /// Size changes are handled separately before calling this.
    fn apply_metadata_updates(
        &self,
        conn: &rusqlite::Connection,
        ino: u64,
        attr: &SetAttr,
    ) -> VfsResult<FileAttr> {
        let now = Timestamp::now();

        if let Some(mode) = attr.mode {
            conn.execute(
                "UPDATE fs_inode SET mode = (mode & ?1) | (?2 & ~?1) WHERE ino = ?3",
                rusqlite::params![S_IFMT as i64, mode as i64, ino as i64],
            )
            .map_err(sql_err)?;
        }
        if let Some(uid) = attr.uid {
            conn.execute(
                "UPDATE fs_inode SET uid = ?1 WHERE ino = ?2",
                rusqlite::params![uid, ino as i64],
            )
            .map_err(sql_err)?;
        }
        if let Some(gid) = attr.gid {
            conn.execute(
                "UPDATE fs_inode SET gid = ?1 WHERE ino = ?2",
                rusqlite::params![gid, ino as i64],
            )
            .map_err(sql_err)?;
        }
        if let Some(time) = &attr.atime {
            let ts = match time {
                TimeOrNow::Now => now,
                TimeOrNow::Time(t) => *t,
            };
            conn.execute(
                "UPDATE fs_inode SET atime = ?1, atime_nsec = ?2 WHERE ino = ?3",
                rusqlite::params![ts.sec, ts.nsec, ino as i64],
            )
            .map_err(sql_err)?;
        }
        if let Some(time) = &attr.mtime {
            let ts = match time {
                TimeOrNow::Now => now,
                TimeOrNow::Time(t) => *t,
            };
            conn.execute(
                "UPDATE fs_inode SET mtime = ?1, mtime_nsec = ?2 WHERE ino = ?3",
                rusqlite::params![ts.sec, ts.nsec, ino as i64],
            )
            .map_err(sql_err)?;
        }

        // Always touch ctime.
        conn.execute(
            "UPDATE fs_inode SET ctime = ?1, ctime_nsec = ?2 WHERE ino = ?3",
            rusqlite::params![now.sec, now.nsec, ino as i64],
        )
        .map_err(sql_err)?;

        conn.query_row(
            &format!("SELECT {INODE_COLS} FROM fs_inode WHERE ino = ?1"),
            [ino as i64],
            Db::row_to_attr,
        )
        .map_err(|_| VfsError::NotFound)
    }
}
