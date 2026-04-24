use anyhow::Result;
use std::path::Path;

#[derive(Debug, Clone)]
pub enum KeySource {
    ExplicitFlag,
    ProjectCredentials,
    GlobalCredentials,
    EnvVar,
}

impl KeySource {
    pub fn label(&self) -> &'static str {
        match self {
            KeySource::ExplicitFlag => "--key flag",
            KeySource::ProjectCredentials => "project credentials",
            KeySource::GlobalCredentials => "global credentials",
            KeySource::EnvVar => "SUPERMEMORY_API_KEY env",
        }
    }
}

/// Resolve API key with priority:
/// 1. explicit `--key` flag
/// 2. project-level stored credentials
/// 3. global stored credentials
/// 4. `SUPERMEMORY_API_KEY` env var
pub fn resolve_api_key(explicit_key: Option<&str>, mount_path: Option<&Path>) -> Result<String> {
    resolve_api_key_with_source(explicit_key, mount_path).map(|(k, _)| k)
}

pub fn resolve_api_key_with_source(
    explicit_key: Option<&str>,
    mount_path: Option<&Path>,
) -> Result<(String, KeySource)> {
    if let Some(k) = explicit_key {
        return Ok((k.to_string(), KeySource::ExplicitFlag));
    }
    if let Some(p) = mount_path {
        if let Some(creds) = smfs_core::config::credentials::load_project(p) {
            return Ok((creds.api_key, KeySource::ProjectCredentials));
        }
    }
    if let Some(creds) = smfs_core::config::credentials::load_global() {
        return Ok((creds.api_key, KeySource::GlobalCredentials));
    }
    if let Ok(k) = std::env::var("SUPERMEMORY_API_KEY") {
        if !k.is_empty() {
            return Ok((k, KeySource::EnvVar));
        }
    }
    anyhow::bail!("API key required. Run `smfs login` or pass --key.")
}
