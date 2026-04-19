//! `smfs logs` — tail a running daemon's log file.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;

use smfs_core::daemon;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Container tag whose log to read. Defaults to the nearest `.smfs`
    /// marker's tag.
    pub tag: Option<String>,

    /// Follow the log like `tail -f`.
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// How many trailing lines to print before following (default 200).
    #[arg(short = 'n', long, default_value_t = 200)]
    pub lines: usize,
}

pub async fn run(args: Args) -> Result<()> {
    let tag = super::status::resolve_tag(args.tag)?;
    let path = daemon::log_path(&tag);
    if !path.exists() {
        anyhow::bail!("no log at {} (tag running?)", path.display());
    }

    let mut file =
        std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;

    // Print the last N lines (cheap pass: read whole file if small, else seek back).
    let tail = tail_last_lines(&mut file, args.lines)?;
    for line in tail {
        println!("{line}");
    }

    if !args.follow {
        return Ok(());
    }

    // Follow mode: seek to end, poll for growth every 500ms, print new lines.
    let mut pos = file.metadata()?.len();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
        let current = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        if current <= pos {
            continue;
        }
        file.seek(SeekFrom::Start(pos))?;
        let mut buf = vec![0u8; (current - pos) as usize];
        if file.read_exact(&mut buf).is_ok() {
            print!("{}", String::from_utf8_lossy(&buf));
        }
        pos = current;
    }
}

fn tail_last_lines(file: &mut std::fs::File, n: usize) -> Result<Vec<String>> {
    let mut reader = BufReader::new(file);
    let mut lines: Vec<String> = Vec::new();
    for line in reader.by_ref().lines() {
        lines.push(line?);
        if lines.len() > n * 2 {
            let overflow = lines.len() - n;
            lines.drain(..overflow);
        }
    }
    if lines.len() > n {
        let overflow = lines.len() - n;
        lines.drain(..overflow);
    }
    Ok(lines)
}
