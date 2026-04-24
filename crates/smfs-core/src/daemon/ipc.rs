//! IPC server. Runs inside the daemon as a tokio task.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{watch, Notify};

use super::protocol::{Request, Response};
use crate::cache::SupermemoryFs;

/// State the IPC handler reads to answer requests.
#[allow(missing_debug_implementations)] // SupermemoryFs doesn't implement Debug in full
pub struct IpcState {
    pub tag: String,
    pub mount_path: String,
    pub fs: Arc<SupermemoryFs>,
    pub started_at: Instant,
    pub pull_enabled: bool,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub org_name: Option<String>,
    /// Fired when an `Unmount` request arrives — daemon main loop awaits this
    /// and treats it the same as SIGTERM.
    pub shutdown_notify: Arc<Notify>,
}

/// Bind to `socket_path`, accept connections, dispatch one request per.
/// Exits when the shutdown watch channel flips true.
pub async fn serve(
    state: Arc<IpcState>,
    socket_path: std::path::PathBuf,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    // Clean any leftover socket from a crashed prior run.
    let _ = std::fs::remove_file(&socket_path);
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    tracing::info!(socket = %socket_path.display(), "IPC socket ready");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            res = listener.accept() => {
                match res {
                    Ok((stream, _addr)) => {
                        let s = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_conn(stream, s).await {
                                tracing::debug!(error = %e, "ipc handler error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "ipc accept failed");
                    }
                }
            }
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

async fn handle_conn(stream: UnixStream, state: Arc<IpcState>) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };

    let resp = match serde_json::from_str::<Request>(&line) {
        Ok(req) => dispatch(req, &state).await,
        Err(e) => Response::Error {
            message: format!("invalid request: {e}"),
        },
    };

    let body = serde_json::to_string(&resp)?;
    writer.write_all(body.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.shutdown().await?;
    Ok(())
}

async fn dispatch(req: Request, state: &IpcState) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Status => Response::Status {
            tag: state.tag.clone(),
            mount_path: state.mount_path.clone(),
            pid: std::process::id(),
            uptime_secs: state.started_at.elapsed().as_secs(),
            queue_len: state.fs.push_queue_len(),
            pull_enabled: state.pull_enabled,
            user_id: state.user_id.clone(),
            user_name: state.user_name.clone(),
            org_name: state.org_name.clone(),
        },
        Request::Sync => {
            let pulled = crate::sync::pull::delta_pull(&state.fs).await.unwrap_or(0);
            // Wait briefly for push queue to drain before responding, so
            // the caller gets a more useful "pushed_pending" number.
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline && state.fs.push_queue_len() > 0 {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Response::SyncDone {
                pulled,
                pushed_pending: state.fs.push_queue_len(),
            }
        }
        Request::Unmount => {
            state.shutdown_notify.notify_waiters();
            Response::UnmountAck
        }
    }
}
