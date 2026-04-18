//! `smfs grep` — semantic search across a mounted container.

use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::Path;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Search query.
    pub query: String,

    /// Directory path to scope the search (optional).
    pub path: Option<String>,

    /// Container tag (auto-detected from .smfs marker if not given).
    #[arg(long)]
    pub tag: Option<String>,

    /// Supermemory API key (resolved from stored credentials if omitted).
    #[arg(long)]
    pub key: Option<String>,

    /// Override the Supermemory API base URL.
    #[arg(long, env = "SUPERMEMORY_API_URL")]
    pub api_url: Option<String>,
}

pub async fn run(args: Args) -> Result<()> {
    use super::marker::read_smfs_marker;

    if args.query.trim().is_empty() {
        eprintln!("# supermemory semantic search — provide a query");
        eprintln!("# inside a mounted container, `grep` without flags is powered by semantic search.");
        eprintln!("# usage:");
        eprintln!("#   grep \"natural language query\"         search by meaning, all files");
        eprintln!("#   grep \"query\" path/to/dir/             scope to a directory");
        eprintln!("#   grep -F \"exact string\" path/to/file   exact match (bypasses semantic)");
        return Ok(());
    }

    let marker = read_smfs_marker();

    // Resolve container tag + API URL.
    let (tag, api_url) = if let Some(tag) = &args.tag {
        let url = args
            .api_url
            .clone()
            .unwrap_or_else(|| "https://api.supermemory.ai".to_string());
        (tag.clone(), url)
    } else if let Some(ref m) = marker {
        let url = args.api_url.clone().unwrap_or_else(|| m.api_url.clone());
        (m.tag.clone(), url)
    } else {
        anyhow::bail!(
            "No container tag found. Either run from inside a mounted directory or pass --tag."
        );
    };

    let mount_path = marker
        .as_ref()
        .and_then(|m| m.mount_path.as_deref())
        .map(std::path::Path::new);
    let key = super::auth::resolve_api_key(args.key.as_deref(), mount_path)?;

    let client = smfs_core::api::ApiClient::new(&api_url, &key, &tag);

    // Determine filepath prefix from path arg, stripping the mount path if present.
    // Canonicalize the mount path from the marker. If no marker was found from CWD,
    // try resolving one from the path argument itself (handles calling from outside).
    let canonical_mount = mount_path
        .and_then(|mp| mp.canonicalize().ok())
        .or_else(|| {
            // If path arg is given, walk up from it to find .smfs marker.
            args.path.as_deref().and_then(|p| {
                let target = if p.starts_with('/') {
                    Path::new(p).to_path_buf()
                } else {
                    std::env::current_dir().ok()?.join(p)
                };
                let target = target.canonicalize().ok()?;
                let mut dir = if target.is_dir() {
                    target
                } else {
                    target.parent()?.to_path_buf()
                };
                loop {
                    if dir.join(".smfs").exists() {
                        // Read mount_path from the marker
                        let content = std::fs::read_to_string(dir.join(".smfs")).ok()?;
                        for line in content.lines() {
                            if let Some(v) = line.strip_prefix("mount_path=") {
                                return Path::new(v).canonicalize().ok();
                            }
                        }
                        return Some(dir);
                    }
                    if !dir.pop() {
                        break;
                    }
                }
                None
            })
        });

    let filepath = args.path.as_deref().and_then(|p| {
        let raw = if p.starts_with('/') {
            Path::new(p).to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(p))
                .unwrap_or_else(|_| Path::new(p).to_path_buf())
        };
        let abs = raw
            .canonicalize()
            .unwrap_or(raw)
            .to_string_lossy()
            .into_owned();

        let relative = if let Some(ref cm) = canonical_mount {
            let cm_str = cm.to_string_lossy();
            abs.strip_prefix(cm_str.as_ref())
                .map(|s| s.to_string())
                .unwrap_or(abs)
        } else {
            abs
        };

        if relative.is_empty() || relative == "/" {
            return None;
        }

        let relative = if relative.starts_with('/') {
            relative
        } else {
            format!("/{relative}")
        };

        let relative = if !relative.ends_with('/') && Path::new(&relative).extension().is_none() {
            format!("{relative}/")
        } else {
            relative
        };

        Some(relative)
    });

    let resp = client.search(&args.query, filepath.as_deref()).await?;

    if resp.results.is_empty() {
        eprintln!(
            "# supermemory semantic search — no results for {:?}",
            args.query
        );
        eprintln!("# this searches by meaning, not exact text. try a natural language query.");
        eprintln!("# for exact string matching: grep -F \"pattern\" <path>");
        return Ok(());
    }

    // Header: tells LLMs and users what this output is and how to use it.
    eprintln!(
        "# supermemory semantic search — {} results for {:?}",
        resp.results.len(),
        args.query
    );
    eprintln!("# searches by meaning across files in this container. usage:");
    eprintln!("#   grep \"natural language query\"          search all files");
    eprintln!("#   grep \"query\" path/to/dir/              search within directory");
    eprintln!(
        "#   grep -F \"exact string\" path/to/dir/    exact match (bypasses semantic search)"
    );
    eprintln!();

    for result in &resp.results {
        let fp = result.filepath.as_deref().unwrap_or("(unknown)");
        let content = result
            .memory
            .as_deref()
            .or(result.chunk.as_deref())
            .unwrap_or("");

        let preview = content.lines().next().unwrap_or(content);
        let preview = if preview.len() > 200 {
            &preview[..200]
        } else {
            preview
        };

        println!("{}:  {}", fp, preview);
    }

    Ok(())
}
