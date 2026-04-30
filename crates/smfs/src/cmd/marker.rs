pub struct SmfsMarker {
    pub tag: String,
    pub api_url: String,
    pub mount_path: Option<String>,
}

pub fn parse_all_markers(content: &str) -> Vec<SmfsMarker> {
    content
        .split("\n\n")
        .filter_map(parse_one_marker)
        .collect()
}

fn parse_one_marker(block: &str) -> Option<SmfsMarker> {
    let mut tag = None;
    let mut url = None;
    let mut mount_path = None;
    for line in block.lines() {
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
    Some(SmfsMarker {
        tag: tag?,
        api_url: url.unwrap_or_else(|| "https://api.supermemory.ai".to_string()),
        mount_path,
    })
}

fn select_for_path<'a>(markers: &'a [SmfsMarker], target: &std::path::Path) -> Option<&'a SmfsMarker> {
    markers.iter().find(|m| {
        m.mount_path
            .as_deref()
            .and_then(|mp| std::fs::canonicalize(mp).ok())
            .map(|mp| target.starts_with(&mp))
            .unwrap_or(false)
    })
}

pub fn format_marker(m: &SmfsMarker) -> String {
    format!(
        "container_tag={}\napi_url={}\nmount_path={}\n",
        m.tag,
        m.api_url,
        m.mount_path.as_deref().unwrap_or("")
    )
}

pub fn read_smfs_marker() -> Option<SmfsMarker> {
    let cwd = std::env::current_dir().ok()?;
    read_smfs_marker_for_path(&cwd)
}

pub fn read_smfs_marker_for_path(start: &std::path::Path) -> Option<SmfsMarker> {
    let mut dir = start.to_path_buf();
    loop {
        let marker_file = dir.join(".smfs");
        if marker_file.exists() {
            let content = std::fs::read_to_string(&marker_file).ok()?;
            let markers = parse_all_markers(&content);
            if let Some(m) = select_for_path(&markers, start) {
                return Some(SmfsMarker {
                    tag: m.tag.clone(),
                    api_url: m.api_url.clone(),
                    mount_path: m.mount_path.clone(),
                });
            }
            return markers.into_iter().next();
        }
        if !dir.pop() {
            break;
        }
    }
    None
}
