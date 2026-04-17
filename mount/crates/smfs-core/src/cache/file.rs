//! SQLite-backed file handle implementing [`crate::vfs::File`].

use std::sync::Arc;

use async_trait::async_trait;

use super::db::Db;
use crate::vfs::{FileAttr, Timestamp, VfsError, VfsResult};

/// A handle to an open file backed by chunked SQLite storage.
///
/// Each read/write operates directly on `fs_data` chunks. The handle
/// stores only the inode number — all state lives in the database.
#[derive(Debug)]
pub struct SqliteFile {
    pub(crate) db: Arc<Db>,
    pub(crate) ino: u64,
    pub(crate) api: Option<Arc<crate::api::ApiClient>>,
    pub(crate) filepath: Option<String>,
}

#[async_trait]
impl crate::vfs::File for SqliteFile {
    async fn read(&self, offset: u64, size: usize) -> VfsResult<Vec<u8>> {
        let conn = self.db.conn.lock();
        let chunk_size = self.db.chunk_size as u64;

        // Get file size.
        let file_size: i64 = conn
            .query_row(
                "SELECT size FROM fs_inode WHERE ino = ?1",
                [self.ino as i64],
                |r| r.get(0),
            )
            .map_err(|_| VfsError::NotFound)?;
        let file_size = file_size as u64;

        if offset >= file_size {
            return Ok(Vec::new());
        }

        let read_size = size.min((file_size - offset) as usize);
        let start_chunk = offset / chunk_size;
        let end_chunk = (offset + read_size as u64).saturating_sub(1) / chunk_size;

        let mut stmt = conn
            .prepare_cached(
                "SELECT chunk_index, data FROM fs_data
                 WHERE ino = ?1 AND chunk_index >= ?2 AND chunk_index <= ?3
                 ORDER BY chunk_index",
            )
            .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;

        let mut rows = stmt
            .query_map(
                rusqlite::params![self.ino as i64, start_chunk as i64, end_chunk as i64],
                |row| {
                    let idx: i64 = row.get(0)?;
                    let data: Vec<u8> = row.get(1)?;
                    Ok((idx as u64, data))
                },
            )
            .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;

        let mut result = Vec::with_capacity(read_size);
        let start_offset_in_chunk = (offset % chunk_size) as usize;
        let mut next_expected = start_chunk;

        while let Some(Ok((chunk_idx, chunk_data))) = rows.next() {
            // Fill gaps with zeros for sparse regions.
            while next_expected < chunk_idx && result.len() < read_size {
                let skip = if next_expected == start_chunk {
                    start_offset_in_chunk
                } else {
                    0
                };
                let zeros = (chunk_size as usize - skip).min(read_size - result.len());
                result.resize(result.len() + zeros, 0);
                next_expected += 1;
            }

            if result.len() >= read_size {
                break;
            }

            let skip = if chunk_idx == start_chunk {
                start_offset_in_chunk
            } else {
                0
            };
            let available = chunk_data.len().saturating_sub(skip);
            let take = available.min(read_size - result.len());
            result.extend_from_slice(&chunk_data[skip..skip + take]);
            next_expected = chunk_idx + 1;
        }

        // Fill trailing sparse region.
        if result.len() < read_size {
            result.resize(read_size, 0);
        }

        Ok(result)
    }

    async fn write(&self, offset: u64, data: &[u8]) -> VfsResult<u32> {
        if data.is_empty() {
            return Ok(0);
        }

        let conn = self.db.conn.lock();
        let chunk_size = self.db.chunk_size;

        let tx = conn
            .unchecked_transaction()
            .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;

        // Get current size.
        let current_size: i64 = tx
            .query_row(
                "SELECT size FROM fs_inode WHERE ino = ?1",
                [self.ino as i64],
                |r| r.get(0),
            )
            .map_err(|_| VfsError::NotFound)?;

        // Write chunks.
        let mut written = 0usize;
        while written < data.len() {
            let current_offset = offset + written as u64;
            let chunk_idx = (current_offset / chunk_size as u64) as i64;
            let offset_in_chunk = (current_offset % chunk_size as u64) as usize;

            let remaining_in_chunk = chunk_size - offset_in_chunk;
            let to_write = remaining_in_chunk.min(data.len() - written);

            let chunk_data = if to_write != chunk_size {
                // Partial write: read-modify-write.
                let existing: Option<Vec<u8>> = tx
                    .query_row(
                        "SELECT data FROM fs_data WHERE ino = ?1 AND chunk_index = ?2",
                        rusqlite::params![self.ino as i64, chunk_idx],
                        |r| r.get(0),
                    )
                    .ok();

                let mut buf = existing.unwrap_or_default();
                if buf.len() < offset_in_chunk + to_write {
                    buf.resize(offset_in_chunk + to_write, 0);
                }
                buf[offset_in_chunk..offset_in_chunk + to_write]
                    .copy_from_slice(&data[written..written + to_write]);
                buf
            } else {
                // Full chunk write.
                data[written..written + to_write].to_vec()
            };

            tx.execute(
                "INSERT OR REPLACE INTO fs_data (ino, chunk_index, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![self.ino as i64, chunk_idx, chunk_data],
            )
            .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;

            written += to_write;
        }

        // Update size and mtime.
        let new_size = (current_size as u64).max(offset + data.len() as u64);
        let now = Timestamp::now();
        tx.execute(
            "UPDATE fs_inode SET size = ?1, mtime = ?2, mtime_nsec = ?3, ctime = ?4, ctime_nsec = ?5 WHERE ino = ?6",
            rusqlite::params![
                new_size as i64,
                now.sec,
                now.nsec,
                now.sec,
                now.nsec,
                self.ino as i64
            ],
        )
        .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;

        tx.commit()
            .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;

        Ok(data.len() as u32)
    }

    async fn truncate(&self, size: u64) -> VfsResult<()> {
        let conn = self.db.conn.lock();
        let chunk_size = self.db.chunk_size as u64;

        let tx = conn
            .unchecked_transaction()
            .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;

        if size == 0 {
            // Fast path: delete all chunks.
            tx.execute("DELETE FROM fs_data WHERE ino = ?1", [self.ino as i64])
                .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;
        } else {
            // Delete chunks past the new size.
            let last_chunk = (size.saturating_sub(1)) / chunk_size;
            tx.execute(
                "DELETE FROM fs_data WHERE ino = ?1 AND chunk_index > ?2",
                rusqlite::params![self.ino as i64, last_chunk as i64],
            )
            .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;

            // Trim last chunk if needed.
            let remainder = (size % chunk_size) as usize;
            if remainder > 0 {
                let existing: Option<Vec<u8>> = tx
                    .query_row(
                        "SELECT data FROM fs_data WHERE ino = ?1 AND chunk_index = ?2",
                        rusqlite::params![self.ino as i64, last_chunk as i64],
                        |r| r.get(0),
                    )
                    .ok();
                if let Some(mut chunk_data) = existing {
                    if chunk_data.len() > remainder {
                        chunk_data.truncate(remainder);
                        tx.execute(
                            "INSERT OR REPLACE INTO fs_data (ino, chunk_index, data) VALUES (?1, ?2, ?3)",
                            rusqlite::params![self.ino as i64, last_chunk as i64, chunk_data],
                        )
                        .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;
                    }
                }
            }
        }

        // Update inode size + timestamps.
        let now = Timestamp::now();
        tx.execute(
            "UPDATE fs_inode SET size = ?1, mtime = ?2, mtime_nsec = ?3, ctime = ?4, ctime_nsec = ?5 WHERE ino = ?6",
            rusqlite::params![size as i64, now.sec, now.nsec, now.sec, now.nsec, self.ino as i64],
        )
        .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;

        tx.commit()
            .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;

        Ok(())
    }

    async fn flush(&self) -> VfsResult<()> {
        // SQLite writes are already durable after each transaction commit.
        // If we have an API client, push the file content to the cloud.
        let Some(api) = &self.api else { return Ok(()) };
        let Some(filepath) = &self.filepath else {
            return Ok(());
        };

        let size: i64 = {
            let conn = self.db.conn.lock();
            conn.query_row(
                "SELECT size FROM fs_inode WHERE ino = ?1",
                [self.ino as i64],
                |r| r.get(0),
            )
            .unwrap_or(0)
        };

        if size == 0 {
            return Ok(());
        }

        let content = self.read(0, size as usize).await?;
        let text = String::from_utf8_lossy(&content).into_owned();

        // Check if this inode already has a remote document.
        let existing_remote_id = self.db.get_remote_id(self.ino);

        if let Some(remote_id) = existing_remote_id {
            // Existing document — PATCH to update content.
            let req = crate::api::UpdateDocumentReq {
                filepath: Some(filepath.clone()),
                content: Some(text.clone()),
            };
            match api.update_document(&remote_id, &req).await {
                Ok(()) => {
                    tracing::debug!(filepath, doc_id = %remote_id, "updated in API (PATCH)");
                }
                Err(crate::api::ApiError::NotFound) => {
                    // Remote doc was deleted externally. Clear stale mapping and re-create.
                    self.db.delete_remote_id(self.ino);
                    match api.create_document(&text, filepath).await {
                        Ok(resp) => {
                            self.db.set_remote_id(self.ino, &resp.id);
                            tracing::debug!(filepath, doc_id = %resp.id, "re-created in API after 404");
                        }
                        Err(e) => {
                            tracing::warn!(filepath, error = %e, "failed to re-create in API");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(filepath, doc_id = %remote_id, error = %e, "failed to update in API");
                }
            }
        } else {
            // New document — POST to create, then store the returned ID.
            match api.create_document(&text, filepath).await {
                Ok(resp) => {
                    self.db.set_remote_id(self.ino, &resp.id);
                    tracing::debug!(filepath, doc_id = %resp.id, "created in API (POST)");
                }
                Err(e) => {
                    tracing::warn!(filepath, error = %e, "failed to create in API");
                }
            }
        }

        Ok(())
    }

    async fn fsync(&self) -> VfsResult<()> {
        // Force a WAL checkpoint.
        let conn = self.db.conn.lock();
        conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)")
            .map_err(|e| VfsError::Io(std::io::Error::other(e.to_string())))?;
        Ok(())
    }

    async fn getattr(&self) -> VfsResult<FileAttr> {
        let conn = self.db.conn.lock();
        conn.query_row(
            "SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec
             FROM fs_inode WHERE ino = ?1",
            [self.ino as i64],
            Db::row_to_attr,
        )
        .map_err(|_| VfsError::NotFound)
    }
}
