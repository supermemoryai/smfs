//! `smfs logout` — remove stored credentials.

use anyhow::Result;
use clap::Args as ClapArgs;
use smfs_core::config::credentials;
use std::path::PathBuf;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Only remove the current project's credential (detected from .smfs marker or cwd).
    #[arg(long)]
    pub project: bool,
}

pub async fn run(args: Args) -> Result<()> {
    if args.project {
        let mount_path = super::marker::read_smfs_marker()
            .and_then(|m| m.mount_path.map(PathBuf::from))
            .unwrap_or_else(|| std::env::current_dir().expect("cannot determine cwd"));

        credentials::remove_project(&mount_path)?;
        eprintln!("Project credentials removed.");
    } else {
        credentials::remove_global()?;
        credentials::remove_all_projects()?;
        eprintln!("All credentials removed.");
    }
    Ok(())
}
