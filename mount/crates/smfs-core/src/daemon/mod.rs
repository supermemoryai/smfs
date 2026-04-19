//! Daemon lifecycle + IPC.
//!
//! The `smfs` binary can run as either a short-lived CLI invocation (e.g.
//! `smfs list`) or as a long-lived background daemon that owns an active
//! mount. This module is what the daemon side uses internally — the
//! `smfs` crate wires subcommands to [`client::send_request`] for the CLI
//! side and spawns [`ipc::serve`] inside the daemon body.
//!
//! ## Directory layout
//!
//! ```text
//! <cache_dir>/
//! ├── <tag>.db                  # SQLite cache (already existed pre-M9)
//! ├── sockets/<tag>.sock        # Unix IPC socket
//! ├── pids/<tag>.pid            # daemon pid
//! └── logs/<tag>.log            # per-tag rolling log
//! ```

pub mod client;
pub mod ipc;
pub mod protocol;

use std::path::PathBuf;

use crate::config::cache_dir;

pub fn sockets_dir() -> PathBuf {
    cache_dir().join("sockets")
}
pub fn pids_dir() -> PathBuf {
    cache_dir().join("pids")
}
pub fn logs_dir() -> PathBuf {
    cache_dir().join("logs")
}

pub fn socket_path(tag: &str) -> PathBuf {
    sockets_dir().join(format!("{tag}.sock"))
}
pub fn pid_path(tag: &str) -> PathBuf {
    pids_dir().join(format!("{tag}.pid"))
}
pub fn log_path(tag: &str) -> PathBuf {
    logs_dir().join(format!("{tag}.log"))
}

/// Create `sockets/`, `pids/`, `logs/` subdirectories if missing.
pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(sockets_dir())?;
    std::fs::create_dir_all(pids_dir())?;
    std::fs::create_dir_all(logs_dir())?;
    Ok(())
}

/// Is the given pid alive? POSIX: shells out to `kill -0 <pid>`, which
/// does not actually send a signal — it's just a liveness probe that
/// succeeds iff the process exists and we're permitted to signal it.
/// Using a subprocess avoids needing `unsafe` (the crate is
/// `#![forbid(unsafe_code)]`).
pub fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Read the pid from a per-tag pid file, or return None.
pub fn read_pid(tag: &str) -> Option<u32> {
    std::fs::read_to_string(pid_path(tag))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Remove leftover socket/pid files from a previous run whose daemon
/// isn't alive anymore. Returns true if anything was cleaned.
pub fn cleanup_stale(tag: &str) -> bool {
    let mut cleaned = false;
    match read_pid(tag) {
        Some(pid) if !pid_alive(pid) => {
            let _ = std::fs::remove_file(pid_path(tag));
            let _ = std::fs::remove_file(socket_path(tag));
            cleaned = true;
        }
        None => {
            if socket_path(tag).exists() {
                let _ = std::fs::remove_file(socket_path(tag));
                cleaned = true;
            }
        }
        _ => {}
    }
    cleaned
}
