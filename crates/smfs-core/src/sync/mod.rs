//! Background sync engine.
//!
//! Four loops:
//!
//! - **Loop A — delta pull.** Every ~30s, walk `/v3/documents/list` sorted by
//!   `updatedAt desc` and reconcile anything newer than our watermark into
//!   the local cache.
//! - **Loop C — deletion scan.** Every ~5min, diff the full remote ID set
//!   against the local `fs_remote` table and unlink anything that
//!   disappeared.
//! - **Loop D — push worker.** Claims queued push jobs from `push_queue`
//!   and sends them; coalesces rapid writes to at most 2 server requests
//!   per filepath (one inflight + one pending).
//! - **Loop E — inflight poller.** Polls `GET /v3/documents/:id` for docs
//!   whose server-side processing hasn't flipped to `done` yet; updates
//!   `mirrored_updated_at` and emits INFO/WARN/STOP tiers when stuck.

pub mod pull;
pub mod push;
pub mod scan;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::cache::SupermemoryFs;

/// Knobs for the sync engine. All optional — defaults are production-sane.
#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    pub delta_interval: Duration,
    pub deletion_scan_interval: Duration,
    /// When `false`, skip the pull-side loops (A delta pull, C deletion
    /// scan). Push (D) and inflight status poller (E) always run because
    /// killing them would stop delivering local writes to Supermemory —
    /// which defeats the purpose of mounting. Default `true`.
    pub pull_enabled: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            delta_interval: Duration::from_secs(30),
            deletion_scan_interval: Duration::from_secs(300),
            pull_enabled: true,
        }
    }
}

/// Orchestrates background sync for a mount. Spawn with [`SyncEngine::start`]
/// and signal shutdown via the `watch::Sender<bool>` (true = stop).
#[derive(Debug)]
pub struct SyncEngine;

impl SyncEngine {
    /// Run the synchronous startup sequence: a full deletion scan first (to
    /// catch anything deleted while we were offline) and a full pull (to
    /// hydrate everything else). Blocks until both complete.
    pub async fn initial_pull(fs: &Arc<SupermemoryFs>) -> anyhow::Result<(usize, usize)> {
        let removed = scan::deletion_scan(fs).await.unwrap_or(0);
        let reconciled = pull::full_pull(fs).await?;
        Ok((removed, reconciled))
    }

    /// Spawn background loops for this mount. Push (D) and inflight status
    /// poller (E) are always spawned — they are the mount's write path and
    /// disabling them would defeat the purpose of running the mount.
    ///
    /// Pull-side loops — A (delta pull) and C (deletion scan) — are spawned
    /// only when `opts.pull_enabled` is true. Setting it to false is how
    /// `smfs mount --no-sync` stops polling for remote changes while
    /// keeping local writes flowing to Supermemory.
    ///
    /// Returns a JoinSet whose tasks exit when `shutdown.send(true)` is
    /// called.
    pub fn start(
        fs: Arc<SupermemoryFs>,
        opts: SyncOptions,
        shutdown: watch::Receiver<bool>,
    ) -> JoinSet<()> {
        let mut set = JoinSet::new();

        if opts.pull_enabled {
            let fs_a = fs.clone();
            let mut sd_a = shutdown.clone();
            set.spawn(async move {
                run_delta_loop(fs_a, opts.delta_interval, &mut sd_a).await;
            });

            let fs_c = fs.clone();
            let mut sd_c = shutdown.clone();
            set.spawn(async move {
                run_deletion_loop(fs_c, opts.deletion_scan_interval, &mut sd_c).await;
            });
        }

        let fs_d = fs.clone();
        let sd_d = shutdown.clone();
        set.spawn(async move {
            push::run_push_worker(fs_d, sd_d).await;
        });

        let fs_e = fs.clone();
        let sd_e = shutdown.clone();
        set.spawn(async move {
            push::run_inflight_poller(fs_e, sd_e).await;
        });

        set
    }

    /// Final deletion scan before the mount releases. Best-effort: logs on
    /// failure and returns.
    pub async fn unmount_scan(fs: &Arc<SupermemoryFs>) {
        match scan::deletion_scan(fs).await {
            Ok(n) if n > 0 => tracing::info!(removed = n, "final deletion scan"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "final deletion scan failed"),
        }
    }
}

async fn run_delta_loop(
    fs: Arc<SupermemoryFs>,
    base_interval: Duration,
    shutdown: &mut watch::Receiver<bool>,
) {
    let mut empty_streak = 0u32;
    loop {
        let interval = adaptive_interval(base_interval, empty_streak);
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
        }

        match pull::delta_pull(&fs).await {
            Ok(n) => {
                if n == 0 {
                    empty_streak = empty_streak.saturating_add(1);
                } else {
                    empty_streak = 0;
                    tracing::debug!(reconciled = n, "delta pull");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "delta pull failed");
            }
        }
    }
}

async fn run_deletion_loop(
    fs: Arc<SupermemoryFs>,
    base_interval: Duration,
    shutdown: &mut watch::Receiver<bool>,
) {
    loop {
        let interval = jittered(base_interval, 30);
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
        }

        match scan::deletion_scan(&fs).await {
            Ok(n) if n > 0 => tracing::info!(removed = n, "deletion scan"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "deletion scan failed"),
        }
    }
}

/// Adaptive cadence: shorter after activity, stretch when idle, add ±jitter.
fn adaptive_interval(base: Duration, empty_streak: u32) -> Duration {
    let secs = base.as_secs_f64();
    let adjusted = if empty_streak == 0 {
        (secs / 3.0).max(10.0)
    } else if empty_streak >= 3 {
        (secs * 2.0).min(60.0)
    } else {
        secs
    };
    jittered(Duration::from_secs_f64(adjusted), 5)
}

/// Add uniform ±`max_jitter_secs` jitter to an interval (never below 1s).
fn jittered(base: Duration, max_jitter_secs: i64) -> Duration {
    // Cheap pseudo-random from system time nanos; avoids pulling in a
    // dedicated RNG crate for this one use.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as i64)
        .unwrap_or(0);
    let jitter = (nanos % (2 * max_jitter_secs + 1)) - max_jitter_secs;
    let secs = (base.as_secs() as i64 + jitter).max(1);
    Duration::from_secs(secs as u64)
}
