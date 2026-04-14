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
    /// Path where the filesystem should be mounted (must exist).
    pub path: PathBuf,

    /// Supermemory container tag to mount. One mount per container tag;
    /// mounts cannot overlap or share a path.
    pub container_tag: String,

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

    /// Supermemory API key. Normally passed by the TS CLI; accepted here for direct use.
    #[arg(long, env = "SUPERMEMORY_API_KEY", hide_env_values = true)]
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

    // 2. Create mountpoint if it doesn't exist.
    if !args.path.exists() {
        std::fs::create_dir_all(&args.path)?;
    }

    // 3. Get effective uid/gid of the calling user.
    #[allow(unsafe_code)]
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };

    // 4. Build MountOpts.
    let opts = MountOpts::new(args.path.clone(), backend).with_ownership(uid, gid);

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

    let fs = Arc::new(match &args.key {
        Some(key) => {
            let api = Arc::new(smfs_core::api::ApiClient::new(
                args.api_url.as_deref().unwrap_or("https://api.supermemory.ai"),
                key,
                &args.container_tag,
            ));
            SupermemoryFs::with_api(db, api)
        }
        None => SupermemoryFs::new(db),
    });
    let handle = mount_fs(fs, opts).await?;

    eprintln!(
        "supermemoryfs mounted at {} (backend: {}, ctrl+c to unmount)",
        handle.mountpoint().display(),
        handle.backend(),
    );

    // 6. Hold mount until Ctrl+C.
    tokio::signal::ctrl_c().await?;
    eprintln!("\nunmounting...");

    drop(handle);
    Ok(())
}
