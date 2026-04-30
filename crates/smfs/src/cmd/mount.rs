//! `smfs mount` — mount a Supermemory container at a local path.
//!
//! In production this is usually invoked indirectly via
//! `supermemory mount <path> <tag>` from the TypeScript CLI, which reads the
//! user's stored credentials and execs this subcommand with `--key`.
//! It can also be used directly for scripting or debugging.

use anyhow::{Context, Result};
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

    /// Internal: skip injecting the path-scoped agent hint. Used for
    /// baseline measurement; not part of the supported user surface.
    #[arg(long, hide = true)]
    pub no_inject_hint: bool,

    #[arg(long)]
    pub no_import: bool,
}

pub async fn run(args: Args) -> Result<()> {
    use smfs_core::mount::MountBackend;

    // 1. Parse backend (or use OS default).
    let backend = match &args.backend {
        Some(b) => b.parse::<MountBackend>()?,
        None => MountBackend::default(),
    };

    let (container_tag, mount_path) = resolve_tag_and_path(&args.container_tag, args.path)?;

    let api_key = super::auth::resolve_api_key(args.key.as_deref(), Some(&mount_path))?;
    let api_url_str = args
        .api_url
        .clone()
        .or_else(|| {
            smfs_core::config::credentials::load_project(&mount_path).and_then(|c| c.api_url)
        })
        .or_else(|| smfs_core::config::credentials::load_global().and_then(|c| c.api_url))
        .unwrap_or_else(|| "https://api.supermemory.ai".to_string());

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

    let import_existing = !args.no_import;

    if args.foreground {
        // Inline path — run the daemon body in this process. Ctrl-C / SIGTERM
        // still unmount cleanly via the IPC shutdown notify.
        let cfg = super::daemon_runtime::DaemonConfig {
            container_tag: container_tag.clone(),
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
            import_existing,
        };
        return super::daemon_runtime::run(cfg).await;
    }

    // Default path — fork into a background daemon via `smfs daemon-inner`.
    //
    // Refuse if another daemon already owns this tag.
    if let Some(pid) = smfs_core::daemon::read_pid(&container_tag) {
        if smfs_core::daemon::pid_alive(pid) {
            anyhow::bail!(
                "tag '{}' is already mounted (pid {}). Use `smfs unmount` first.",
                container_tag,
                pid,
            );
        }
    }
    // Clean any leftover socket/pid from a prior crash.
    smfs_core::daemon::cleanup_stale(&container_tag);
    smfs_core::daemon::ensure_dirs()?;

    // Open the per-tag log file for the child's stdout/stderr. Parent
    // handoff: the daemon never writes to the controlling TTY again.
    let log_path = smfs_core::daemon::log_path(&container_tag);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_file_err = log_file.try_clone()?;

    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon-inner")
        .arg("--container-tag")
        .arg(&container_tag)
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
    if !import_existing {
        cmd.arg("--no-import");
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

    let socket = smfs_core::daemon::socket_path(&container_tag);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut last_err: Option<String> = None;
    loop {
        // Ping the daemon — once it responds, we know the mount is live.
        if socket.exists() {
            match smfs_core::daemon::client::send_request(
                &container_tag,
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
        container_tag,
        child_pid,
    );
    eprintln!("log: {}", log_path.display());

    // Best-effort: opportunistically clean orphan hint blocks from prior
    // crashed daemons, then install the path-scoped hint for this mount.
    // Failures here must NEVER fail the mount itself.
    install_hint_best_effort(&container_tag, &mount_path, args.no_inject_hint);

    Ok(())
}

/// Sweep stale hints + install the new one. Logs to stderr; never fails the
/// caller.
fn install_hint_best_effort(tag: &str, mount_path: &std::path::Path, skip_install: bool) {
    use smfs_core::agent_hint;

    match agent_hint::sweep_orphans(Some(tag)) {
        Ok(report) if !report.is_empty() => {
            let total_tags: usize = report.iter().map(|(_, ts)| ts.len()).sum();
            let tags_joined: Vec<String> = report
                .iter()
                .flat_map(|(_, ts)| ts.iter().cloned())
                .collect();
            eprintln!(
                "✓ Cleaned {} stale hint(s) ({})",
                total_tags,
                tags_joined.join(", "),
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "agent-hint sweep failed"),
    }

    if skip_install {
        return;
    }
    // Resolve the absolute, canonical mount path so the rule body names a
    // path the agent can actually match on (resolves symlinks, removes ./).
    let canonical = std::fs::canonicalize(mount_path).unwrap_or_else(|_| mount_path.to_path_buf());
    match agent_hint::install(tag, &canonical) {
        Ok(written) if !written.is_empty() => {
            let names: Vec<String> = written.iter().map(|p| friendly_path(p)).collect();
            eprintln!(
                "✓ Updated {} (path-scoped semantic-search hint; auto-removed on unmount)",
                names.join(", "),
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "agent-hint install failed"),
    }
}

/// Render a path with `$HOME` collapsed to `~` for cleaner log output.
fn friendly_path(p: &std::path::Path) -> String {
    if let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()) {
        if let Ok(rel) = p.strip_prefix(&home) {
            return format!("~/{}", rel.display());
        }
    }
    p.display().to_string()
}

fn looks_like_path(s: &str) -> bool {
    s == "." || s == ".." || s.contains('/')
}

fn validate_tag(tag: &str) -> anyhow::Result<()> {
    if tag.is_empty() {
        anyhow::bail!("container tag cannot be empty");
    }
    if tag.len() > 100 {
        anyhow::bail!("container tag must be 100 characters or less");
    }
    if let Some(bad) = tag
        .chars()
        .find(|c| !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | ':'))
    {
        anyhow::bail!(
            "container tag '{tag}' contains unsupported character '{bad}'. \
             Allowed: a-z A-Z 0-9 _ - :"
        );
    }
    Ok(())
}

fn normalize_to_absolute(raw: &std::path::Path) -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::fs::canonicalize(raw) {
        return Ok(p);
    }
    let base = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        std::env::current_dir()
            .context("cannot determine current directory")?
            .join(raw)
    };
    let mut normalized = PathBuf::new();
    for component in base.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            c => normalized.push(c),
        }
    }
    Ok(normalized)
}

fn resolve_tag_and_path(
    positional: &str,
    explicit_path: Option<PathBuf>,
) -> anyhow::Result<(String, PathBuf)> {
    if looks_like_path(positional) {
        if explicit_path.is_some() {
            anyhow::bail!("cannot use both a path as the tag and --path");
        }
        let canon = normalize_to_absolute(std::path::Path::new(positional))?;
        let tag = canon
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| {
                anyhow::anyhow!("cannot derive container tag from path '{positional}'")
            })?;
        validate_tag(&tag)?;
        Ok((tag, canon))
    } else {
        validate_tag(positional)?;
        let mount_path = match explicit_path {
            Some(p) => normalize_to_absolute(&p)?,
            None => std::env::current_dir()
                .context("cannot determine current directory")?
                .join(positional),
        };
        Ok((positional.to_string(), mount_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn looks_like_path_cases() {
        assert!(looks_like_path("."));
        assert!(looks_like_path(".."));
        assert!(looks_like_path("./notes"));
        assert!(looks_like_path("../sibling"));
        assert!(looks_like_path("/absolute/path"));
        assert!(looks_like_path("foo/bar"));

        assert!(!looks_like_path("mytag"));
        assert!(!looks_like_path("prod-notes"));
        assert!(!looks_like_path("user_123"));
        assert!(!looks_like_path("project:alpha"));
    }

    #[test]
    fn looks_like_path_no_is_dir_check() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("mycontainer");
        fs::create_dir(&dir).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        assert!(!looks_like_path("mycontainer"));
        std::env::set_current_dir(prev).unwrap();
    }

    #[test]
    fn resolve_tag_and_path_plain_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let (tag, path) = resolve_tag_and_path("mynotes", None).unwrap();
        assert_eq!(tag, "mynotes");
        assert!(path.is_absolute());
        assert!(path.ends_with("mynotes"));
        let _ = tmp;
    }

    #[test]
    fn resolve_tag_and_path_explicit_path() {
        let tmp = tempfile::tempdir().unwrap();
        let (tag, path) = resolve_tag_and_path("mynotes", Some(tmp.path().to_path_buf())).unwrap();
        assert_eq!(tag, "mynotes");
        assert_eq!(path, tmp.path().canonicalize().unwrap());
    }

    #[test]
    fn resolve_tag_and_path_explicit_relative_path_normalizes_to_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let (_, path) = resolve_tag_and_path("mytag", Some(PathBuf::from("subdir"))).unwrap();
        std::env::set_current_dir(prev).unwrap();
        assert!(
            path.is_absolute(),
            "relative --path must normalize to absolute: {path:?}"
        );
        assert!(path.ends_with("subdir"));
    }

    #[test]
    fn resolve_tag_and_path_dot() {
        let cwd = std::env::current_dir().unwrap();
        let expected_tag = cwd.file_name().unwrap().to_string_lossy().into_owned();
        let (tag, path) = resolve_tag_and_path(".", None).unwrap();
        assert_eq!(tag, expected_tag);
        assert!(path.is_absolute());
    }

    #[test]
    fn resolve_tag_and_path_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let named = tmp.path().join("mycontainer");
        fs::create_dir(&named).unwrap();
        let abs = named.to_str().unwrap();
        let (tag, path) = resolve_tag_and_path(abs, None).unwrap();
        assert_eq!(tag, "mycontainer");
        assert!(path.is_absolute());
    }

    #[test]
    fn resolve_tag_and_path_nonexistent_relative() {
        let (tag, path) = resolve_tag_and_path("./newdir", None).unwrap();
        assert_eq!(tag, "newdir");
        assert!(
            path.is_absolute(),
            "path must be absolute even when dir doesn't exist: {path:?}"
        );
    }

    #[test]
    fn resolve_tag_and_path_dot_with_explicit_path_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_tag_and_path(".", Some(tmp.path().to_path_buf()));
        assert!(err.is_err());
    }

    #[test]
    fn resolve_tag_and_path_existing_dir_named_as_tag_with_path_works() {
        let tmp = tempfile::tempdir().unwrap();
        let tag_dir = tmp.path().join("mytag");
        fs::create_dir(&tag_dir).unwrap();
        let other = tmp.path().join("other");
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let (tag, path) = resolve_tag_and_path("mytag", Some(other.clone())).unwrap();
        std::env::set_current_dir(prev).unwrap();
        assert_eq!(tag, "mytag");
        assert_eq!(path, other);
    }

    #[test]
    fn resolve_tag_and_path_root_errors() {
        let err = resolve_tag_and_path("/", None);
        assert!(err.is_err());
    }

    #[test]
    fn validate_tag_rejects_dot_in_path_component() {
        let err = resolve_tag_and_path("./v1.2.3", None);
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("unsupported character"), "got: {msg}");
    }

    #[test]
    fn validate_tag_rejects_space_in_path_component() {
        let err = resolve_tag_and_path("/tmp/my project", None);
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("unsupported character"), "got: {msg}");
    }

    #[test]
    fn validate_tag_rejects_plain_invalid_chars() {
        for bad in &["my.tag", "my@tag", "foo!", "bar+baz"] {
            let err = resolve_tag_and_path(bad, None);
            assert!(err.is_err(), "expected error for tag '{bad}'");
        }
    }

    #[test]
    fn validate_tag_accepts_valid_chars() {
        let (tag, _) = resolve_tag_and_path("my-tag_v2:prod", None).unwrap();
        assert_eq!(tag, "my-tag_v2:prod");
    }
}
