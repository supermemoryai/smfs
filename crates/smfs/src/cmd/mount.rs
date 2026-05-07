//! `smfs mount` — mount a Supermemory container at a local path.
//!
//! In production this is usually invoked indirectly via
//! `supermemory mount <path> <tag>` from the TypeScript CLI, which reads the
//! user's stored credentials and execs this subcommand with `--key`.
//! It can also be used directly for scripting or debugging.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const DEFAULT_STARTUP_INACTIVITY_TIMEOUT_SECS: u64 = 30;

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

    #[arg(long, default_value_t = DEFAULT_STARTUP_INACTIVITY_TIMEOUT_SECS)]
    pub startup_timeout: u64,

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
    let startup_path = smfs_core::daemon::startup_path(&container_tag);
    let _ = std::fs::remove_file(&startup_path);

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

    let mut child = cmd.spawn()?;
    let child_pid = child.id();
    // The child will self-install a session via setsid; parent just waits
    // until the child's IPC socket comes up, then exits.

    let socket = smfs_core::daemon::socket_path(&container_tag);
    let startup_timeout = Duration::from_secs(args.startup_timeout);
    let mut wait_state = StartupWaitState::new(startup_timeout, Instant::now());
    let mut display = StartupDisplay::new(&container_tag);
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
        if let Some((body, progress)) = super::startup::read_progress(&container_tag) {
            let now = Instant::now();
            if progress.pid == child_pid {
                display.observe_progress(&progress, now);
            }
            wait_state.observe_progress(body, progress, now, child_pid);
        }
        display.tick(Instant::now());
        if wait_state.timed_out(Instant::now()) {
            display.finish();
            let _ = child.kill();
            let _ = child.wait();
            let last_progress = wait_state
                .last_progress_summary()
                .unwrap_or_else(|| "<none>".to_string());
            anyhow::bail!(
                "daemon made no startup progress for {}s before becoming ready (pid {}). Log: {}\nLast progress: {}\nLast error: {}",
                args.startup_timeout,
                child_pid,
                log_path.display(),
                last_progress,
                last_err.unwrap_or_else(|| "<none>".into()),
            );
        }
        // Did the child die early?
        if let Some(status) = child.try_wait()? {
            anyhow::bail!(
                "daemon exited before becoming ready (status: {}). Log: {}",
                status,
                log_path.display(),
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    display.finish();
    let _ = std::fs::remove_file(&startup_path);

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

#[derive(Debug)]
struct StartupDisplay {
    tag: String,
    enabled: bool,
    target_loaded: usize,
    total: Option<usize>,
    displayed_loaded: f64,
    last_target_loaded: usize,
    last_target_at: Instant,
    last_tick: Instant,
    rate: f64,
    rendered: bool,
}

impl StartupDisplay {
    fn new(tag: &str) -> Self {
        let now = Instant::now();
        Self {
            tag: tag.to_string(),
            enabled: std::io::stderr().is_terminal(),
            target_loaded: 0,
            total: None,
            displayed_loaded: 0.0,
            last_target_loaded: 0,
            last_target_at: now,
            last_tick: now,
            rate: 0.0,
            rendered: false,
        }
    }

    fn observe_progress(&mut self, progress: &super::startup::StartupProgress, now: Instant) {
        let Some(loaded) = progress.loaded else {
            return;
        };
        if loaded < self.target_loaded {
            return;
        }
        if let Some(total) = progress.total {
            if total > 0 {
                self.total = Some(total);
            }
        }
        if loaded > self.last_target_loaded {
            let elapsed = now.duration_since(self.last_target_at).as_secs_f64();
            if elapsed > 0.0 {
                let instant_rate = (loaded - self.last_target_loaded) as f64 / elapsed;
                self.rate = if self.rate > 0.0 {
                    (self.rate * 0.7) + (instant_rate * 0.3)
                } else {
                    instant_rate
                };
            }
            self.last_target_loaded = loaded;
            self.last_target_at = now;
        }
        self.target_loaded = loaded;
    }

    fn tick(&mut self, now: Instant) {
        if !self.enabled || self.target_loaded == 0 {
            self.last_tick = now;
            return;
        }
        if self.displayed_loaded >= self.target_loaded as f64 {
            self.last_tick = now;
            return;
        }
        let elapsed = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;
        let speed = (self.rate * 0.85).clamp(25.0, 1200.0);
        let next =
            (self.displayed_loaded + (speed * elapsed).max(1.0)).min(self.target_loaded as f64);
        if next.floor() as usize != self.displayed_loaded.floor() as usize {
            self.displayed_loaded = next;
            self.render(false);
        } else {
            self.displayed_loaded = next;
        }
    }

    fn finish(&mut self) {
        if !self.enabled {
            return;
        }
        if self.target_loaded > 0 {
            self.displayed_loaded = self.target_loaded as f64;
            self.render(true);
        } else if self.rendered {
            eprintln!();
        }
    }

    fn render(&mut self, newline: bool) {
        let line = self.render_line();
        eprint!("\r{line}\x1b[K");
        if newline {
            eprintln!();
        }
        let _ = std::io::stderr().flush();
        self.rendered = true;
    }

    fn render_line(&self) -> String {
        let loaded = self.displayed_loaded.floor() as usize;
        if let Some(total) = self.total {
            let display_total = total.max(loaded);
            let pct = loaded
                .saturating_mul(100)
                .checked_div(display_total)
                .unwrap_or(0)
                .min(100);
            if self.rate > 0.0 {
                format!(
                    "syncing {}: {} / {} files loaded ({}%, {:.0} files/s)",
                    self.tag, loaded, display_total, pct, self.rate
                )
            } else {
                format!(
                    "syncing {}: {} / {} files loaded ({}%)",
                    self.tag, loaded, display_total, pct
                )
            }
        } else {
            format!("syncing {}: {} files loaded", self.tag, loaded)
        }
    }
}

#[derive(Debug)]
struct StartupWaitState {
    inactivity_timeout: Duration,
    last_activity: Instant,
    last_progress_body: Option<String>,
    last_progress: Option<super::startup::StartupProgress>,
}

impl StartupWaitState {
    fn new(inactivity_timeout: Duration, now: Instant) -> Self {
        Self {
            inactivity_timeout,
            last_activity: now,
            last_progress_body: None,
            last_progress: None,
        }
    }

    fn observe_progress(
        &mut self,
        body: String,
        progress: super::startup::StartupProgress,
        now: Instant,
        expected_pid: u32,
    ) {
        if progress.pid != expected_pid {
            return;
        }
        if self.last_progress_body.as_deref() != Some(body.as_str()) {
            self.last_activity = now;
            self.last_progress_body = Some(body);
            self.last_progress = Some(progress);
        }
    }

    fn timed_out(&self, now: Instant) -> bool {
        now.duration_since(self.last_activity) >= self.inactivity_timeout
    }

    fn last_progress_summary(&self) -> Option<String> {
        self.last_progress.as_ref().map(|p| {
            if p.message.is_empty() {
                format!("seq={} phase={}", p.seq, p.phase)
            } else {
                format!("seq={} phase={} message={}", p.seq, p.phase, p.message)
            }
        })
    }
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

    #[test]
    fn startup_wait_times_out_without_progress() {
        let now = Instant::now();
        let state = StartupWaitState::new(Duration::from_secs(30), now);

        assert!(!state.timed_out(now + Duration::from_secs(29)));
        assert!(state.timed_out(now + Duration::from_secs(30)));
    }

    #[test]
    fn startup_wait_progress_resets_inactivity_timer() {
        let now = Instant::now();
        let mut state = StartupWaitState::new(Duration::from_secs(30), now);

        state.observe_progress(
            "{\"seq\":1}".to_string(),
            crate::cmd::startup::StartupProgress {
                pid: 42,
                seq: 1,
                phase: "validating_key".to_string(),
                message: "validating API key".to_string(),
                loaded: None,
                total: None,
            },
            now + Duration::from_secs(25),
            42,
        );

        assert!(!state.timed_out(now + Duration::from_secs(54)));
        assert!(state.timed_out(now + Duration::from_secs(55)));
        assert_eq!(
            state.last_progress_summary().as_deref(),
            Some("seq=1 phase=validating_key message=validating API key")
        );
    }

    #[test]
    fn startup_wait_identical_progress_does_not_reset_timer() {
        let now = Instant::now();
        let mut state = StartupWaitState::new(Duration::from_secs(30), now);
        let progress = crate::cmd::startup::StartupProgress {
            pid: 42,
            seq: 1,
            phase: "opening_cache".to_string(),
            message: "opening cache".to_string(),
            loaded: None,
            total: None,
        };

        state.observe_progress(
            "{\"seq\":1}".to_string(),
            progress.clone(),
            now + Duration::from_secs(10),
            42,
        );
        state.observe_progress(
            "{\"seq\":1}".to_string(),
            progress,
            now + Duration::from_secs(35),
            42,
        );

        assert!(state.timed_out(now + Duration::from_secs(40)));
    }

    #[test]
    fn startup_wait_ignores_progress_from_other_pid() {
        let now = Instant::now();
        let mut state = StartupWaitState::new(Duration::from_secs(30), now);

        state.observe_progress(
            "{\"seq\":1}".to_string(),
            crate::cmd::startup::StartupProgress {
                pid: 7,
                seq: 1,
                phase: "initial_sync".to_string(),
                message: "reconciled 100 docs".to_string(),
                loaded: Some(100),
                total: Some(1000),
            },
            now + Duration::from_secs(25),
            42,
        );

        assert!(state.timed_out(now + Duration::from_secs(30)));
        assert!(state.last_progress_summary().is_none());
    }

    #[test]
    fn startup_display_clamps_percent_and_total() {
        let mut display = StartupDisplay::new("eval");
        display.enabled = true;
        display.displayed_loaded = 120.0;
        display.target_loaded = 120;
        display.total = Some(100);

        assert_eq!(
            display.render_line(),
            "syncing eval: 120 / 120 files loaded (100%)"
        );
    }
}
