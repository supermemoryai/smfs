//! `smfs login` — store Supermemory API credentials globally.

use anyhow::{bail, Result};
use clap::Args as ClapArgs;
use smfs_core::config::credentials::{self, Credentials};
use std::io::{BufRead, IsTerminal, Write};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Supermemory API key. If omitted, prompts interactively.
    #[arg(long)]
    pub key: Option<String>,

    /// Override the Supermemory API base URL (defaults to production).
    #[arg(long)]
    pub api_url: Option<String>,
}

pub async fn run(args: Args) -> Result<()> {
    let api_key = if let Some(k) = args.key {
        k
    } else {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            bail!("No API key provided. Pass --key or run interactively.");
        }
        eprint!("Enter your Supermemory API key: ");
        std::io::stderr().flush()?;
        let mut key = String::new();
        stdin.lock().read_line(&mut key)?;
        let key = key.trim().to_string();
        if key.is_empty() {
            bail!("API key cannot be empty.");
        }
        key
    };

    let base_url = args
        .api_url
        .as_deref()
        .unwrap_or("https://api.supermemory.ai");

    eprint!("Validating API key... ");
    match smfs_core::api::ApiClient::validate_key(base_url, &api_key).await {
        Ok(session) => {
            eprintln!("ok (org: {})", session.org_name);
        }
        Err(smfs_core::api::ApiError::Auth) => {
            bail!("Invalid API key.");
        }
        Err(e) => {
            eprintln!("warning: could not validate ({e}). Saving anyway.");
        }
    }

    credentials::save_global(&Credentials {
        api_key,
        api_url: args.api_url,
    })?;

    eprintln!("Credentials saved.");
    Ok(())
}
