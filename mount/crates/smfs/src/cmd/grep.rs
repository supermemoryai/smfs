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

    /// Supermemory API key.
    #[arg(long, env = "SUPERMEMORY_API_KEY", hide_env_values = true)]
    pub key: Option<String>,

    /// Override the Supermemory API base URL.
    #[arg(long, env = "SUPERMEMORY_API_URL")]
    pub api_url: Option<String>,
}

/// Read the `.smfs` marker file to get container tag and API URL.
fn read_smfs_marker() -> Option<(String, String)> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let marker = dir.join(".smfs");
        if marker.exists() {
            let content = std::fs::read_to_string(&marker).ok()?;
            let mut tag = None;
            let mut url = None;
            for line in content.lines() {
                if let Some(v) = line.strip_prefix("container_tag=") {
                    tag = Some(v.to_string());
                }
                if let Some(v) = line.strip_prefix("api_url=") {
                    url = Some(v.to_string());
                }
            }
            return Some((tag?, url.unwrap_or_else(|| "https://api.supermemory.ai".to_string())));
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

pub async fn run(args: Args) -> Result<()> {
    // Resolve container tag + API URL.
    let (tag, api_url) = if let Some(tag) = &args.tag {
        let url = args
            .api_url
            .clone()
            .unwrap_or_else(|| "https://api.supermemory.ai".to_string());
        (tag.clone(), url)
    } else if let Some((tag, url)) = read_smfs_marker() {
        let url = args.api_url.clone().unwrap_or(url);
        (tag, url)
    } else {
        anyhow::bail!(
            "No container tag found. Either run from inside a mounted directory or pass --tag."
        );
    };

    let key = args.key.as_deref().ok_or_else(|| {
        anyhow::anyhow!("API key required. Pass --key or set SUPERMEMORY_API_KEY.")
    })?;

    let client = smfs_core::api::ApiClient::new(&api_url, key, &tag);

    // Determine filepath prefix from path arg.
    let filepath = args.path.as_deref().map(|p| {
        if p.starts_with('/') {
            p.to_string()
        } else {
            format!("/{p}")
        }
    });
    let filepath = filepath.as_deref().map(|p| {
        if !p.ends_with('/') && Path::new(p).extension().is_none() {
            format!("{p}/")
        } else {
            p.to_string()
        }
    });

    let resp = client
        .search(&args.query, filepath.as_deref())
        .await?;

    if resp.results.is_empty() {
        eprintln!("[supermemory] No results for {:?}", args.query);
        eprintln!("[supermemory] Use grep -F for exact string matching.");
        return Ok(());
    }

    eprintln!(
        "[supermemory] Semantic search for {:?} ({} results)\n",
        args.query,
        resp.results.len()
    );

    for result in &resp.results {
        let fp = result.filepath.as_deref().unwrap_or("(unknown)");
        let content = result
            .memory
            .as_deref()
            .or(result.chunk.as_deref())
            .unwrap_or("");

        // Truncate long content to first line or 200 chars.
        let preview = content.lines().next().unwrap_or(content);
        let preview = if preview.len() > 200 {
            &preview[..200]
        } else {
            preview
        };

        println!("{}:  {}", fp, preview);
    }

    eprintln!("\n[supermemory] Use grep -F for exact string matching.");

    Ok(())
}
