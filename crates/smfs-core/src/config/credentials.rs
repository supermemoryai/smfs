use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub api_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
}

fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("ai", "supermemory", "supermemoryfs")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/tmp/supermemoryfs-config"))
}

fn global_path() -> PathBuf {
    config_dir().join("credentials.json")
}

fn projects_dir() -> PathBuf {
    config_dir().join("projects")
}

fn encode_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    let encoded = s.replace('/', "-");
    encoded.strip_prefix('-').unwrap_or(&encoded).to_string()
}

fn project_path(mount_path: &Path) -> PathBuf {
    projects_dir().join(format!("{}.json", encode_path(mount_path)))
}

fn write_json(path: &Path, creds: &Credentials) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(creds)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn load_json(path: &Path) -> Option<Credentials> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save_global(creds: &Credentials) -> Result<()> {
    write_json(&global_path(), creds)
}

pub fn save_project(mount_path: &Path, creds: &Credentials) -> Result<()> {
    write_json(&project_path(mount_path), creds)
}

pub fn load_project(mount_path: &Path) -> Option<Credentials> {
    load_json(&project_path(mount_path))
}

pub fn load_global() -> Option<Credentials> {
    load_json(&global_path())
}

pub fn resolve(mount_path: Option<&Path>) -> Option<Credentials> {
    if let Some(p) = mount_path {
        if let Some(c) = load_json(&project_path(p)) {
            return Some(c);
        }
    }
    load_json(&global_path())
}

pub fn remove_global() -> Result<()> {
    match std::fs::remove_file(global_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn remove_project(mount_path: &Path) -> Result<()> {
    match std::fs::remove_file(project_path(mount_path)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn remove_all_projects() -> Result<()> {
    let dir = projects_dir();
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_path_strips_leading_dash() {
        assert_eq!(encode_path(Path::new("/Users/me/code")), "Users-me-code");
    }

    #[test]
    fn encode_path_relative() {
        assert_eq!(encode_path(Path::new("foo/bar")), "foo-bar");
    }
}
