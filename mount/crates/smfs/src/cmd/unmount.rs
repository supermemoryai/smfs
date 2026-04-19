//! `smfs unmount` — graceful shutdown of a running daemon.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Args as ClapArgs;

use smfs_core::daemon::client::send_request;
use smfs_core::daemon::protocol::{Request, Response};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Mountpoint or tag to unmount. If omitted, resolves via the nearest
    /// `.smfs` marker walking up from the current directory.
    pub target: Option<String>,

    /// Force unmount even if drain + graceful shutdown fail (falls back to
    /// `umount` / `fusermount -u`).
    #[arg(long)]
    pub force: bool,
}

pub async fn run(args: Args) -> Result<()> {
    // Resolve tag + optional mountpoint path.
    let (tag, mount_path) = resolve(args.target)?;

    // Send the Unmount request. If the socket doesn't exist the daemon is
    // already gone — fall back to umount-by-path if we know one.
    match send_request(&tag, Request::Unmount).await {
        Ok(Response::UnmountAck) => {}
        Ok(Response::Error { message }) => anyhow::bail!("daemon error: {message}"),
        Ok(other) => anyhow::bail!("unexpected response: {other:?}"),
        Err(e) => {
            if args.force {
                tracing::warn!(error = %e, "IPC unmount failed, forcing via umount");
            } else {
                anyhow::bail!("daemon unreachable: {e}. Use --force to attempt a raw unmount.");
            }
        }
    }

    // Poll the pid file for process exit (bounded).
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if let Some(pid) = smfs_core::daemon::read_pid(&tag) {
            if !smfs_core::daemon::pid_alive(pid) {
                break;
            }
        } else {
            // PID file gone → daemon finished cleanup.
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // Force-umount on the path if still present + --force.
    if args.force {
        if let Some(path) = &mount_path {
            force_umount(path);
        }
    }

    println!("unmounted '{tag}'");
    Ok(())
}

/// Resolve a tag + mountpoint from the CLI target (path | tag | none).
fn resolve(target: Option<String>) -> Result<(String, Option<PathBuf>)> {
    if let Some(t) = target.as_deref() {
        let as_path = PathBuf::from(t);
        if as_path.exists() && as_path.is_dir() {
            // Walk up from the given path to find .smfs
            let marker = read_marker_from(&as_path).with_context(|| {
                format!("no .smfs marker found at or above {}", as_path.display())
            })?;
            return Ok((marker.tag, Some(as_path)));
        }
        // Not a path on disk — treat as tag.
        return Ok((t.to_string(), None));
    }
    let marker = super::marker::read_smfs_marker()
        .context("no target given and no .smfs marker found in cwd ancestors")?;
    let mp = marker.mount_path.as_deref().map(PathBuf::from);
    Ok((marker.tag, mp))
}

fn read_marker_from(start: &Path) -> Option<super::marker::SmfsMarker> {
    let mut dir = start.to_path_buf();
    loop {
        let marker = dir.join(".smfs");
        if marker.exists() {
            let content = std::fs::read_to_string(&marker).ok()?;
            let mut tag = None;
            let mut api_url = None;
            let mut mount_path = None;
            for line in content.lines() {
                if let Some(v) = line.strip_prefix("container_tag=") {
                    tag = Some(v.to_string());
                }
                if let Some(v) = line.strip_prefix("api_url=") {
                    api_url = Some(v.to_string());
                }
                if let Some(v) = line.strip_prefix("mount_path=") {
                    mount_path = Some(v.to_string());
                }
            }
            return Some(super::marker::SmfsMarker {
                tag: tag?,
                api_url: api_url.unwrap_or_else(|| "https://api.supermemory.ai".to_string()),
                mount_path,
            });
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

fn force_umount(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("umount").arg(path).output();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("fusermount")
            .arg("-u")
            .arg(path)
            .output();
    }
}
