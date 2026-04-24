//! Shared `.smfs` marker file reader.

pub struct SmfsMarker {
    pub tag: String,
    pub api_url: String,
    pub mount_path: Option<String>,
}

pub fn read_smfs_marker() -> Option<SmfsMarker> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let marker = dir.join(".smfs");
        if marker.exists() {
            let content = std::fs::read_to_string(&marker).ok()?;
            let mut tag = None;
            let mut url = None;
            let mut mount_path = None;
            for line in content.lines() {
                if let Some(v) = line.strip_prefix("container_tag=") {
                    tag = Some(v.to_string());
                }
                if let Some(v) = line.strip_prefix("api_url=") {
                    url = Some(v.to_string());
                }
                if let Some(v) = line.strip_prefix("mount_path=") {
                    mount_path = Some(v.to_string());
                }
            }
            return Some(SmfsMarker {
                tag: tag?,
                api_url: url.unwrap_or_else(|| "https://api.supermemory.ai".to_string()),
                mount_path,
            });
        }
        if !dir.pop() {
            break;
        }
    }
    None
}
