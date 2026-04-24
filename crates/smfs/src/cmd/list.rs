//! `smfs list` — enumerate running daemons by scanning the sockets
//! directory and pinging each one for live status.

use anyhow::Result;

use smfs_core::daemon;
use smfs_core::daemon::client::send_request;
use smfs_core::daemon::protocol::{Request, Response};

pub async fn run() -> Result<()> {
    let dir = daemon::sockets_dir();
    let mut rows: Vec<(String, String, u32, u64, usize)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(tag) = name.strip_suffix(".sock") else {
                continue;
            };
            match send_request(tag, Request::Status).await {
                Ok(Response::Status {
                    tag,
                    mount_path,
                    pid,
                    uptime_secs,
                    queue_len,
                    ..
                }) => rows.push((tag, mount_path, pid, uptime_secs, queue_len)),
                Ok(_) | Err(_) => {
                    // Stale socket — cleanup sweep is the mount command's job.
                }
            }
        }
    }
    if rows.is_empty() {
        println!("no active mounts");
        return Ok(());
    }
    println!(
        "{:<24}  {:<10}  {:<10}  {:<10}  MOUNT",
        "TAG", "PID", "UPTIME", "QUEUE"
    );
    for (tag, mount_path, pid, uptime, queue) in rows {
        println!(
            "{:<24}  {:<10}  {:<10}  {:<10}  {}",
            tag,
            pid,
            format_uptime(uptime),
            queue,
            mount_path
        );
    }
    Ok(())
}

fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}
