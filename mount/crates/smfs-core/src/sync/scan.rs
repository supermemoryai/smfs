//! Deletion reconciliation.
//!
//! The API offers no soft-delete or "changes-since" endpoint, so catching
//! hard deletes requires a periodic full ID-set diff against the local
//! `fs_remote` table. We skip the scan when `total_items` hasn't changed
//! since the last scan (a cheap single-page probe) to avoid paginating the
//! full list when nothing can possibly have been deleted.

use std::collections::HashSet;
use std::sync::Arc;

use crate::api::ListDocumentsReq;
use crate::cache::SupermemoryFs;

const PAGE_SIZE: u32 = 100;
const SYNC_META_LAST_TOTAL: &str = "last_scan_total_items";

/// Run one deletion-scan pass. Returns `Ok(removed)` where `removed` is the
/// number of local inodes that were unlinked because their remote_id
/// disappeared from the server.
pub async fn deletion_scan(fs: &Arc<SupermemoryFs>) -> anyhow::Result<usize> {
    let Some(api) = fs.api() else {
        return Ok(0);
    };

    let probe = api
        .list_documents_page(&ListDocumentsReq {
            container_tags: vec![api.container_tag().to_string()],
            filepath: None,
            limit: 1,
            page: 1,
            include_content: Some(false),
            sort: None,
            order: None,
        })
        .await
        .map_err(|e| anyhow::anyhow!("probe list failed: {e}"))?;
    let total = probe.pagination.total_items;

    let last_total: Option<u32> = fs
        .db()
        .sync_meta_get(SYNC_META_LAST_TOTAL)
        .and_then(|s| s.parse().ok());
    if last_total == Some(total) {
        return Ok(0);
    }

    let mut remote_ids: HashSet<String> = HashSet::new();
    let mut page = 1u32;
    loop {
        let resp = api
            .list_documents_page(&ListDocumentsReq {
                container_tags: vec![api.container_tag().to_string()],
                filepath: None,
                limit: PAGE_SIZE,
                page,
                include_content: Some(false),
                sort: None,
                order: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!("deletion scan list failed: {e}"))?;

        if resp.memories.is_empty() {
            break;
        }
        for d in &resp.memories {
            remote_ids.insert(d.id.clone());
        }
        if page >= resp.pagination.total_pages {
            break;
        }
        page += 1;
    }

    let local_ids: Vec<String> = {
        let conn = fs.db().conn.lock();
        let mut stmt = conn
            .prepare("SELECT remote_id FROM fs_remote")
            .map_err(|e| anyhow::anyhow!(e))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| anyhow::anyhow!(e))?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let mut removed = 0usize;
    for id in local_ids {
        if !remote_ids.contains(&id) {
            if let Ok(true) = fs.apply_deletion(&id) {
                removed += 1;
            }
        }
    }

    fs.db().sync_meta_set(SYNC_META_LAST_TOTAL, &total.to_string());
    Ok(removed)
}
