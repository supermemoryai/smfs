//! `smfs init` — install the grep shell wrapper.

use anyhow::Result;
use clap::Args as ClapArgs;

#[derive(ClapArgs, Debug)]
pub struct Args {}

const SHELL_WRAPPER: &str = r#"
# supermemoryfs grep wrapper — semantic search inside mounted containers
grep() {
    for arg in "$@"; do
        case "$arg" in
            -*) command grep "$@"; return ;;
        esac
    done
    # Check if CWD or the path argument is inside a supermemory mount.
    _smfs_found=""
    # First: walk up from CWD.
    _smfs_dir="$PWD"
    while [ "$_smfs_dir" != "/" ]; do
        if [ -f "$_smfs_dir/.smfs" ]; then
            _smfs_mp=$(grep '^mount_path=' "$_smfs_dir/.smfs" 2>/dev/null | cut -d= -f2-)
            # Only trigger if CWD is the mount dir or inside it.
            case "$PWD" in "$_smfs_mp"|"$_smfs_mp"/*) _smfs_found=1 ;; esac
            break
        fi
        _smfs_dir="$(dirname "$_smfs_dir")"
    done
    # Second: if not found via CWD, check if a path argument points into a mount.
    if [ -z "$_smfs_found" ]; then
        for arg in "$@"; do
            case "$arg" in -*) continue ;; esac
            # Resolve the path argument.
            if [ -d "$arg" ]; then
                _smfs_dir="$arg"
            elif [ -d "$(dirname "$arg")" ]; then
                _smfs_dir="$(dirname "$arg")"
            else
                continue
            fi
            _smfs_dir="$(cd "$_smfs_dir" 2>/dev/null && pwd -P)" || continue
            while [ "$_smfs_dir" != "/" ]; do
                if [ -f "$_smfs_dir/.smfs" ]; then
                    _smfs_found=1
                    break 2
                fi
                _smfs_dir="$(dirname "$_smfs_dir")"
            done
        done
    fi
    if [ -n "$_smfs_found" ]; then
        smfs grep "$@"
    else
        command grep "$@"
    fi
}
"#;

const MARKER: &str = "supermemoryfs grep wrapper";

/// Install or update the grep wrapper in ~/.zshrc.
/// Returns true if installed or updated, false if already up to date.
pub fn ensure_grep_wrapper_installed() -> Result<bool> {
    let home = std::env::var("HOME").map(std::path::PathBuf::from)?;
    let zshrc = home.join(".zshrc");

    if let Ok(content) = std::fs::read_to_string(&zshrc) {
        if content.contains(MARKER) {
            if content.contains(SHELL_WRAPPER.trim()) {
                return Ok(false); // already up to date
            }
            // Old version exists — remove it and re-add below.
            let mut cleaned = String::new();
            let mut skip = false;
            for line in content.lines() {
                if line.contains(MARKER) {
                    skip = true;
                    continue;
                }
                if skip && line.trim() == "}" {
                    skip = false;
                    continue;
                }
                if !skip {
                    cleaned.push_str(line);
                    cleaned.push('\n');
                }
            }
            std::fs::write(&zshrc, cleaned)?;
        }
    }

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&zshrc)?;
    file.write_all(SHELL_WRAPPER.as_bytes())?;

    Ok(true)
}

pub async fn run(_args: Args) -> Result<()> {
    if ensure_grep_wrapper_installed()? {
        eprintln!("semantic grep installed. run: source ~/.zshrc");
    } else {
        eprintln!("semantic grep already installed.");
    }
    Ok(())
}
