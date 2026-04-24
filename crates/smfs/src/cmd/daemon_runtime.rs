//! Shared daemon runtime. Called by both `smfs mount --foreground` and
//! `smfs daemon-inner` (the hidden subcommand invoked by the forking
//! parent). Owns the full lifecycle: open the cache, start the sync
//! engine, mount the filesystem, expose the IPC control socket, then
//! block on SIGTERM / SIGINT / IPC unmount and run the drain/unmount path.
//!
//! The one thing this module does NOT do is any TTY detachment
//! (`setsid` + stdio redirection) — those are the caller's responsibility
//! because the two caller profiles (foreground vs daemon child) differ.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::sync::Notify;

use smfs_core::cache::{Db, SupermemoryFs};
use smfs_core::daemon;
use smfs_core::mount::{mount_fs, MountBackend, MountOpts};

/// Config needed to run the daemon body — subset of `mount::Args` that
/// drives behavior. Built once by `mount::run` and passed through either
/// an inline call (foreground) or a re-exec into `daemon-inner`.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub container_tag: String,
    pub mount_path: PathBuf,
    pub backend: MountBackend,
    pub api_key: String,
    pub api_url: String,
    pub memory_paths: Option<String>,
    pub ephemeral: bool,
    pub clean: bool,
    pub sync_interval: u64,
    pub deletion_scan_interval: u64,
    pub no_sync: bool,
    pub drain_timeout: u64,
}

pub async fn run(cfg: DaemonConfig) -> Result<()> {
    let created_dir = !cfg.mount_path.exists();
    if created_dir {
        std::fs::create_dir_all(&cfg.mount_path)?;
    }

    // uid/gid of the invoking user for the mount ownership.
    #[allow(unsafe_code)]
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };

    let marker_path = cfg
        .mount_path
        .parent()
        .unwrap_or(&cfg.mount_path)
        .join(".smfs");
    std::fs::write(
        &marker_path,
        format!(
            "container_tag={}\napi_url={}\nmount_path={}\n",
            cfg.container_tag,
            cfg.api_url,
            cfg.mount_path.display(),
        ),
    )?;

    let opts = MountOpts::new(cfg.mount_path.clone(), cfg.backend).with_ownership(uid, gid);

    let session = if cfg.ephemeral {
        smfs_core::api::ApiClient::validate_key(&cfg.api_url, &cfg.api_key)
            .await
            .ok()
    } else {
        Some(
            smfs_core::api::ApiClient::validate_key(&cfg.api_url, &cfg.api_key)
                .await
                .context("validating API key (required to scope cache by org)")?,
        )
    };

    let db = if cfg.ephemeral {
        eprintln!("using ephemeral in-memory cache (nothing persists after unmount)");
        Arc::new(Db::open_in_memory()?)
    } else {
        let org_id = session
            .as_ref()
            .and_then(|s| s.org_id.as_deref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "server did not return org id; cannot open cache. Run `smfs login` and retry."
                )
            })?;
        let db_path = smfs_core::config::cache_db_path(org_id, &cfg.container_tag);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let legacy_path = smfs_core::config::legacy_cache_db_path(&cfg.container_tag);
        if legacy_path.exists() && legacy_path != db_path {
            let _ = std::fs::remove_file(&legacy_path);
            let _ = std::fs::remove_file(legacy_path.with_extension("db-wal"));
            let _ = std::fs::remove_file(legacy_path.with_extension("db-shm"));
        }
        if cfg.clean {
            let _ = std::fs::remove_file(&db_path);
            let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
            let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
            eprintln!("cache cleared");
        }
        Arc::new(Db::open(&db_path)?)
    };

    let mut api_client =
        smfs_core::api::ApiClient::new(&cfg.api_url, &cfg.api_key, &cfg.container_tag);
    if let Some(uid) = session.as_ref().and_then(|s| s.user_id.clone()) {
        api_client = api_client.with_user_id(uid);
    }
    let api = Arc::new(api_client);
    let session_user_id = session.as_ref().and_then(|s| s.user_id.clone());
    let session_user_name = session.as_ref().and_then(|s| s.user_name.clone());
    let session_org_name = session.as_ref().map(|s| s.org_name.clone());

    if let Some(raw) = &cfg.memory_paths {
        let paths: Vec<String> = if raw.is_empty() {
            Vec::new()
        } else {
            raw.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        api.update_memory_paths(paths).await?;
    }

    let fs = Arc::new(SupermemoryFs::with_api(db, api));

    // Initial pull is a pull-side op — gated by --no-sync.
    if !cfg.no_sync {
        match smfs_core::sync::SyncEngine::initial_pull(&fs).await {
            Ok((removed, reconciled)) => {
                eprintln!(
                    "initial sync: {reconciled} docs reconciled, {removed} stale entries removed"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "initial sync failed; mount will continue");
            }
        }
    }

    // Sync engine: push always on, pull gated by --no-sync.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let sync_opts = smfs_core::sync::SyncOptions {
        delta_interval: std::time::Duration::from_secs(cfg.sync_interval),
        deletion_scan_interval: std::time::Duration::from_secs(cfg.deletion_scan_interval),
        pull_enabled: !cfg.no_sync,
    };
    let sync_tasks = smfs_core::sync::SyncEngine::start(fs.clone(), sync_opts, shutdown_rx.clone());

    let handle = mount_fs(fs.clone(), opts).await?;

    // Auto-install grep wrapper on first mount.
    if let Ok(true) = super::init::ensure_grep_wrapper_present() {
        eprintln!(
            "semantic grep enabled. run: source ~/.zshrc (new terminals have it automatically)"
        );
    }

    // Bring up the IPC control socket. Clients use it for status/sync/unmount.
    daemon::ensure_dirs().context("creating daemon state dirs")?;
    let ipc_shutdown_notify = Arc::new(Notify::new());
    let state = Arc::new(smfs_core::daemon::ipc::IpcState {
        tag: cfg.container_tag.clone(),
        mount_path: cfg.mount_path.display().to_string(),
        fs: fs.clone(),
        started_at: Instant::now(),
        pull_enabled: !cfg.no_sync,
        user_id: session_user_id,
        user_name: session_user_name,
        org_name: session_org_name,
        shutdown_notify: ipc_shutdown_notify.clone(),
    });
    let socket_path = daemon::socket_path(&cfg.container_tag);
    let ipc_shutdown_rx = shutdown_rx.clone();
    let ipc_socket = socket_path.clone();
    let ipc_handle = tokio::spawn(async move {
        if let Err(e) = daemon::ipc::serve(state, ipc_socket, ipc_shutdown_rx).await {
            tracing::warn!(error = %e, "ipc server exited with error");
        }
    });

    // Write our PID. Keep it alive for the life of the process; cleaned
    // up at the end of this function.
    let pid_path = daemon::pid_path(&cfg.container_tag);
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&pid_path, std::process::id().to_string())?;

    eprintln!(
        "supermemoryfs mounted at {} (backend: {}, tag: {})",
        handle.mountpoint().display(),
        handle.backend(),
        cfg.container_tag,
    );

    // Wait for SIGTERM, SIGINT, or IPC `Unmount` request.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("register SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
            _ = ipc_shutdown_notify.notified() => {},
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = ipc_shutdown_notify.notified() => {},
        }
    }
    eprintln!("\nunmounting...");

    // Drain the push queue. Push-side op — runs regardless of --no-sync.
    let deadline = Instant::now() + std::time::Duration::from_secs(cfg.drain_timeout);
    let mut last_report = 0usize;
    loop {
        let n = fs.push_queue_len();
        if n == 0 {
            if last_report > 0 {
                eprintln!("push queue drained");
            }
            break;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                pending = n,
                "push queue drain timeout; rows persist and will resume next mount"
            );
            eprintln!("push queue drain timed out with {n} pending (will resume next mount)");
            break;
        }
        if n != last_report {
            eprintln!("draining push queue: {n} pending");
            last_report = n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // Signal sync + IPC loops to exit.
    let _ = shutdown_tx.send(true);
    let mut set = sync_tasks;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while set.join_next().await.is_some() {}
    })
    .await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), ipc_handle).await;

    // Final deletion scan — pull-side, gated by --no-sync.
    if !cfg.no_sync {
        smfs_core::sync::SyncEngine::unmount_scan(&fs).await;
    }

    drop(handle);
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("umount")
            .arg(&cfg.mount_path)
            .output();
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let _ = std::fs::remove_file(&marker_path);
    let _ = std::fs::remove_file(&pid_path);
    let _ = std::fs::remove_file(&socket_path);
    if created_dir {
        let _ = std::fs::remove_dir(&cfg.mount_path);
    }
    Ok(())
}
