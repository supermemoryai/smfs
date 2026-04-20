//! Push side of the sync engine.
//!
//! Two loops:
//!
//! - **Loop D — push worker.** Claims one queued job at a time from
//!   `push_queue`, sends the corresponding HTTP request, and either clears
//!   the row (success, possibly promoting a pending write) or bumps the
//!   attempt count with exponential backoff. Wakes on a `tokio::sync::Notify`
//!   signalled by every `push_queue_upsert`, or on a 200ms fallback poll.
//!
//! - **Loop E — inflight status poller.** For every doc we've POSTed whose
//!   `fs_remote.last_status` hasn't reached `done` yet, periodically
//!   `GET /v3/documents/:id` on an age-bucketed cadence. Updates
//!   `mirrored_updated_at` when status flips to `done` so the pull
//!   reconciler's watermark stays honest. Logs INFO/WARN/STOP tiers for
//!   stuck-processing detection.
//!
//! Together with the dirty_since flag set at write-time, these loops give
//! the mount a durable, coalescing, crash-safe write path: any rapid save
//! burst collapses to at-most-2 server requests per filepath (one inflight
//! plus one pending), retries survive `wrangler dev` restarts, and an
//! unmount drains the queue before releasing the mount.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;

use crate::api::{ApiError, UpdateDocumentReq};
use crate::cache::db::PushOp;
use crate::cache::SupermemoryFs;

/// Exponential backoff in milliseconds for the Nth failed attempt
/// (attempt=0 → first retry, already-failed once).
fn backoff_ms(attempt: i64) -> i64 {
    match attempt {
        0 => 500,
        1 => 1_000,
        2 => 2_000,
        3 => 5_000,
        4 => 15_000,
        5 => 30_000,
        _ => 60_000,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Block until the remote doc reaches `status=done` or the deadline passes.
///
/// The Supermemory server accepts POST and PATCH synchronously but processes
/// them asynchronously (extracting → chunking → embedding → indexing → done).
/// Issuing a second PATCH *while* the doc is still processing silently drops
/// the new content, so before we send a follow-up write on the same doc we
/// must wait for the previous one to finish.
async fn wait_until_done(api: &crate::api::ApiClient, remote_id: &str, max_wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + max_wait;
    loop {
        match api.get_document(remote_id).await {
            Ok(doc) if doc.status == "done" => return true,
            Ok(_) => {}
            Err(ApiError::NotFound) => return false,
            Err(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Max number of pushes we allow in flight concurrently per mount.
/// Bounds HTTP connection count and server load while still letting
/// independent files process in parallel.
const PUSH_CONCURRENCY: usize = 8;

/// Run loop D: the push worker. Claims queued jobs and spawns a
/// bounded fan-out of `process_job` tasks so unrelated filepaths
/// push concurrently. Same-file safety is guaranteed by the
/// `inflight_started_at` atomic update inside `push_queue_claim_next`.
pub async fn run_push_worker(fs: Arc<SupermemoryFs>, mut shutdown: watch::Receiver<bool>) {
    if fs.api().is_none() {
        return; // offline mount: nothing to push
    }
    let notify = fs.db().push_notify();
    let sem = Arc::new(tokio::sync::Semaphore::new(PUSH_CONCURRENCY));
    let mut set = tokio::task::JoinSet::new();

    'outer: loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break 'outer; }
            }
            _ = notify.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }

        // Reap any finished spawns so the JoinSet doesn't grow
        // unboundedly under a steady workload. Non-blocking.
        while let Ok(Some(_)) =
            tokio::time::timeout(Duration::from_millis(0), set.join_next()).await
        {}

        // Claim while permits + jobs are available. Break out when
        // either runs dry and wait for the next wake.
        loop {
            if *shutdown.borrow() {
                break 'outer;
            }
            let permit = match sem.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => break, // all permits held; rely on next wake
            };
            let Some(job) = fs.db().push_queue_claim_next(now_ms()) else {
                drop(permit);
                break; // no more eligible rows
            };
            let fs_clone = fs.clone();
            set.spawn(async move {
                let _permit = permit;
                process_job(&fs_clone, job).await;
            });
        }
    }

    // Graceful drain on shutdown: wait for any remaining spawned tasks.
    // daemon_runtime's drain_timeout already bounds overall unmount.
    while set.join_next().await.is_some() {}
}

/// Max time we'll block waiting for a remote doc's async pipeline to
/// reach `done` before we give up and move on. 30s is generous for
/// localhost (typical 2-4s) and still bounded enough for production.
const WAIT_DONE_MAX: Duration = Duration::from_secs(30);

fn poison_with_sibling(fs: &Arc<SupermemoryFs>, filepath: &str, status: u16, body: &str) {
    fs.db().push_queue_poison(filepath, status, body, now_ms());
    let reason = format!(
        "smfs: server rejected this file (HTTP {}).\n\nreason: {}\n\nTo retry, delete this error file and re-copy the source.\n",
        status, body
    );
    let sibling = format!("{}.smfs-error.txt", filepath);
    if let Err(e) = fs.create_derived_sibling(&sibling, &reason) {
        tracing::warn!(filepath, sibling, error = %e, "push: failed to write error sibling");
    } else {
        tracing::warn!(filepath, status, "push: poisoned; error sibling written");
    }
}

async fn process_job(fs: &Arc<SupermemoryFs>, job: crate::cache::db::PushJob) {
    let api = fs.api().expect("push worker requires api");
    let db = fs.db();

    match job.op {
        PushOp::Create | PushOp::Update => {
            let Some(ino) = job.content_ino else {
                tracing::warn!(filepath = %job.filepath, "push: create/update without content_ino; dropping row");
                db.push_queue_drop(&job.filepath);
                return;
            };
            let content = db.read_all_content(ino);

            // Non-UTF-8 → route through multipart; JSON content column
            // rejects null bytes.
            if std::str::from_utf8(&content).is_err() {
                process_binary_upload(fs, api, db.clone(), &job, ino, &content).await;
                return;
            }
            let text = std::str::from_utf8(&content)
                .expect("validated above")
                .to_string();

            if let Some(remote_id) = job.remote_id.as_deref() {
                // Before a PATCH, ensure the doc has finished any prior
                // processing — PATCHing a doc mid-pipeline silently drops
                // the content update.
                wait_until_done(api, remote_id, WAIT_DONE_MAX).await;

                let req = UpdateDocumentReq {
                    filepath: Some(job.filepath.clone()),
                    content: Some(text),
                    metadata: None,
                };
                match api.update_document(remote_id, &req).await {
                    Ok(()) => {
                        tracing::debug!(filepath = %job.filepath, remote_id, "push: PATCH ok");
                        // Block until the PATCH's reprocessing settles, so
                        // the next coalesced write sees a `done` doc.
                        wait_until_done(api, remote_id, WAIT_DONE_MAX).await;
                        db.set_mirrored_state(ino, None, Some("done"), Some(now_ms()));
                        db.set_dirty_since(ino, None);
                        db.push_queue_finalize_success(&job.filepath, now_ms());
                        db.push_notify().notify_one();
                    }
                    Err(ApiError::NotFound) => {
                        tracing::warn!(filepath = %job.filepath, remote_id, "push: PATCH 404; clearing remote_id and retrying as POST");
                        db.clear_remote_id_after_404(&job.filepath, remote_id);
                        db.push_queue_finalize_failure(
                            &job.filepath,
                            "patch_404_retry_create",
                            now_ms(),
                            0,
                        );
                        db.push_notify().notify_one();
                    }
                    Err(ApiError::Rejected { status, body }) => {
                        poison_with_sibling(fs, &job.filepath, status, &body);
                    }
                    Err(e) => {
                        let bo = backoff_ms(job.attempt);
                        tracing::warn!(filepath = %job.filepath, attempt = job.attempt, backoff_ms = bo, error = %e, "push: PATCH failed");
                        db.push_queue_finalize_failure(&job.filepath, &e.to_string(), now_ms(), bo);
                    }
                }
            } else {
                match api.create_document(&text, &job.filepath).await {
                    Ok(resp) => {
                        tracing::debug!(filepath = %job.filepath, remote_id = %resp.id, "push: POST ok");
                        db.set_remote_id(ino, &resp.id);
                        db.push_queue_set_remote_id(&job.filepath, &resp.id);
                        // Wait for this POST's pipeline to reach `done` so
                        // a coalesced follow-up PATCH lands cleanly.
                        wait_until_done(api, &resp.id, WAIT_DONE_MAX).await;
                        db.set_mirrored_state(ino, None, Some("done"), Some(now_ms()));
                        db.set_dirty_since(ino, None);
                        db.push_queue_finalize_success(&job.filepath, now_ms());
                        db.push_notify().notify_one();
                    }
                    Err(ApiError::Rejected { status, body }) => {
                        poison_with_sibling(fs, &job.filepath, status, &body);
                    }
                    Err(e) => {
                        let bo = backoff_ms(job.attempt);
                        tracing::warn!(filepath = %job.filepath, attempt = job.attempt, backoff_ms = bo, error = %e, "push: POST failed");
                        db.push_queue_finalize_failure(&job.filepath, &e.to_string(), now_ms(), bo);
                    }
                }
            }
        }

        PushOp::UploadBinary => {
            let Some(ino) = job.content_ino else {
                tracing::warn!(filepath = %job.filepath, "push: upload_binary without content_ino; dropping row");
                db.push_queue_drop(&job.filepath);
                return;
            };
            let content = db.read_all_content(ino);
            process_binary_upload(fs, api, db.clone(), &job, ino, &content).await;
        }

        PushOp::Delete => {
            let result = match job.remote_id.as_deref() {
                Some(rid) => api.delete_documents_by_ids(&[rid]).await,
                None => api.delete_documents(&job.filepath).await,
            };
            match result {
                Ok(r) => {
                    tracing::debug!(filepath = %job.filepath, remote_id = ?job.remote_id, deleted = r.deleted_count, "push: DELETE ok");
                    db.push_queue_finalize_success(&job.filepath, now_ms());
                    db.push_notify().notify_one();
                }
                Err(ApiError::Rejected { status, body }) => {
                    db.push_queue_poison(&job.filepath, status, &body, now_ms());
                    tracing::warn!(filepath = %job.filepath, status, "push: DELETE poisoned");
                }
                Err(e) => {
                    let bo = backoff_ms(job.attempt);
                    tracing::warn!(filepath = %job.filepath, attempt = job.attempt, backoff_ms = bo, error = %e, "push: DELETE failed");
                    db.push_queue_finalize_failure(&job.filepath, &e.to_string(), now_ms(), bo);
                }
            }
        }

        PushOp::Rename => {
            let Some(new_fp) = job.rename_to.clone() else {
                tracing::warn!(filepath = %job.filepath, "push: rename without rename_to; dropping row");
                db.push_queue_drop(&job.filepath);
                return;
            };
            let remote_id = match job.remote_id.clone() {
                Some(id) => Some(id),
                None => match api.list_documents(Some(&job.filepath)).await {
                    Ok(docs) => docs.into_iter().next().map(|d| d.id),
                    Err(e) => {
                        let bo = backoff_ms(job.attempt);
                        tracing::warn!(old = %job.filepath, error = %e, backoff_ms = bo, "push: rename lookup failed");
                        db.push_queue_finalize_failure(
                            &job.filepath,
                            &format!("rename lookup: {e}"),
                            now_ms(),
                            bo,
                        );
                        return;
                    }
                },
            };

            let Some(remote_id) = remote_id else {
                tracing::debug!(old = %job.filepath, new = %new_fp, "push: rename target has no remote doc; nothing to do");
                db.push_queue_finalize_success(&job.filepath, now_ms());
                db.push_notify().notify_one();
                return;
            };

            // Wait for any prior write on this doc to finish — same reason
            // as PATCH.
            wait_until_done(api, &remote_id, WAIT_DONE_MAX).await;

            let req = UpdateDocumentReq {
                filepath: Some(new_fp.clone()),
                content: None,
                metadata: None,
            };
            match api.update_document(&remote_id, &req).await {
                Ok(()) => {
                    tracing::debug!(old = %job.filepath, new = %new_fp, remote_id, "push: rename ok");
                    wait_until_done(api, &remote_id, WAIT_DONE_MAX).await;
                    db.push_queue_finalize_success(&job.filepath, now_ms());
                    db.push_notify().notify_one();
                }
                Err(ApiError::Rejected { status, body }) => {
                    db.push_queue_poison(&job.filepath, status, &body, now_ms());
                    tracing::warn!(filepath = %job.filepath, status, "push: rename poisoned");
                }
                Err(e) => {
                    let bo = backoff_ms(job.attempt);
                    tracing::warn!(old = %job.filepath, attempt = job.attempt, backoff_ms = bo, error = %e, "push: rename failed");
                    db.push_queue_finalize_failure(&job.filepath, &e.to_string(), now_ms(), bo);
                }
            }
        }
    }
}

async fn process_binary_upload(
    fs: &Arc<SupermemoryFs>,
    api: &crate::api::ApiClient,
    db: std::sync::Arc<crate::cache::db::Db>,
    job: &crate::cache::db::PushJob,
    ino: u64,
    content: &[u8],
) {
    let filename = job
        .filepath
        .rsplit('/')
        .next()
        .unwrap_or("file")
        .to_string();
    let mime = guess_mime(&filename);

    match api
        .create_document_multipart(content, &job.filepath, &mime, &filename)
        .await
    {
        Ok(resp) => {
            tracing::debug!(
                filepath = %job.filepath,
                remote_id = %resp.id,
                mime,
                size = content.len(),
                "push: multipart upload ok"
            );
            db.set_remote_id(ino, &resp.id);
            db.push_queue_set_remote_id(&job.filepath, &resp.id);
            wait_until_done(api, &resp.id, WAIT_DONE_MAX).await;
            db.set_mirrored_state(ino, None, Some("done"), Some(now_ms()));
            db.set_dirty_since(ino, None);
            db.push_queue_finalize_success(&job.filepath, now_ms());
            db.push_notify().notify_one();
        }
        Err(ApiError::Rejected { status, body }) => {
            poison_with_sibling(fs, &job.filepath, status, &body);
        }
        Err(e) => {
            let bo = backoff_ms(job.attempt);
            tracing::warn!(filepath = %job.filepath, attempt = job.attempt, backoff_ms = bo, error = %e, "push: multipart upload failed");
            db.push_queue_finalize_failure(&job.filepath, &e.to_string(), now_ms(), bo);
        }
    }
}

fn guess_mime(filename: &str) -> String {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "aiff" | "aif" => "audio/aiff",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "xml" => "application/xml",
        "json" => "application/json",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Stuck-detection tier based on how long the row has been awaiting `done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StuckTier {
    Ok,
    Info,
    Warn,
    Stop,
}

fn stuck_tier(age_ms: i64) -> StuckTier {
    if age_ms < 60_000 {
        StuckTier::Ok
    } else if age_ms < 300_000 {
        StuckTier::Info
    } else if age_ms < 3_600_000 {
        StuckTier::Warn
    } else {
        StuckTier::Stop
    }
}

/// Run loop E: the inflight status poller. One
/// `GET /v3/documents/processing` per tick — regardless of how many
/// docs are currently in flight — tells us the set of still-processing
/// ids. Anything local that's no longer in that set has reached a
/// terminal state; we issue a per-id GET only in that transition case.
pub async fn run_inflight_poller(fs: Arc<SupermemoryFs>, mut shutdown: watch::Receiver<bool>) {
    if fs.api().is_none() {
        return;
    }
    let api = fs.api().unwrap().clone();
    let db = fs.db().clone();
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }

        let rows = db.inodes_awaiting_done();
        if rows.is_empty() {
            continue;
        }

        let processing = match api.get_processing_documents().await {
            Ok(docs) => {
                tracing::debug!(
                    returned = docs.len(),
                    tracked = rows.len(),
                    "inflight bulk poll"
                );
                docs
            }
            Err(e) => {
                tracing::debug!(error = %e, "inflight bulk poll failed");
                continue;
            }
        };
        let processing_map: std::collections::HashMap<&str, &crate::api::Document> =
            processing.iter().map(|d| (d.id.as_str(), d)).collect();

        let now = now_ms();
        for row in rows {
            // Stuck tiers key on age since last observation, same as before.
            let age = row.last_status_at.map(|t| now - t).unwrap_or(0);
            match stuck_tier(age) {
                StuckTier::Stop => {
                    tracing::error!(
                        ino = row.ino,
                        remote_id = %row.remote_id,
                        age_s = age / 1000,
                        last_status = ?row.last_status,
                        "push: server processing stuck >1h; giving up, marking inode dirty for retry"
                    );
                    db.set_mirrored_state(row.ino, None, Some("stuck"), Some(now));
                    db.set_dirty_since(row.ino, Some(now));
                    continue;
                }
                StuckTier::Warn => {
                    tracing::warn!(
                        ino = row.ino,
                        remote_id = %row.remote_id,
                        age_s = age / 1000,
                        "push: server processing >5min"
                    );
                }
                StuckTier::Info => {
                    tracing::info!(
                        ino = row.ino,
                        remote_id = %row.remote_id,
                        age_s = age / 1000,
                        "push: server processing >60s"
                    );
                }
                StuckTier::Ok => {}
            }

            if let Some(doc) = processing_map.get(row.remote_id.as_str()) {
                // Still in the processing list — update status; mirrored_at
                // stays unchanged (we only advance it when done is confirmed).
                db.set_mirrored_state(row.ino, None, Some(&doc.status), Some(now));
            } else {
                // Disappeared from the processing list — terminal state.
                // Fall back to a single GET to learn which terminal.
                match api.get_document(&row.remote_id).await {
                    Ok(doc) if doc.status == "done" => {
                        let mirrored = crate::cache::parse_iso_to_ms(&doc.updated_at);
                        db.set_mirrored_state(row.ino, mirrored, Some("done"), Some(now));
                        tracing::debug!(
                            ino = row.ino,
                            remote_id = %row.remote_id,
                            "push: status done"
                        );
                    }
                    Ok(doc) => {
                        db.set_mirrored_state(row.ino, None, Some(&doc.status), Some(now));
                    }
                    Err(ApiError::NotFound) => {
                        tracing::warn!(
                            ino = row.ino,
                            remote_id = %row.remote_id,
                            "push: terminal-state GET 404; remote doc vanished"
                        );
                        db.delete_remote_id(row.ino);
                    }
                    Err(e) => {
                        tracing::debug!(
                            ino = row.ino,
                            remote_id = %row.remote_id,
                            error = %e,
                            "push: terminal-state GET error"
                        );
                        db.set_mirrored_state(row.ino, None, None, Some(now));
                    }
                }
            }
        }
    }
}
