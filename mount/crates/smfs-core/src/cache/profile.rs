//! Virtual read-only `profile.md` backed by the `POST /v4/profile` API.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::api::{ApiClient, ProfileResp};
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::mode::S_IFREG;
use crate::vfs::types::{FileAttr, Timestamp};

pub const PROFILE_INO: u64 = u64::MAX - 1;
pub const PROFILE_NAME: &str = "profile.md";

/// A virtual read-only file that shows the user's memory profile.
#[derive(Debug)]
pub struct ProfileFile {
    api: Arc<ApiClient>,
    cached: Mutex<Option<Vec<u8>>>,
}

impl ProfileFile {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self {
            api,
            cached: Mutex::new(None),
        }
    }

    async fn ensure_content(&self) -> VfsResult<Vec<u8>> {
        {
            let cached = self.cached.lock();
            if let Some(data) = cached.as_ref() {
                return Ok(data.clone());
            }
        }

        let content = match self.api.get_profile().await {
            Ok(resp) => format_profile(&resp),
            Err(e) => format!(
                "# Memory Profile\n\n(Failed to load profile: {})\n",
                e
            ),
        };

        let bytes = content.into_bytes();
        *self.cached.lock() = Some(bytes.clone());
        Ok(bytes)
    }

    pub fn profile_attr() -> FileAttr {
        let now = Timestamp::now();
        FileAttr {
            ino: PROFILE_INO,
            mode: S_IFREG | 0o444,
            nlink: 1,
            uid: 0,
            gid: 0,
            size: 65536, // placeholder — actual size determined on read
            blocks: 128,
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
        let content = self.ensure_content().await?;
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
        let content = self.ensure_content().await?;
        let mut attr = Self::profile_attr();
        attr.size = content.len() as u64;
        attr.blocks = attr.size.div_ceil(512);
        Ok(attr)
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
