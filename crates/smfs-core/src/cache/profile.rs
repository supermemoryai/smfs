//! Virtual read-only `profile.md` backed by the `POST /ns/{namespace}/profile` API.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;

use crate::api::{ApiClient, ProfileResp};
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::mode::S_IFREG;
use crate::vfs::types::{FileAttr, Timestamp};

pub const PROFILE_INO: u64 = u64::MAX - 1;
pub const PROFILE_NAME: &str = "profile.md";

#[derive(Debug)]
pub struct ProfileFile {
    api: Arc<ApiClient>,
    cache: RwLock<Option<Vec<u8>>>,
}

impl ProfileFile {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self {
            api,
            cache: RwLock::new(None),
        }
    }

    pub async fn warm(&self) {
        match self.api.get_profile().await {
            Ok(resp) => {
                *self.cache.write() = Some(format_profile(&resp).into_bytes());
            }
            Err(e) => {
                tracing::warn!(error = %e, "profile warm failed; profile.md will be empty until next mount");
            }
        }
    }

    pub fn profile_attr(&self) -> FileAttr {
        let now = Timestamp::now();
        let size = self
            .cache
            .read()
            .as_ref()
            .map(|v| v.len() as u64)
            .unwrap_or(0);
        FileAttr {
            ino: PROFILE_INO,
            mode: S_IFREG | 0o444,
            nlink: 1,
            uid: 0,
            gid: 0,
            size,
            blocks: size.div_ceil(512),
            atime: now,
            mtime: now,
            ctime: now,
            rdev: 0,
            blksize: 4096,
        }
    }
}

#[async_trait]
impl crate::vfs::traits::File for ProfileFile {
    async fn read(&self, offset: u64, size: usize) -> VfsResult<Vec<u8>> {
        let cache = self.cache.read();
        let Some(content) = cache.as_ref() else {
            return Ok(Vec::new());
        };
        let offset = offset as usize;
        if offset >= content.len() {
            return Ok(Vec::new());
        }
        let end = (offset + size).min(content.len());
        Ok(content[offset..end].to_vec())
    }

    async fn write(&self, _offset: u64, _data: &[u8]) -> VfsResult<u32> {
        Err(VfsError::PermissionDenied)
    }

    async fn truncate(&self, _size: u64) -> VfsResult<()> {
        Err(VfsError::PermissionDenied)
    }

    async fn flush(&self) -> VfsResult<()> {
        Ok(())
    }

    async fn fsync(&self) -> VfsResult<()> {
        Ok(())
    }

    async fn getattr(&self) -> VfsResult<FileAttr> {
        Ok(self.profile_attr())
    }
}

fn format_profile(resp: &ProfileResp) -> String {
    let mut out = String::new();
    out.push_str("# Memory Profile\n");
    out.push_str("# This file is auto-generated from your memories.\n");
    out.push_str("# It is not editable. To update, modify the source files\n");
    out.push_str("# that contain this information.\n\n");

    if let Some(statics) = &resp.profile.static_memories {
        if !statics.is_empty() {
            out.push_str("## Core Knowledge\n");
            for mem in statics {
                out.push_str(&format!("- {}\n", mem));
            }
            out.push('\n');
        }
    }

    if let Some(dynamics) = &resp.profile.dynamic {
        if !dynamics.is_empty() {
            out.push_str("## Recent Context\n");
            for mem in dynamics {
                out.push_str(&format!("- {}\n", mem));
            }
        }
    }

    if out.lines().count() <= 4 {
        out.push_str("(No memories yet. Write files to generate memories.)\n");
    }

    out
}
