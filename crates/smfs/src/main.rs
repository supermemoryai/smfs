//! `smfs` — the supermemoryfs mount daemon binary.
//!
//! This binary is a thin CLI dispatch layer. All real logic lives in the
//! [`smfs_core`] library — this file parses arguments, initializes logging,
//! and hands control to the appropriate command handler.

#![deny(unsafe_code)]

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

mod cmd;

/// Top-level CLI definition.
#[derive(Parser)]
#[command(
    name = "smfs",
    version,
    about = "Mount your Supermemory container as a local filesystem",
    long_about = "supermemoryfs (smfs) — exposes a Supermemory container as a real local directory. \
                  Typically invoked indirectly via `supermemory mount`, but can also be used directly."
)]
struct Cli {
    #[command(subcommand)]
    command: cmd::Command,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    cmd::dispatch(cli.command).await
}

/// Initialize `tracing` with env-based filtering.
///
/// Default level is `info` for both `smfs` and `smfs_core`. Override via
/// `RUST_LOG`, e.g. `RUST_LOG=smfs=debug,smfs_core=trace smfs mount ./mnt`.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("smfs=info,smfs_core=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
