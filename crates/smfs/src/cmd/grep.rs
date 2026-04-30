//! `smfs grep` — semantic search across a mounted container.

use anyhow::Result;
use clap::Args as ClapArgs;
use std::collections::HashMap;
use std::path::Path;

fn read_local_or_sidecar(mount: &Path, filepath: &str) -> Option<String> {
    let stripped = filepath.trim_start_matches('/');
    let local = mount.join(stripped);
    if let Ok(c) = std::fs::read_to_string(&local) {
        return Some(c);
    }
    for suffix in &[
        ".pdf-transcription.md",
        ".image-transcription.md",
        ".video-transcription.md",
        ".audio-transcription.md",
        ".webpage-transcription.md",
    ] {
        let sidecar = mount.join(format!("{stripped}{suffix}"));
        if let Ok(c) = std::fs::read_to_string(&sidecar) {
            return Some(c);
        }
    }
    None
}

fn line_range_in_file(file_content: &str, chunk: &str) -> Option<(usize, usize)> {
    if chunk.is_empty() {
        return None;
    }

    if let Some(pos) = file_content.find(chunk) {
        let start = file_content[..pos].matches('\n').count() + 1;
        let last_char_len = chunk.chars().next_back()?.len_utf8();
        let last_char_start = pos + chunk.len() - last_char_len;
        let end = file_content[..last_char_start].matches('\n').count() + 1;
        return Some((start, end));
    }

    let norm = |s: &str| -> String { s.split_whitespace().collect::<Vec<_>>().join(" ") };
    let normed_file = norm(file_content);
    let normed_chunk = norm(chunk);
    if normed_chunk.is_empty() {
        return None;
    }
    let norm_pos_byte = normed_file.find(&normed_chunk)?;
    let target_start = normed_file[..norm_pos_byte].chars().count();
    let normed_chunk_chars = normed_chunk.chars().count();
    let target_end_inclusive = target_start + normed_chunk_chars - 1;

    let mut orig_start_byte: Option<usize> = None;
    let mut orig_end_byte: Option<usize> = None;
    let mut norm_idx: usize = 0;
    let mut need_separator = false;
    for (i, ch) in file_content.char_indices() {
        if ch.is_whitespace() {
            if norm_idx > 0 {
                need_separator = true;
            }
            continue;
        }
        if need_separator {
            norm_idx += 1;
            need_separator = false;
        }
        if norm_idx == target_start && orig_start_byte.is_none() {
            orig_start_byte = Some(i);
        }
        if norm_idx == target_end_inclusive {
            orig_end_byte = Some(i);
            break;
        }
        norm_idx += 1;
    }

    let start_byte = orig_start_byte?;
    let end_byte = orig_end_byte?;
    let start = file_content[..start_byte].matches('\n').count() + 1;
    let end = file_content[..end_byte].matches('\n').count() + 1;
    Some((start, end))
}

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
        eprintln!(
            "# inside a mounted container, `grep` without flags is powered by semantic search."
        );
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
            args.path.as_deref().and_then(|p| {
                let target = if p.starts_with('/') {
                    Path::new(p).to_path_buf()
                } else {
                    std::env::current_dir().ok()?.join(p)
                };
                let target = target.canonicalize().ok()?;
                let search_from = if target.is_dir() {
                    target
                } else {
                    target.parent()?.to_path_buf()
                };
                let m = super::marker::read_smfs_marker_for_path(&search_from)?;
                m.mount_path
                    .as_deref()
                    .and_then(|mp| Path::new(mp).canonicalize().ok())
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

    let mut file_cache: HashMap<String, Option<String>> = HashMap::new();

    for (i, result) in resp.results.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let fp = result.filepath.as_deref().unwrap_or("(unknown)");

        if let Some(memory) = result.memory.as_deref() {
            let escaped = memory
                .replace('\\', "\\\\")
                .replace('\n', "\\n")
                .replace('\r', "\\r");
            println!("{}:{}", fp, escaped);
            continue;
        }

        let chunk = result.chunk.as_deref().unwrap_or("");
        let escaped = chunk
            .replace('\\', "\\\\")
            .replace('\n', "\\n")
            .replace('\r', "\\r");

        let line_range = canonical_mount
            .as_ref()
            .zip(result.filepath.as_deref())
            .and_then(|(cm, path)| {
                let content = file_cache
                    .entry(path.to_string())
                    .or_insert_with(|| read_local_or_sidecar(cm, path))
                    .as_deref()?;
                line_range_in_file(content, chunk)
            });

        if let Some((start, end)) = line_range {
            if start == end {
                println!("{}:{}:{}", fp, start, escaped);
            } else {
                println!("{}:{}-{}:{}", fp, start, end, escaped);
            }
        } else {
            println!("{}:{}", fp, escaped);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::line_range_in_file;

    #[test]
    fn verbatim_single_line() {
        let file = "alpha\nbeta\ngamma\n";
        assert_eq!(line_range_in_file(file, "beta"), Some((2, 2)));
    }

    #[test]
    fn verbatim_multiline_chunk() {
        let file = "alpha\nbeta\ngamma\ndelta\n";
        assert_eq!(line_range_in_file(file, "beta\ngamma"), Some((2, 3)));
    }

    #[test]
    fn first_line_match() {
        let file = "alpha\nbeta\n";
        assert_eq!(line_range_in_file(file, "alpha"), Some((1, 1)));
    }

    #[test]
    fn empty_chunk_returns_none() {
        assert_eq!(line_range_in_file("anything", ""), None);
    }

    #[test]
    fn no_match_returns_none() {
        assert_eq!(line_range_in_file("alpha\nbeta\n", "missing"), None);
    }

    #[test]
    fn verbatim_chunk_ending_in_multibyte_char() {
        let file = "alpha\nnaï\ngamma\n";
        assert_eq!(line_range_in_file(file, "naï"), Some((2, 2)));
    }

    #[test]
    fn verbatim_match_across_blank_line() {
        let file = "abc\n\ndef\n";
        assert_eq!(line_range_in_file(file, "def"), Some((3, 3)));
    }

    #[test]
    fn whitespace_normalized_match_across_blank_line() {
        let file = "abc\n\ndef\n";
        assert_eq!(line_range_in_file(file, "abc def"), Some((1, 3)));
    }

    #[test]
    fn whitespace_normalized_with_leading_whitespace() {
        let file = "  hello world\n";
        assert_eq!(line_range_in_file(file, "hello   world"), Some((1, 1)));
    }

    #[test]
    fn whitespace_normalized_chunk_spans_lines() {
        let file = "intro\n\nalpha beta\ngamma delta\nepsilon\n";
        assert_eq!(
            line_range_in_file(file, "alpha   beta\n\ngamma   delta"),
            Some((3, 4))
        );
    }
}
