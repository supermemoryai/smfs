//! `smfs sync` — ask a running daemon to force an immediate pull + push drain.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;

use smfs_core::daemon::client::send_request;
use smfs_core::daemon::protocol::{Request, Response};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Container tag of the mount. Defaults to the tag from the nearest
    /// `.smfs` marker walking up from the current dir.
    pub tag: Option<String>,
}

pub async fn run(args: Args) -> Result<()> {
    let tag = super::status::resolve_tag(args.tag)?;
    let resp = send_request(&tag, Request::Sync)
        .await
        .with_context(|| format!("sync for tag '{tag}'"))?;
    match resp {
        Response::SyncDone {
            pulled,
            pushed_pending,
        } => {
            println!("pulled: {pulled}");
            println!("pending push: {pushed_pending}");
            Ok(())
        }
        Response::Error { message } => anyhow::bail!("daemon error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}
