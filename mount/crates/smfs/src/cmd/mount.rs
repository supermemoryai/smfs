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
}

pub async fn run(args: Args) -> Result<()> {
    use smfs_core::cache::{Db, SupermemoryFs};
    use smfs_core::mount::{mount_fs, MountBackend, MountOpts};
    use std::sync::Arc;

    // 1. Parse backend (or use OS default).
    let backend = match &args.backend {
        Some(b) => b.parse::<MountBackend>()?,
        None => MountBackend::default(),
    };

    // 2. Resolve mount path (default: ./<container_tag>/ in current directory).
    let mount_path = args.path.unwrap_or_else(|| {
        std::env::current_dir()
            .expect("cannot determine current directory")
            .join(&args.container_tag)
    });
    let created_dir = !mount_path.exists();
    if created_dir {
        std::fs::create_dir_all(&mount_path)?;
    }

    // 3. Get effective uid/gid of the calling user.
    #[allow(unsafe_code)]
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };

    // 4. Write .smfs marker in the parent directory.
    let marker_path = mount_path.parent().unwrap_or(&mount_path).join(".smfs");
    let api_url_str = args
        .api_url
        .as_deref()
        .unwrap_or("https://api.supermemory.ai");
    std::fs::write(
        &marker_path,
        format!(
            "container_tag={}\napi_url={}\nmount_path={}\n",
            args.container_tag,
            api_url_str,
            mount_path.display(),
        ),
    )?;

    // 5. Build MountOpts.
    let mount_path_copy = mount_path.clone();
    let opts = MountOpts::new(mount_path, backend).with_ownership(uid, gid);

    // 5. Open SQLite cache and create SupermemoryFs.
    let db = if args.ephemeral {
        eprintln!("using ephemeral in-memory cache (nothing persists after unmount)");
        Arc::new(Db::open_in_memory()?)
    } else {
        let db_path = smfs_core::config::cache_db_path(&args.container_tag);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if args.clean {
            let _ = std::fs::remove_file(&db_path);
            let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
            let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
            eprintln!("cache cleared");
        }
        Arc::new(Db::open(&db_path)?)
    };

    let api_key = super::auth::resolve_api_key(args.key.as_deref(), Some(&mount_path_copy))?;

    // Auto-save to project credentials when --key is explicitly provided.
    if args.key.is_some() {
        let creds = smfs_core::config::credentials::Credentials {
            api_key: api_key.clone(),
            api_url: args.api_url.clone(),
        };
        if let Err(e) = smfs_core::config::credentials::save_project(&mount_path_copy, &creds) {
            tracing::warn!("failed to save project credentials: {e}");
        }
    }

    let api = Arc::new(smfs_core::api::ApiClient::new(
        api_url_str,
        &api_key,
        &args.container_tag,
    ));
    let fs = Arc::new(SupermemoryFs::with_api(db, api));
    let handle = mount_fs(fs, opts).await?;

    // Auto-install grep wrapper on first mount.
    if let Ok(true) = super::init::ensure_grep_wrapper_installed() {
        eprintln!(
            "semantic grep enabled. run: source ~/.zshrc (new terminals have it automatically)"
        );
    }

    eprintln!(
        "supermemoryfs mounted at {} (backend: {}, ctrl+c to unmount)",
        handle.mountpoint().display(),
        handle.backend(),
    );

    // 6. Hold mount until Ctrl+C or SIGTERM.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("register SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    eprintln!("\nunmounting...");

    drop(handle);
    // Explicitly unmount in case the handle drop didn't (Linux FUSE).
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("umount")
            .arg(&mount_path_copy)
            .output();
    }
    // Wait for kernel to release the mount before cleanup.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let _ = std::fs::remove_file(&marker_path);
    if created_dir {
        let _ = std::fs::remove_dir(&mount_path_copy);
    }
    Ok(())
}
