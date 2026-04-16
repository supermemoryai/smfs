//! `smfs init` — install the grep shell wrapper.

use anyhow::Result;
use clap::Args as ClapArgs;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Append the shell wrapper to ~/.zshrc instead of printing to stdout.
    #[arg(long)]
    pub install: bool,
}

const SHELL_WRAPPER: &str = r#"
# supermemoryfs grep wrapper — semantic search inside mounted containers
# Added by: smfs init --install
grep() {
    # If any flags present, use normal grep
    for arg in "$@"; do
        case "$arg" in
            -*) command grep "$@"; return ;;
        esac
    done
    # Check for .smfs marker in current dir or parent dirs
    _smfs_dir="$PWD"
    while [ "$_smfs_dir" != "/" ]; do
        if [ -f "$_smfs_dir/.smfs" ]; then
            smfs grep "$@"
            return
        fi
        _smfs_dir="$(dirname "$_smfs_dir")"
    done
    command grep "$@"
}
"#;

pub async fn run(args: Args) -> Result<()> {
    if args.install {
        let home = std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .map_err(|_| anyhow::anyhow!("cannot determine home directory"))?;
        let zshrc = home.join(".zshrc");

        // Check if already installed
        if let Ok(content) = std::fs::read_to_string(&zshrc) {
            if content.contains("supermemoryfs grep wrapper") {
                eprintln!("grep wrapper already installed in ~/.zshrc");
                return Ok(());
            }
        }

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&zshrc)?;
        file.write_all(SHELL_WRAPPER.as_bytes())?;
        eprintln!("grep wrapper installed in ~/.zshrc");
        eprintln!("run: source ~/.zshrc");
    } else {
        print!("{SHELL_WRAPPER}");
    }

    Ok(())
}
