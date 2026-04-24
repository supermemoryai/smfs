//! `smfs install` — self-install the binary to `~/.local/bin` and print PATH hint.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[arg(hide = true)]
    pub version: Option<String>,
}

pub async fn run(_args: Args) -> Result<()> {
    let install_dir = resolve_install_dir()?;
    fs::create_dir_all(&install_dir)
        .with_context(|| format!("failed to create {}", install_dir.display()))?;

    let target = install_dir.join("smfs");
    let source = env::current_exe().context("failed to locate current executable")?;

    let source_canon = fs::canonicalize(&source).ok();
    let target_canon = fs::canonicalize(&target).ok();
    let same_file = source_canon.is_some() && source_canon == target_canon;

    if !same_file {
        if target.exists() {
            fs::remove_file(&target)
                .with_context(|| format!("failed to remove existing {}", target.display()))?;
        }
        fs::copy(&source, &target)
            .with_context(|| format!("failed to copy to {}", target.display()))?;
        let mut perms = fs::metadata(&target)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&target, perms)?;
    }

    eprintln!(
        "smfs {} installed to {}",
        env!("CARGO_PKG_VERSION"),
        target.display()
    );

    if !dir_on_path(&install_dir) {
        print_path_hint(&install_dir);
    }

    Ok(())
}

fn resolve_install_dir() -> Result<PathBuf> {
    if let Ok(custom) = env::var("SMFS_INSTALL_DIR") {
        if !custom.is_empty() {
            return Ok(PathBuf::from(custom));
        }
    }
    let home = env::var("HOME").context("$HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/bin"))
}

fn dir_on_path(dir: &Path) -> bool {
    let Ok(path) = env::var("PATH") else {
        return false;
    };
    let dir_canon = fs::canonicalize(dir).ok();
    for entry in path.split(':') {
        if entry.is_empty() {
            continue;
        }
        let entry_path = PathBuf::from(entry);
        if entry_path == dir {
            return true;
        }
        if let (Some(a), Some(b)) = (dir_canon.as_ref(), fs::canonicalize(&entry_path).ok()) {
            if *a == b {
                return true;
            }
        }
    }
    false
}

fn print_path_hint(dir: &Path) {
    let shell = env::var("SHELL").unwrap_or_default();
    let dir_str = dir.display().to_string();

    let (rc_path, line) = if shell.ends_with("/fish") {
        (
            "~/.config/fish/config.fish".to_string(),
            format!("fish_add_path {dir_str}"),
        )
    } else if shell.ends_with("/zsh") {
        (
            "~/.zshrc".to_string(),
            format!(r#"export PATH="{dir_str}:$PATH""#),
        )
    } else if shell.ends_with("/bash") {
        let rc = if cfg!(target_os = "macos") {
            "~/.bash_profile"
        } else {
            "~/.bashrc"
        };
        (rc.to_string(), format!(r#"export PATH="{dir_str}:$PATH""#))
    } else {
        (
            "your shell's startup file".to_string(),
            format!(r#"export PATH="{dir_str}:$PATH""#),
        )
    };

    eprintln!();
    eprintln!("{dir_str} is not on your PATH.");
    eprintln!("Add this line to {rc_path} and restart your shell:");
    eprintln!();
    eprintln!("    {line}");
    eprintln!();
}
