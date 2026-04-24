//! Deletion reconciliation.

use std::collections::HashSet;
use std::sync::Arc;

use crate::api::ListDocumentsReq;
use crate::cache::SupermemoryFs;

const PAGE_SIZE: u32 = 100;

/// Run one deletion-scan pass. Returns `Ok(removed)` where `removed` is the
/// number of local inodes that were unlinked because their remote_id
/// disappeared from the server.
pub async fn deletion_scan(fs: &Arc<SupermemoryFs>) -> anyhow::Result<usize> {
    let Some(api) = fs.api() else {
        return Ok(0);
    };

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

    Ok(removed)
}
