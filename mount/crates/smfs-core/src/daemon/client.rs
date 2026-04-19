//! IPC client — connects to a daemon's unix socket and sends a single
//! JSON request, returns the JSON response.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::protocol::{Request, Response};

/// Send a single request to the daemon that owns the given tag. Returns
/// an error if the daemon isn't running / socket doesn't exist / daemon
/// closes without replying.
pub async fn send_request(tag: &str, req: Request) -> Result<Response> {
    let socket = super::socket_path(tag);
    send_request_to_path(&socket, req).await
}

pub async fn send_request_to_path(socket: &Path, req: Request) -> Result<Response> {
    if !socket.exists() {
        anyhow::bail!("daemon not running (no socket at {})", socket.display());
    }
    let fut = UnixStream::connect(socket);
    let stream = tokio::time::timeout(Duration::from_secs(5), fut)
        .await
        .with_context(|| format!("timeout connecting to {}", socket.display()))?
        .with_context(|| format!("connect to {}", socket.display()))?;

    let (reader, mut writer) = stream.into_split();
    let body = serde_json::to_string(&req)?;
    writer.write_all(body.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.shutdown().await?;

    let mut lines = BufReader::new(reader).lines();
    let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .context("timeout waiting for daemon response")?
        .context("read response line")?
        .context("daemon closed without responding")?;

    Ok(serde_json::from_str(&line)?)
}
