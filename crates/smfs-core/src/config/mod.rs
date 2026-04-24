//! Configuration and XDG paths.
//!
//! Resolves cache database location, log file paths, and IPC socket paths
//! per operating system. Uses the `directories` crate so we don't branch
//! on OS manually.

pub mod credentials;

use std::path::PathBuf;

/// Return the platform-appropriate cache directory for supermemoryfs.
///
/// - Linux: `$XDG_CACHE_HOME/supermemoryfs` (usually `~/.cache/supermemoryfs`)
/// - macOS: `~/Library/Caches/supermemoryfs`
pub fn cache_dir() -> PathBuf {
    directories::ProjectDirs::from("ai", "supermemory", "supermemoryfs")
        .map(|d| d.cache_dir().to_path_buf())
        .unwrap_or_else(|| {
            // Fallback if home directory can't be determined.
            PathBuf::from("/tmp/supermemoryfs")
        })
}

pub fn cache_db_path(org_id: &str, container_tag: &str) -> PathBuf {
    cache_dir().join(org_id).join(format!("{container_tag}.db"))
}

pub fn legacy_cache_db_path(container_tag: &str) -> PathBuf {
    cache_dir().join(format!("{container_tag}.db"))
}

/// Path to the daemon log file.
pub fn daemon_log_path() -> PathBuf {
    cache_dir().join("daemon.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_db_path_contains_org_and_tag() {
        let path = cache_db_path("org123", "my-tag");
        let s = path.to_str().unwrap();
        assert!(s.contains("org123"));
        assert!(s.contains("my-tag.db"));
    }

    #[test]
    fn cache_db_path_different_orgs_differ_for_same_tag() {
        assert_ne!(cache_db_path("orgA", "work"), cache_db_path("orgB", "work"));
    }

    #[test]
    fn cache_db_path_different_tags_differ() {
        assert_ne!(cache_db_path("org", "a"), cache_db_path("org", "b"));
    }
}
