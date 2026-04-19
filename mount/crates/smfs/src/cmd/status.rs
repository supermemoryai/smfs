//! `smfs status` — ask a running daemon for its current state.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;

use smfs_core::daemon::client::send_request;
use smfs_core::daemon::protocol::{Request, Response};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Container tag of the mount to query. Defaults to the tag resolved
    /// from the nearest `.smfs` marker walking up from the current dir.
    pub tag: Option<String>,

    /// Emit the raw JSON response instead of a formatted line.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: Args) -> Result<()> {
    let tag = resolve_tag(args.tag)?;
    let resp = send_request(&tag, Request::Status)
        .await
        .with_context(|| format!("status for tag '{tag}'"))?;
    match resp {
        Response::Status {
            tag,
            mount_path,
            pid,
            uptime_secs,
            queue_len,
            pull_enabled,
        } => {
            if args.json {
                println!(
                    "{{\"tag\":\"{tag}\",\"mount_path\":\"{mount_path}\",\"pid\":{pid},\"uptime_secs\":{uptime_secs},\"queue_len\":{queue_len},\"pull_enabled\":{pull_enabled}}}"
                );
            } else {
                println!("tag:          {tag}");
                println!("mount path:   {mount_path}");
                println!("pid:          {pid}");
                println!("uptime:       {uptime_secs}s");
                println!("push queue:   {queue_len} pending");
                println!("pull enabled: {pull_enabled}");
            }
            Ok(())
        }
        Response::Error { message } => anyhow::bail!("daemon error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

pub(crate) fn resolve_tag(explicit: Option<String>) -> Result<String> {
    if let Some(t) = explicit {
        return Ok(t);
    }
    let marker = super::marker::read_smfs_marker()
        .context("no --tag given and no .smfs marker found in cwd ancestors")?;
    Ok(marker.tag)
}
