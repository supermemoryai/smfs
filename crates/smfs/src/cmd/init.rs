//! `smfs init` — install the grep shell wrapper.

use anyhow::Result;
use clap::Args as ClapArgs;

#[derive(ClapArgs, Debug)]
pub struct Args {}

const SHELL_WRAPPER: &str = r#"
# supermemoryfs grep wrapper — semantic search inside mounted containers
grep() {
    # Any flag → real grep; semantic doesn't know about flags.
    for arg in "$@"; do
        case "$arg" in
            -*) command grep "$@"; return ;;
        esac
    done

    _smfs_found=""

    # Path A: CWD walk. Trigger if $PWD is actually inside a mount path.
    _smfs_dir="$PWD"
    _smfs_pwd_real="$(pwd -P)"
    while [ "$_smfs_dir" != "/" ]; do
        if [ -f "$_smfs_dir/.smfs" ]; then
            _smfs_mp=$(command grep '^mount_path=' "$_smfs_dir/.smfs" 2>/dev/null | cut -d= -f2-)
            _smfs_mp_real="$(cd "$_smfs_mp" 2>/dev/null && pwd -P)"
            case "$_smfs_pwd_real" in "$_smfs_mp_real"|"$_smfs_mp_real"/*) _smfs_found=1 ;; esac
            break
        fi
        _smfs_dir="$(dirname "$_smfs_dir")"
    done

    # Path B: check path args (skip the first non-flag arg — it's grep's pattern).
    # A match only counts when the resolved path is actually inside the mount.
    if [ -z "$_smfs_found" ]; then
        _smfs_pattern_seen=0
        for arg in "$@"; do
            case "$arg" in -*) continue ;; esac
            if [ "$_smfs_pattern_seen" = "0" ]; then
                _smfs_pattern_seen=1
                continue
            fi
            if [ -d "$arg" ]; then
                _smfs_resolved="$(cd "$arg" 2>/dev/null && pwd -P)"
            elif [ -e "$arg" ] || [ -d "$(dirname "$arg")" ]; then
                _smfs_parent="$(cd "$(dirname "$arg")" 2>/dev/null && pwd -P)"
                [ -z "$_smfs_parent" ] && continue
                _smfs_resolved="$_smfs_parent/$(basename "$arg")"
            else
                continue
            fi
            [ -z "$_smfs_resolved" ] && continue
            _smfs_dir="$_smfs_resolved"
            [ ! -d "$_smfs_dir" ] && _smfs_dir="$(dirname "$_smfs_dir")"
            while [ "$_smfs_dir" != "/" ]; do
                if [ -f "$_smfs_dir/.smfs" ]; then
                    _smfs_mp=$(command grep '^mount_path=' "$_smfs_dir/.smfs" 2>/dev/null | cut -d= -f2-)
                    _smfs_mp_real="$(cd "$_smfs_mp" 2>/dev/null && pwd -P)"
                    case "$_smfs_resolved" in
                        "$_smfs_mp_real"|"$_smfs_mp_real"/*) _smfs_found=1; break 2 ;;
                    esac
                    break
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

/// Append the wrapper to ~/.zshrc if no marker is present. No-op if any
/// wrapper block already exists — this is the cheap path used by `mount`.
/// Returns true when a fresh install happened.
pub fn ensure_grep_wrapper_present() -> Result<bool> {
    let home = std::env::var("HOME").map(std::path::PathBuf::from)?;
    let zshrc = home.join(".zshrc");

    if let Ok(content) = std::fs::read_to_string(&zshrc) {
        if content.contains(MARKER) {
            return Ok(false);
        }
    }

    append_wrapper(&zshrc)?;
    Ok(true)
}

/// Strip any existing wrapper block and append a fresh copy. This is the
/// force path used by `smfs init` — run it after upgrading the binary so
/// the shell integration matches the current version.
pub fn reinstall_grep_wrapper() -> Result<()> {
    let home = std::env::var("HOME").map(std::path::PathBuf::from)?;
    let zshrc = home.join(".zshrc");

    if let Ok(content) = std::fs::read_to_string(&zshrc) {
        if content.contains(MARKER) {
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

    append_wrapper(&zshrc)?;
    Ok(())
}

fn append_wrapper(zshrc: &std::path::Path) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(zshrc)?;
    file.write_all(SHELL_WRAPPER.as_bytes())?;
    Ok(())
}

pub async fn run(_args: Args) -> Result<()> {
    reinstall_grep_wrapper()?;
    use std::io::IsTerminal;
    let color = std::io::stderr().is_terminal();
    let cmd = if color {
        "\x1b[1;36msource ~/.zshrc\x1b[0m"
    } else {
        "source ~/.zshrc"
    };
    eprintln!("semantic grep (re)installed.");
    eprintln!("run: {cmd}");
    Ok(())
}
