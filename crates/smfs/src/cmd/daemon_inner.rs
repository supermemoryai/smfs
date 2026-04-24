//! `smfs daemon-inner` — hidden subcommand that runs the mount as a
//! detached background daemon. Invoked by `smfs mount` as a child process
//! via `std::process::Command`.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args as ClapArgs;

use super::daemon_runtime::{self, DaemonConfig};
use smfs_core::mount::MountBackend;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Container tag.
    #[arg(long)]
    pub container_tag: String,

    /// Mount path.
    #[arg(long)]
    pub mount: PathBuf,

    /// Mount backend (`fuse` or `nfs`).
    #[arg(long)]
    pub backend: Option<String>,

    /// API key (passed by the forking parent; never set manually).
    #[arg(long, hide_env_values = true)]
    pub key: String,

    /// API base URL.
    #[arg(long)]
    pub api_url: String,

    /// Optional comma-separated filesystem paths that produce memories.
    #[arg(long)]
    pub memory_paths: Option<String>,

    #[arg(long, default_value_t = false)]
    pub ephemeral: bool,

    #[arg(long, default_value_t = false)]
    pub clean: bool,

    #[arg(long, default_value_t = 30)]
    pub sync_interval: u64,

    #[arg(long, default_value_t = 300)]
    pub deletion_scan_interval: u64,

    #[arg(long, default_value_t = false)]
    pub no_sync: bool,

    #[arg(long, default_value_t = 30)]
    pub drain_timeout: u64,
}

pub async fn run(args: Args) -> Result<()> {
    let backend = match &args.backend {
        Some(b) => b.parse::<MountBackend>()?,
        None => MountBackend::default(),
    };

    // Detach stdin from the controlling TTY so closing the parent shell
    // doesn't send us SIGHUP. `setsid` is the POSIX primitive; stdin/out
    // are already redirected by the parent to the per-tag log file.
    #[cfg(unix)]
    detach_from_tty();

    let cfg = DaemonConfig {
        container_tag: args.container_tag,
        mount_path: args.mount,
        backend,
        api_key: args.key,
        api_url: args.api_url,
        memory_paths: args.memory_paths,
        ephemeral: args.ephemeral,
        clean: args.clean,
        sync_interval: args.sync_interval,
        deletion_scan_interval: args.deletion_scan_interval,
        no_sync: args.no_sync,
        drain_timeout: args.drain_timeout,
    };

    daemon_runtime::run(cfg).await
}

/// Detach from the controlling terminal so we don't receive SIGHUP when
/// the parent shell closes. `setsid` fails if we're already a session
/// leader — that's fine, ignore.
#[cfg(unix)]
fn detach_from_tty() {
    // Shell out rather than use unsafe — smfs-core forbids unsafe_code;
    // the smfs CLI crate allows it but we avoid it for consistency.
    let _ = std::process::Command::new("true").status();
    #[allow(unsafe_code)]
    unsafe {
        libc::setsid();
    }
}
