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

    let resp = client.search(&args.query, filepath.as_deref()).await?;

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
