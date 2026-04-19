//! `smfs mount` — mount a Supermemory container at a local path.
//!
//! In production this is usually invoked indirectly via
//! `supermemory mount <path> <tag>` from the TypeScript CLI, which reads the
//! user's stored credentials and execs this subcommand with `--key`.
//! It can also be used directly for scripting or debugging.

use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::PathBuf;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Supermemory container tag to mount. One mount per container tag;
    /// mounts cannot overlap or share a path.
    pub container_tag: String,

    /// Mount path. Defaults to ./<container_tag>/ in the current directory.
    #[arg(long)]
    pub path: Option<PathBuf>,

    /// Mount backend (`fuse` or `nfs`). Defaults to `fuse` on Linux and `nfs` on macOS.
    #[arg(long)]
    pub backend: Option<String>,

    /// Run the daemon in the foreground instead of detaching into the background.
    #[arg(long)]
    pub foreground: bool,

    /// Delete local cache before mounting. Pulls fresh from the API.
    #[arg(long)]
    pub clean: bool,

    /// Use in-memory cache. Nothing persists after unmount.
    #[arg(long)]
    pub ephemeral: bool,

    /// Supermemory API key. Saved to project credentials when provided.
    #[arg(long)]
    pub key: Option<String>,

    /// Override the Supermemory API base URL.
    #[arg(long, env = "SUPERMEMORY_API_URL")]
    pub api_url: Option<String>,

    /// Filesystem paths under this mount that should produce memories
    /// (comma-separated). Entries ending with `/` match any file inside that
    /// folder recursively; other entries match exactly.
    ///
    ///   `--memory-paths "/notes/,/journal.md"` → scope to those paths
    ///   `--memory-paths ""`                    → disable memory generation
    ///   (flag omitted)                         → leave existing config alone
    ///
    /// When omitted, nothing is written — the server keeps whatever the tag
    /// already has, falling back to its built-in defaults when unset.
    #[arg(long)]
    pub memory_paths: Option<String>,

    /// Delta-pull interval in seconds (default 30).
    #[arg(long, default_value_t = 30)]
    pub sync_interval: u64,

    /// Deletion-scan interval in seconds (default 300).
    #[arg(long, default_value_t = 300)]
    pub deletion_scan_interval: u64,

    /// Stop polling Supermemory for remote changes. Local writes still
    /// push to the server normally — this only disables the pull side
    /// (delta + deletion scan), so your file edits still sync up even
    /// when the flag is set.
    #[arg(long)]
    pub no_sync: bool,

    /// Max seconds to spend draining the push queue at unmount time.
    /// Queue rows persist in SQLite; anything not drained resumes on next
    /// mount. Default 30s.
    #[arg(long, default_value_t = 30)]
    pub drain_timeout: u64,
}

pub async fn run(args: Args) -> Result<()> {
    use smfs_core::mount::MountBackend;

    // 1. Parse backend (or use OS default).
    let backend = match &args.backend {
        Some(b) => b.parse::<MountBackend>()?,
        None => MountBackend::default(),
    };

    // 2. Resolve mount path (default: ./<container_tag>/ in cwd).
    let mount_path = args.path.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .expect("cannot determine current directory")
            .join(&args.container_tag)
    });

    // 3. Resolve API url + key up-front (in the parent) so we can pass them
    //    to the daemon child as flags.
    let api_url_str = args
        .api_url
        .as_deref()
        .unwrap_or("https://api.supermemory.ai")
        .to_string();
    let api_key = super::auth::resolve_api_key(args.key.as_deref(), Some(&mount_path))?;

    // Auto-save project credentials if --key was explicit.
    if args.key.is_some() {
        let creds = smfs_core::config::credentials::Credentials {
            api_key: api_key.clone(),
            api_url: args.api_url.clone(),
        };
        if let Err(e) = smfs_core::config::credentials::save_project(&mount_path, &creds) {
            tracing::warn!("failed to save project credentials: {e}");
        }
    }

    if args.foreground {
        // Inline path — run the daemon body in this process. Ctrl-C / SIGTERM
        // still unmount cleanly via the IPC shutdown notify.
        let cfg = super::daemon_runtime::DaemonConfig {
            container_tag: args.container_tag.clone(),
            mount_path,
            backend,
            api_key,
            api_url: api_url_str,
            memory_paths: args.memory_paths,
            ephemeral: args.ephemeral,
            clean: args.clean,
            sync_interval: args.sync_interval,
            deletion_scan_interval: args.deletion_scan_interval,
            no_sync: args.no_sync,
            drain_timeout: args.drain_timeout,
        };
        return super::daemon_runtime::run(cfg).await;
    }

    // Default path — fork into a background daemon via `smfs daemon-inner`.
    //
    // Refuse if another daemon already owns this tag.
    if let Some(pid) = smfs_core::daemon::read_pid(&args.container_tag) {
        if smfs_core::daemon::pid_alive(pid) {
            anyhow::bail!(
                "tag '{}' is already mounted (pid {}). Use `smfs unmount` first.",
                args.container_tag,
                pid,
            );
        }
    }
    // Clean any leftover socket/pid from a prior crash.
    smfs_core::daemon::cleanup_stale(&args.container_tag);
    smfs_core::daemon::ensure_dirs()?;

    // Open the per-tag log file for the child's stdout/stderr. Parent
    // handoff: the daemon never writes to the controlling TTY again.
    let log_path = smfs_core::daemon::log_path(&args.container_tag);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_file_err = log_file.try_clone()?;

    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon-inner")
        .arg("--container-tag")
        .arg(&args.container_tag)
        .arg("--mount")
        .arg(&mount_path)
        .arg("--key")
        .arg(&api_key)
        .arg("--api-url")
        .arg(&api_url_str);
    if let Some(b) = &args.backend {
        cmd.arg("--backend").arg(b);
    }
    if let Some(m) = &args.memory_paths {
        cmd.arg("--memory-paths").arg(m);
    }
    if args.ephemeral {
        cmd.arg("--ephemeral");
    }
    if args.clean {
        cmd.arg("--clean");
    }
    cmd.arg("--sync-interval")
        .arg(args.sync_interval.to_string());
    cmd.arg("--deletion-scan-interval")
        .arg(args.deletion_scan_interval.to_string());
    if args.no_sync {
        cmd.arg("--no-sync");
    }
    cmd.arg("--drain-timeout")
        .arg(args.drain_timeout.to_string());
    cmd.stdin(std::process::Stdio::null())
        .stdout(log_file)
        .stderr(log_file_err);

    let child = cmd.spawn()?;
    let child_pid = child.id();
    // The child will self-install a session via setsid; parent just waits
    // until the child's IPC socket comes up, then exits.

    let socket = smfs_core::daemon::socket_path(&args.container_tag);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut last_err: Option<String> = None;
    loop {
        // Ping the daemon — once it responds, we know the mount is live.
        if socket.exists() {
            match smfs_core::daemon::client::send_request(
                &args.container_tag,
                smfs_core::daemon::protocol::Request::Ping,
            )
            .await
            {
                Ok(smfs_core::daemon::protocol::Response::Pong) => break,
                Ok(other) => {
                    last_err = Some(format!("unexpected response: {other:?}"));
                }
                Err(e) => {
                    last_err = Some(format!("ping: {e}"));
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "daemon did not become ready within 30s (pid {}). Log: {}\nLast error: {}",
                child_pid,
                log_path.display(),
                last_err.unwrap_or_else(|| "<none>".into()),
            );
        }
        // Did the child die early?
        if !smfs_core::daemon::pid_alive(child_pid) {
            anyhow::bail!(
                "daemon exited before becoming ready. Log: {}",
                log_path.display()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    eprintln!(
        "supermemoryfs mounted at {} (tag: {}, pid: {})",
        mount_path.display(),
        args.container_tag,
        child_pid,
    );
    eprintln!("log: {}", log_path.display());
    Ok(())
}
