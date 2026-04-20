//! Pull side of the sync engine.
//!
//! Walks `/v3/documents/list` sorted by `updatedAt desc`, paginating until we
//! reach a doc whose `updatedAt` is older than what we already know about.
//! Each returned doc is handed to [`SupermemoryFs::reconcile_one`] which
//! handles creation, rename, content rewrite, and dirty-priority skipping.

use std::sync::Arc;

use crate::api::{ApiClient, Document, ListDocumentsReq, ListDocumentsResp};
use crate::cache::ReconcileOutcome;
use crate::cache::SupermemoryFs;

const PAGE_SIZE: u32 = 100;
const SYNC_META_LAST_SEEN: &str = "last_seen_updated_at";
/// Per-file cap on R2 rehydration fetch (larger files stay 0-byte stubs).
const REHYDRATE_SIZE_CAP: u64 = 20 * 1024 * 1024;

/// Run one pass of the delta pull loop. Returns the number of remote docs
/// that were reconciled this pass (whether or not they produced local
/// changes).
pub async fn delta_pull(fs: &Arc<SupermemoryFs>) -> anyhow::Result<usize> {
    let Some(api) = fs.api() else {
        return Ok(0);
    };

    let last_seen = fs
        .db()
        .sync_meta_get(SYNC_META_LAST_SEEN)
        .unwrap_or_default();

    let mut newest_seen = last_seen.clone();
    let mut reconciled = 0usize;
    let mut page = 1u32;

    loop {
        let resp = list_page(api, page).await?;
        if resp.memories.is_empty() {
            break;
        }

        let mut hit_watermark = false;
        for doc in &resp.memories {
            if !last_seen.is_empty() && doc.updated_at.as_str() <= last_seen.as_str() {
                hit_watermark = true;
                break;
            }
            let outcome = fs.reconcile_one(doc);
            if matches!(outcome, Ok(ReconcileOutcome::NeedsRehydrate)) {
                rehydrate_if_possible(fs, doc).await;
            }
            reconciled += 1;
            if doc.updated_at > newest_seen {
                newest_seen = doc.updated_at.clone();
            }
        }

        if hit_watermark {
            break;
        }
        if page >= resp.pagination.total_pages {
            break;
        }
        page += 1;
    }

    if !newest_seen.is_empty() && newest_seen != last_seen {
        fs.db().sync_meta_set(SYNC_META_LAST_SEEN, &newest_seen);
    }

    Ok(reconciled)
}

async fn list_page(api: &ApiClient, page: u32) -> anyhow::Result<ListDocumentsResp> {
    api.list_documents_page(&ListDocumentsReq {
        container_tags: vec![api.container_tag().to_string()],
        filepath: None,
        limit: PAGE_SIZE,
        page,
        include_content: Some(true),
        sort: Some("updatedAt".to_string()),
        order: Some("desc".to_string()),
    })
    .await
    .map_err(|e| anyhow::anyhow!("list_documents failed: {e}"))
}

/// Full pull (no watermark). Used on mount when we have no prior state — we
/// want to catch every remote doc regardless of `updatedAt`.
pub async fn full_pull(fs: &Arc<SupermemoryFs>) -> anyhow::Result<usize> {
    let Some(api) = fs.api() else {
        return Ok(0);
    };

    let mut reconciled = 0usize;
    let mut page = 1u32;
    let mut newest_seen = String::new();

    loop {
        let resp = list_page(api, page).await?;
        if resp.memories.is_empty() {
            break;
        }
        for doc in &resp.memories {
            let outcome = fs.reconcile_one(doc);
            if matches!(outcome, Ok(ReconcileOutcome::NeedsRehydrate)) {
                rehydrate_if_possible(fs, doc).await;
            }
            reconciled += 1;
            if doc.updated_at > newest_seen {
                newest_seen = doc.updated_at.clone();
            }
        }
        if page >= resp.pagination.total_pages {
            break;
        }
        page += 1;
    }

    if !newest_seen.is_empty() {
        fs.db().sync_meta_set(SYNC_META_LAST_SEEN, &newest_seen);
    }

    Ok(reconciled)
}

async fn rehydrate_if_possible(fs: &Arc<SupermemoryFs>, doc: &Document) {
    let Some(url) = doc.url.as_deref() else {
        return;
    };
    let Some(ino) = fs.ino_for_remote_id(&doc.id) else {
        tracing::warn!(doc_id = %doc.id, "rehydrate: stub inode not found");
        return;
    };

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "rehydrate: http client build failed");
            return;
        }
    };
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(url, error = %e, "rehydrate: GET failed");
            return;
        }
    };
    if !resp.status().is_success() {
        tracing::warn!(url, status = %resp.status(), "rehydrate: GET non-2xx");
        return;
    }
    if let Some(len) = resp.content_length() {
        if len > REHYDRATE_SIZE_CAP {
            tracing::info!(
                url,
                size = len,
                cap = REHYDRATE_SIZE_CAP,
                "rehydrate: skipping oversized file; stub remains 0 bytes"
            );
            return;
        }
    }
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(url, error = %e, "rehydrate: body read failed");
            return;
        }
    };
    if let Err(e) = fs.rehydrate_raw_bytes(ino, &bytes) {
        tracing::warn!(url, error = %e, "rehydrate: local write failed");
        return;
    }
    tracing::info!(url, bytes = bytes.len(), "rehydrated raw file");
}
