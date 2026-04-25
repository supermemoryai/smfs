//! [`SupermemoryFs`] — SQLite-backed implementation of the [`FileSystem`] trait.

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use lru::LruCache;
use parking_lot::Mutex;

use super::db::{Db, DENTRY_CACHE_MAX, ROOT_INO};
use super::file::SqliteFile;
use super::profile::{ProfileFile, PROFILE_INO, PROFILE_NAME};

const DERIVED_SIBLING_SUFFIXES: &[&str] = &[
    ".image-transcription.md",
    ".pdf-transcription.md",
    ".video-transcription.md",
    ".audio-transcription.md",
    ".webpage-transcription.md",
    ".smfs-error.txt",
];
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
    profile_file: Option<Arc<ProfileFile>>,
    dentry_cache: Mutex<LruCache<(u64, String), u64>>,
}

impl SupermemoryFs {
    /// Create a new `SupermemoryFs` wrapping an already-opened database (offline mode).
    pub fn new(db: Arc<Db>) -> Self {
        Self {
            db,
            api: None,
            profile_file: None,
            dentry_cache: Mutex::new(LruCache::new(NonZeroUsize::new(DENTRY_CACHE_MAX).unwrap())),
        }
    }

    /// Create a `SupermemoryFs` with an API client for cloud sync.
    pub fn with_api(db: Arc<Db>, api: Arc<crate::api::ApiClient>) -> Self {
        let profile_file = Arc::new(ProfileFile::new(api.clone()));
        Self {
            db,
            api: Some(api),
            profile_file: Some(profile_file),
            dentry_cache: Mutex::new(LruCache::new(NonZeroUsize::new(DENTRY_CACHE_MAX).unwrap())),
        }
    }

    pub async fn warm_profile(&self) {
        if let Some(pf) = &self.profile_file {
            pf.warm().await;
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

    /// Pull documents from the API and reconcile each into the local cache.
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
            let _ = self.reconcile_one(doc);
        }

        Ok(())
    }

    /// Reconcile a single remote document against the local cache.
    /// Versioning rules: if local `dirty_since` is newer than the remote's
    /// `updatedAt`, skip (local write in progress wins). Otherwise create,
    /// rename, or rewrite chunks as needed.
    pub(crate) fn reconcile_one(&self, doc: &crate::api::Document) -> VfsResult<ReconcileOutcome> {
        let synth;
        let filepath: &str = match doc.filepath.as_deref() {
            Some(fp) if fp.contains('/') && !fp.ends_with('/') => fp,
            _ => {
                synth = format!("/{}.md", doc.id);
                &synth
            }
        };

        let pos = filepath.rfind('/').expect("synth guarantees a '/'");
        let dir = if pos == 0 { "/" } else { &filepath[..pos] };
        let filename = &filepath[pos + 1..];

        let updated_ms = parse_iso_to_ms(&doc.updated_at);
        // For binary types, `doc.content` holds extracted text, not the raw
        // file — never overwrite the local raw inode with it.
        let is_binary_type = matches!(
            doc.type_.as_deref(),
            Some("image") | Some("pdf") | Some("video") | Some("audio") | Some("webpage")
        );

        // If an inode is already mapped to this remote_id, update it in place.
        if let Some(existing_ino) = self.db.ino_by_remote_id(&doc.id) {
            // Local write is newer — defer to push queue, don't clobber.
            if let Some(dirty_since) = self.db.get_dirty_since(existing_ino) {
                if let Some(ms) = updated_ms {
                    if dirty_since > ms {
                        return Ok(ReconcileOutcome::SkippedDirty);
                    }
                }
            }

            // Have we already mirrored this version?
            let mirrored = self
                .db
                .get_mirrored_state(existing_ino)
                .and_then(|(u, _, _)| u);
            if let (Some(m), Some(u)) = (mirrored, updated_ms) {
                if m >= u && doc.status == "done" {
                    self.sync_transcription_sibling(doc, filepath);
                    self.sync_error_sibling(doc, filepath);
                    return Ok(ReconcileOutcome::Unchanged);
                }
            }

            // Handle rename: current resolved path vs remote filepath.
            let current_fp = self.resolve_filepath(existing_ino);
            if current_fp.as_deref() != Some(filepath) {
                self.apply_rename_to(existing_ino, dir, filename)?;
            }

            // Only rewrite chunks when processing is complete — partial content
            // from docs mid-pipeline would overwrite good local data. Critical:
            // we also only advance `mirrored_updated_at` when we actually
            // mirrored content. Otherwise a rapid PATCH burst caught
            // mid-pipeline would leave `mirrored` set to the latest
            // `updatedAt` while the local content still reflects an earlier
            // version, and the next poll would skip re-reconciliation.
            if doc.status == "done" {
                if !is_binary_type {
                    if let Some(content) = doc.content.as_deref() {
                        self.rewrite_file_content(existing_ino, content)?;
                    }
                }
                self.db.set_mirrored_state(
                    existing_ino,
                    updated_ms,
                    Some(&doc.status),
                    Some(now_ms()),
                );
                self.sync_transcription_sibling(doc, filepath);
            } else {
                self.db
                    .set_mirrored_state(existing_ino, None, Some(&doc.status), Some(now_ms()));
                self.sync_error_sibling(doc, filepath);
            }
            return Ok(ReconcileOutcome::Updated);
        }

        // New doc (not yet mirrored locally) — create inode+dentry+chunks.
        // Skip in-progress docs so we don't materialize partial content.
        if doc.status != "done" {
            if doc.status == "failed" {
                self.sync_error_sibling(doc, filepath);
            }
            return Ok(ReconcileOutcome::DeferredProcessing);
        }

        let parent_ino = self.ensure_dirs(dir)?;

        // If something with this filename already exists (from a prior unmapped
        // pull), attach the remote_id to that inode rather than creating a dup.
        let existing_ino: Option<i64> = {
            let conn = self.db.conn.lock();
            conn.query_row(
                "SELECT ino FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                rusqlite::params![parent_ino as i64, filename],
                |r| r.get(0),
            )
            .ok()
        };
        if let Some(ino_i64) = existing_ino {
            let ino = ino_i64 as u64;
            self.db.set_remote_id(ino, &doc.id);
            if doc.status == "done" {
                if !is_binary_type {
                    if let Some(content) = doc.content.as_deref() {
                        self.rewrite_file_content(ino, content)?;
                    }
                }
                self.sync_transcription_sibling(doc, filepath);
            }
            self.db
                .set_mirrored_state(ino, updated_ms, Some(&doc.status), Some(now_ms()));
            return Ok(ReconcileOutcome::Attached);
        }

        if is_binary_type {
            self.sync_transcription_sibling(doc, filepath);
            self.sync_error_sibling(doc, filepath);
            if doc.url.is_some() {
                let ino = self.create_raw_stub(filepath, &doc.id)?;
                self.db.set_remote_id(ino, &doc.id);
                self.db
                    .set_mirrored_state(ino, updated_ms, Some(&doc.status), Some(now_ms()));
                return Ok(ReconcileOutcome::NeedsRehydrate);
            }
            return Ok(ReconcileOutcome::DeferredProcessing);
        }

        let content = doc.content.as_deref().unwrap_or("");
        let size = content.len() as i64;
        let now = Timestamp::now();

        let file_ino = {
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
            let ino = conn.last_insert_rowid() as u64;

            conn.execute(
                "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?1, ?2, ?3)",
                rusqlite::params![filename, parent_ino as i64, ino as i64],
            )
            .map_err(sql_err)?;

            if !content.is_empty() {
                let chunk_size = self.db.chunk_size;
                let bytes = content.as_bytes();
                for (i, chunk_data) in bytes.chunks(chunk_size).enumerate() {
                    conn.execute(
                        "INSERT OR REPLACE INTO fs_data (ino, chunk_index, data) VALUES (?1, ?2, ?3)",
                        rusqlite::params![ino as i64, i as i64, chunk_data],
                    )
                    .map_err(sql_err)?;
                }
            }
            ino
        };

        self.dentry_cache
            .lock()
            .put((parent_ino, filename.to_string()), file_ino);
        self.db.set_remote_id(file_ino, &doc.id);
        self.db
            .set_mirrored_state(file_ino, updated_ms, Some(&doc.status), Some(now_ms()));
        self.sync_transcription_sibling(doc, filepath);

        Ok(ReconcileOutcome::Created)
    }

    fn remove_derived_sibling(&self, parent_ino: u64, name: &str) {
        let conn = self.db.conn.lock();
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT d.ino, i.derived
                   FROM fs_dentry d JOIN fs_inode i ON i.ino = d.ino
                  WHERE d.parent_ino = ?1 AND d.name = ?2",
                rusqlite::params![parent_ino as i64, name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let Some((ino, derived)) = row else {
            return;
        };
        if derived == 0 {
            return;
        }
        let _ = conn.execute(
            "DELETE FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
            rusqlite::params![parent_ino as i64, name],
        );
        let _ = conn.execute("DELETE FROM fs_data WHERE ino = ?1", [ino]);
        let _ = conn.execute("DELETE FROM fs_inode WHERE ino = ?1", [ino]);
        drop(conn);
        self.dentry_cache
            .lock()
            .pop(&(parent_ino, name.to_string()));
    }

    fn cascade_unlink_derived_siblings(&self, parent_ino: u64, name: &str) {
        for suffix in DERIVED_SIBLING_SUFFIXES {
            let sibling_name = format!("{}{}", name, suffix);
            self.remove_derived_sibling(parent_ino, &sibling_name);
        }
    }

    fn rename_derived_sibling(
        &self,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
    ) {
        let conn = self.db.conn.lock();
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT d.ino, i.derived
                   FROM fs_dentry d JOIN fs_inode i ON i.ino = d.ino
                  WHERE d.parent_ino = ?1 AND d.name = ?2",
                rusqlite::params![old_parent as i64, old_name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let Some((ino, derived)) = row else {
            return;
        };
        if derived == 0 {
            return;
        }
        let _ = conn.execute(
            "DELETE FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
            rusqlite::params![new_parent as i64, new_name],
        );
        let _ = conn.execute(
            "UPDATE fs_dentry SET parent_ino = ?1, name = ?2 WHERE ino = ?3",
            rusqlite::params![new_parent as i64, new_name, ino],
        );
        drop(conn);
        let mut cache = self.dentry_cache.lock();
        cache.pop(&(old_parent, old_name.to_string()));
        cache.put((new_parent, new_name.to_string()), ino as u64);
    }

    fn cascade_rename_derived_siblings(
        &self,
        old_parent: u64,
        old_name: &str,
        new_parent: u64,
        new_name: &str,
    ) {
        for suffix in DERIVED_SIBLING_SUFFIXES {
            let old = format!("{}{}", old_name, suffix);
            let new = format!("{}{}", new_name, suffix);
            self.rename_derived_sibling(old_parent, &old, new_parent, &new);
        }
    }

    /// If the doc is a non-text type that has extracted content, materialize
    /// a read-only `.<type>-transcription.md` sibling next to the raw file.
    fn sync_error_sibling(&self, doc: &crate::api::Document, filepath: &str) {
        if doc.status != "failed" {
            return;
        }
        let reason = format!(
            "smfs: server-side extraction failed for this file.\n\n\
             type: {}\n\
             doc id: {}\n\n\
             The file is stored on the server but could not be processed.\n\
             To retry, delete this error file and re-copy the source.\n",
            doc.type_.as_deref().unwrap_or("<unknown>"),
            doc.id
        );
        let sibling = format!("{}.smfs-error.txt", filepath);
        if let Err(e) = self.create_derived_sibling(&sibling, &reason) {
            tracing::warn!(filepath, sibling, error = %e, "error sibling creation failed");
        }
    }

    fn sync_transcription_sibling(&self, doc: &crate::api::Document, filepath: &str) {
        if doc.status != "done" {
            return;
        }
        let content = match doc.content.as_deref() {
            Some(c) if !c.is_empty() => c,
            _ => return,
        };
        let suffix = match doc.type_.as_deref() {
            Some("image") => ".image-transcription.md",
            Some("pdf") => ".pdf-transcription.md",
            Some("video") => ".video-transcription.md",
            Some("audio") => ".audio-transcription.md",
            Some("webpage") => ".webpage-transcription.md",
            _ => return,
        };
        let sibling_path = format!("{}{}", filepath, suffix);
        if let Err(e) = self.create_derived_sibling(&sibling_path, content) {
            tracing::warn!(filepath, sibling = %sibling_path, error = %e, "transcription sibling creation failed");
        }
    }

    /// Rename an inode's dentry to a new (dir, filename), creating intermediate
    /// directories if needed.
    fn apply_rename_to(&self, ino: u64, new_dir: &str, new_name: &str) -> VfsResult<()> {
        let new_parent = self.ensure_dirs(new_dir)?;
        let conn = self.db.conn.lock();
        let (old_parent, old_name): (i64, String) = conn
            .query_row(
                "SELECT parent_ino, name FROM fs_dentry WHERE ino = ?1",
                [ino as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql_err)?;
        conn.execute(
            "UPDATE fs_dentry SET parent_ino = ?1, name = ?2 WHERE ino = ?3",
            rusqlite::params![new_parent as i64, new_name, ino as i64],
        )
        .map_err(sql_err)?;
        drop(conn);
        let mut cache = self.dentry_cache.lock();
        cache.pop(&(old_parent as u64, old_name));
        cache.put((new_parent, new_name.to_string()), ino);
        Ok(())
    }

    /// Rewrite a file's content by replacing all chunks with the new bytes.
    fn rewrite_file_content(&self, ino: u64, content: &str) -> VfsResult<()> {
        let chunk_size = self.db.chunk_size;
        let bytes = content.as_bytes();
        let size = bytes.len() as i64;
        let now = Timestamp::now();

        let conn = self.db.conn.lock();
        conn.execute("DELETE FROM fs_data WHERE ino = ?1", [ino as i64])
            .map_err(sql_err)?;
        if !bytes.is_empty() {
            for (i, chunk_data) in bytes.chunks(chunk_size).enumerate() {
                conn.execute(
                    "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?1, ?2, ?3)",
                    rusqlite::params![ino as i64, i as i64, chunk_data],
                )
                .map_err(sql_err)?;
            }
        }
        conn.execute(
            "UPDATE fs_inode SET size = ?2, mtime = ?3, mtime_nsec = ?4 WHERE ino = ?1",
            rusqlite::params![ino as i64, size, now.sec, now.nsec as i64],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    pub(crate) fn create_derived_sibling(&self, filepath: &str, content: &str) -> VfsResult<u64> {
        let (dir, filename) = match filepath.rfind('/') {
            Some(pos) => {
                let d = if pos == 0 { "/" } else { &filepath[..pos] };
                (d.to_string(), filepath[pos + 1..].to_string())
            }
            None => return Err(VfsError::InvalidPath("derived sibling needs path".into())),
        };
        if filename.is_empty() {
            return Err(VfsError::InvalidPath(
                "derived sibling needs filename".into(),
            ));
        }
        let parent_ino = self.ensure_dirs(&dir)?;
        let bytes = content.as_bytes();
        let size = bytes.len() as i64;
        let chunk_size = self.db.chunk_size;
        let now = Timestamp::now();

        let conn = self.db.conn.lock();
        let existing: Option<(i64, i64)> = conn
            .query_row(
                "SELECT d.ino, i.derived
                   FROM fs_dentry d JOIN fs_inode i ON i.ino = d.ino
                  WHERE d.parent_ino = ?1 AND d.name = ?2",
                rusqlite::params![parent_ino as i64, filename],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();

        let ino = if let Some((existing_ino, derived)) = existing {
            if derived == 0 {
                return Err(VfsError::AlreadyExists);
            }
            conn.execute("DELETE FROM fs_data WHERE ino = ?1", [existing_ino])
                .map_err(sql_err)?;
            for (i, chunk_data) in bytes.chunks(chunk_size).enumerate() {
                conn.execute(
                    "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?1, ?2, ?3)",
                    rusqlite::params![existing_ino, i as i64, chunk_data],
                )
                .map_err(sql_err)?;
            }
            conn.execute(
                "UPDATE fs_inode SET size = ?2, mtime = ?3, mtime_nsec = ?4 WHERE ino = ?1",
                rusqlite::params![existing_ino, size, now.sec, now.nsec as i64],
            )
            .map_err(sql_err)?;
            existing_ino as u64
        } else {
            conn.execute(
                "INSERT INTO fs_inode (mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec, derived)
                 VALUES (?1, 1, 0, 0, ?2, ?3, ?3, ?3, 0, ?4, ?4, ?4, 1)",
                rusqlite::params![
                    (S_IFREG | 0o444) as i64,
                    size,
                    now.sec,
                    now.nsec as i64,
                ],
            )
            .map_err(sql_err)?;
            let new_ino = conn.last_insert_rowid() as u64;
            conn.execute(
                "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?1, ?2, ?3)",
                rusqlite::params![filename, parent_ino as i64, new_ino as i64],
            )
            .map_err(sql_err)?;
            for (i, chunk_data) in bytes.chunks(chunk_size).enumerate() {
                conn.execute(
                    "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?1, ?2, ?3)",
                    rusqlite::params![new_ino as i64, i as i64, chunk_data],
                )
                .map_err(sql_err)?;
            }
            new_ino
        };
        drop(conn);

        self.dentry_cache.lock().put((parent_ino, filename), ino);
        Ok(ino)
    }

    pub(crate) fn create_raw_stub(&self, filepath: &str, _remote_id: &str) -> VfsResult<u64> {
        let (dir, filename) = match filepath.rfind('/') {
            Some(pos) => {
                let d = if pos == 0 { "/" } else { &filepath[..pos] };
                (d.to_string(), filepath[pos + 1..].to_string())
            }
            None => return Err(VfsError::InvalidPath("stub needs slashed path".into())),
        };
        if filename.is_empty() {
            return Err(VfsError::InvalidPath("stub needs filename".into()));
        }
        let parent_ino = self.ensure_dirs(&dir)?;
        let now = Timestamp::now();

        let conn = self.db.conn.lock();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT ino FROM fs_dentry WHERE parent_ino = ?1 AND name = ?2",
                rusqlite::params![parent_ino as i64, filename],
                |r| r.get(0),
            )
            .ok();
        if let Some(ino) = existing {
            return Ok(ino as u64);
        }
        conn.execute(
            "INSERT INTO fs_inode (mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec)
             VALUES (?1, 1, 0, 0, 0, ?2, ?2, ?2, 0, ?3, ?3, ?3)",
            rusqlite::params![
                (S_IFREG | 0o644) as i64,
                now.sec,
                now.nsec as i64,
            ],
        )
        .map_err(sql_err)?;
        let ino = conn.last_insert_rowid() as u64;
        conn.execute(
            "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?1, ?2, ?3)",
            rusqlite::params![filename, parent_ino as i64, ino as i64],
        )
        .map_err(sql_err)?;
        drop(conn);
        self.dentry_cache.lock().put((parent_ino, filename), ino);
        Ok(ino)
    }

    pub fn rehydrate_raw_bytes(&self, ino: u64, bytes: &[u8]) -> VfsResult<()> {
        let chunk_size = self.db.chunk_size;
        let size = bytes.len() as i64;
        let now = Timestamp::now();

        let conn = self.db.conn.lock();
        conn.execute("DELETE FROM fs_data WHERE ino = ?1", [ino as i64])
            .map_err(sql_err)?;
        for (i, chunk_data) in bytes.chunks(chunk_size).enumerate() {
            conn.execute(
                "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![ino as i64, i as i64, chunk_data],
            )
            .map_err(sql_err)?;
        }
        conn.execute(
            "UPDATE fs_inode SET size = ?2, mtime = ?3, mtime_nsec = ?4 WHERE ino = ?1",
            rusqlite::params![ino as i64, size, now.sec, now.nsec as i64],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    pub fn ino_for_remote_id(&self, remote_id: &str) -> Option<u64> {
        self.db.ino_by_remote_id(remote_id)
    }

    /// Remove the local copy of a document whose remote_id disappeared
    /// from the server. No-op if the remote_id isn't mapped or the inode is
    /// locally dirty.
    pub(crate) fn apply_deletion(&self, remote_id: &str) -> VfsResult<bool> {
        let Some(ino) = self.db.ino_by_remote_id(remote_id) else {
            return Ok(false);
        };
        if self.db.get_dirty_since(ino).is_some() {
            return Ok(false);
        }

        let conn = self.db.conn.lock();
        let parent_row: Option<(i64, String)> = conn
            .query_row(
                "SELECT parent_ino, name FROM fs_dentry WHERE ino = ?1",
                [ino as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let (parent_ino, name) = match parent_row {
            Some(p) => p,
            None => return Ok(false),
        };

        conn.execute("DELETE FROM fs_dentry WHERE ino = ?1", [ino as i64])
            .map_err(sql_err)?;
        conn.execute("DELETE FROM fs_data WHERE ino = ?1", [ino as i64])
            .map_err(sql_err)?;
        conn.execute("DELETE FROM fs_inode WHERE ino = ?1", [ino as i64])
            .map_err(sql_err)?;
        conn.execute("DELETE FROM fs_remote WHERE ino = ?1", [ino as i64])
            .map_err(sql_err)?;
        drop(conn);
        self.dentry_cache.lock().pop(&(parent_ino as u64, name));
        Ok(true)
    }

    /// Access the `Db` handle (for sync engine).
    pub(crate) fn db(&self) -> &Arc<Db> {
        &self.db
    }

    /// Access the `ApiClient` handle, if one is configured.
    pub(crate) fn api(&self) -> Option<&Arc<crate::api::ApiClient>> {
        self.api.as_ref()
    }

    /// Number of rows currently in the push queue (drives unmount drain).
    pub fn push_queue_len(&self) -> usize {
        self.db.push_queue_len()
    }

    /// Snapshot of the push_queue row for a given filepath (for tests /
    /// diagnostics only).
    pub fn push_queue_inspect(&self, filepath: &str) -> Option<PushQueueSnapshot> {
        let conn = self.db.conn.lock();
        conn.query_row(
            "SELECT op, inflight_started_at, pending_op, remote_id
               FROM push_queue WHERE filepath = ?1",
            [filepath],
            |r| {
                Ok(PushQueueSnapshot {
                    op: r.get::<_, String>(0)?,
                    inflight: r.get::<_, Option<i64>>(1)?.is_some(),
                    pending_op: r.get::<_, Option<String>>(2)?,
                    remote_id: r.get::<_, Option<String>>(3)?,
                })
            },
        )
        .ok()
    }

    /// Read the `dirty_since` watermark for an inode (for tests / diagnostics).
    pub fn dirty_since_of(&self, ino: u64) -> Option<i64> {
        self.db.get_dirty_since(ino)
    }

    /// Run one pass of the delta pull synchronously (for tests to trigger a
    /// pull without waiting on the 30s loop).
    pub async fn pull_once(self: &Arc<Self>) -> anyhow::Result<usize> {
        crate::sync::pull::delta_pull(self).await
    }

    /// Import a host file into the VFS. Returns `Ok(false)` if already exists.
    pub async fn import_file(&self, filepath: &str, contents: &[u8]) -> Result<bool, String> {
        let pos = filepath.rfind('/').ok_or("filepath must contain '/'")?;
        let dir = if pos == 0 { "/" } else { &filepath[..pos] };
        let name = &filepath[pos + 1..];
        if name.is_empty() {
            return Err("filepath must not end with '/'".into());
        }

        let parent_ino = self.ensure_dirs(dir).map_err(|e| e.to_string())?;

        let (_, handle) = match self.create_file(parent_ino, name, 0o644, 0, 0).await {
            Ok(v) => v,
            Err(crate::vfs::VfsError::AlreadyExists) => return Ok(false),
            Err(e) => return Err(e.to_string()),
        };

        if !contents.is_empty() {
            handle.write(0, contents).await.map_err(|e| e.to_string())?;
        }
        handle.flush().await.map_err(|e| e.to_string())?;
        Ok(true)
    }
}

/// Snapshot of a push_queue row returned by [`SupermemoryFs::push_queue_inspect`].
#[derive(Debug, Clone)]
pub struct PushQueueSnapshot {
    pub op: String,
    pub inflight: bool,
    pub pending_op: Option<String>,
    pub remote_id: Option<String>,
}

/// Epoch-ms parsing for ISO-8601 timestamps returned by the API.
pub(crate) fn parse_iso_to_ms(iso: &str) -> Option<i64> {
    // Minimal RFC3339 parser sufficient for API timestamps
    // (e.g. "2026-04-18T06:55:52.356Z"). Returns epoch-ms.
    let t = iso.find('T')?;
    let date = &iso[..t];
    let rest = &iso[t + 1..];
    let (time, _tz) = rest.split_once(['Z', '+', '-'])?;
    let (y, m, d): (i64, i64, i64) = {
        let mut it = date.split('-');
        let y = it.next()?.parse().ok()?;
        let m = it.next()?.parse().ok()?;
        let d = it.next()?.parse().ok()?;
        (y, m, d)
    };
    let (h, mi, s_part): (i64, i64, &str) = {
        let mut it = time.split(':');
        let h = it.next()?.parse().ok()?;
        let mi = it.next()?.parse().ok()?;
        let s = it.next()?;
        (h, mi, s)
    };
    let (sec, ms) = match s_part.split_once('.') {
        Some((s, frac)) => {
            let sec: i64 = s.parse().ok()?;
            let mut fracs = frac
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect::<String>();
            fracs.truncate(3);
            while fracs.len() < 3 {
                fracs.push('0');
            }
            let ms: i64 = fracs.parse().ok()?;
            (sec, ms)
        }
        None => (s_part.parse().ok()?, 0),
    };
    // Days since 1970-01-01 using civil-from-days algorithm (Howard Hinnant).
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let total_s = days * 86_400 + h * 3_600 + mi * 60 + sec;
    Some(total_s * 1_000 + ms)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Outcome of reconciling one remote document against the local cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// New doc created locally.
    Created,
    /// Existing local inode had no remote_id; we attached this one to it
    /// (recovers from the pre-M7 pull_documents bug).
    Attached,
    /// Local copy updated (content rewritten and/or renamed).
    Updated,
    /// Remote version matches what we already have.
    Unchanged,
    /// Local write is newer than this remote version; respect it.
    SkippedDirty,
    /// Remote doc is still being processed; wait for next poll to bring the
    /// final version.
    DeferredProcessing,
    /// Pure-pull of a binary doc: we created a 0-byte stub inode; caller
    /// should fetch `doc.url` and populate chunks.
    NeedsRehydrate,
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
        if parent_ino == ROOT_INO && name == PROFILE_NAME {
            if let Some(pf) = &self.profile_file {
                return Ok(Some(pf.profile_attr()));
            }
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
        if ino == PROFILE_INO {
            if let Some(pf) = &self.profile_file {
                return Ok(Some(pf.profile_attr()));
            }
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
            if ino == ROOT_INO && !entries.iter().any(|e| e.name == PROFILE_NAME) {
                if let Some(pf) = &self.profile_file {
                    entries.push(DirEntry {
                        name: PROFILE_NAME.to_string(),
                        attr: pf.profile_attr(),
                    });
                }
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
            if let Some(pf) = &self.profile_file {
                return Ok(pf.clone());
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

        // Push delete to API via the push queue (durable, coalescing).
        if self.api.is_some() {
            if let Some(fp) = filepath_for_api {
                let remote_id = self.db.get_remote_id(child_ino as u64);
                self.db.push_queue_upsert(
                    &fp,
                    super::db::PushOp::Delete,
                    None,
                    None,
                    remote_id.as_deref(),
                    now_ms(),
                );
                tracing::debug!(filepath = %fp, "enqueued push (delete)");
            }
        }

        self.cascade_unlink_derived_siblings(parent_ino, name);

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
        let (src_ino, did_overwrite, dst_remote_id) = {
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
            let mut dst_remote_id: Option<String> = None;

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
                // Capture before the DELETE so we can rebind a pending
                // Create onto this remote_id instead of orphaning it.
                dst_remote_id = tx
                    .query_row(
                        "SELECT remote_id FROM fs_remote WHERE ino = ?1",
                        [dst_ino],
                        |r| r.get::<_, String>(0),
                    )
                    .ok();
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

            (src_ino, did_overwrite, dst_remote_id)
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

        // Atomic-save editors do `write(tmp) + rename(tmp, final)` inside
        // the push debounce window; a Rename upsert coalesces over the
        // pending Create and drops `content_ino`. Retarget the Create
        // row instead so its content survives the rename.
        if self.api.is_some() {
            if let (Some(old_fp), Some(new_fp)) = (old_filepath.as_deref(), new_filepath.as_deref())
            {
                let retargeted = self.db.push_queue_retarget_pending_create(
                    old_fp,
                    new_fp,
                    dst_remote_id.as_deref(),
                    now_ms(),
                );

                if retargeted {
                    if let Some(rid) = dst_remote_id.as_deref() {
                        self.db.set_remote_id(src_ino as u64, rid);
                    }
                    tracing::debug!(
                        old = %old_fp,
                        new = %new_fp,
                        rebind = ?dst_remote_id,
                        "retargeted pending create across rename"
                    );
                } else {
                    if did_overwrite {
                        if let Some(dst_fp) = dst_filepath_for_delete.as_deref() {
                            self.db.push_queue_upsert(
                                dst_fp,
                                super::db::PushOp::Delete,
                                None,
                                None,
                                None,
                                now_ms(),
                            );
                            tracing::debug!(filepath = %dst_fp, "enqueued push (rename overwrote)");
                        }
                    }
                    let remote_id = self.db.get_remote_id(src_ino as u64);
                    self.db.push_queue_upsert(
                        old_fp,
                        super::db::PushOp::Rename,
                        None,
                        Some(new_fp),
                        remote_id.as_deref(),
                        now_ms(),
                    );
                    tracing::debug!(old = %old_fp, new = %new_fp, "enqueued push (rename)");
                }
            }
        }

        self.cascade_rename_derived_siblings(old_parent_ino, old_name, new_parent_ino, new_name);

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
