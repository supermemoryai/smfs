//! Subcommand dispatch.
//!
//! Each public subcommand lives in its own file; this module wires them into
//! a clap `Subcommand` enum and a single `dispatch()` entry point that the
//! binary calls after parsing.
//!
//! For M1 every subcommand is a stub that returns
//! `anyhow::bail!("not implemented (M<n>)")`. The milestone number in each
//! stub points to the plan entry that will deliver the real behavior.

use anyhow::Result;
use clap::Subcommand;

pub mod auth;
pub mod daemon_inner;
pub mod daemon_runtime;
pub mod grep;
pub mod init;
pub mod install;
pub mod list;
pub mod login;
pub mod logout;
pub mod logs;
pub mod marker;
pub mod mount;
pub mod status;
pub mod sync;
pub mod unmount;
pub mod whoami;

/// All user-facing subcommands, plus the hidden `daemon-inner` used when
/// the CLI forks itself into a background process in M10.
#[derive(Subcommand)]
pub enum Command {
    /// Authenticate with Supermemory (prefer `supermemory login` from the TS CLI)
    Login(login::Args),

    /// Show the currently-authenticated user, org, and API endpoint
    Whoami(whoami::Args),

    /// Mount a Supermemory container at a local path
    Mount(mount::Args),

    /// Unmount a running supermemoryfs mount
    Unmount(unmount::Args),

    /// Show status of the running daemon
    Status(status::Args),

    /// List all running supermemoryfs mounts
    List,

    /// Tail a running daemon's log
    Logs(logs::Args),

    /// Semantic search across files in a container
    Grep(grep::Args),

    /// Install the grep shell wrapper for transparent semantic search
    Init(init::Args),

    /// Self-install: move this binary to ~/.local/bin and print PATH hint
    Install(install::Args),

    /// Remove stored credentials
    Logout(logout::Args),

    /// Force a sync cycle now
    Sync(sync::Args),

    /// Internal: long-running daemon entry point (do not invoke directly)
    #[command(hide = true)]
    DaemonInner(daemon_inner::Args),
}

/// Route a parsed command to its handler.
pub async fn dispatch(cmd: Command) -> Result<()> {
    match cmd {
        Command::Login(args) => login::run(args).await,
        Command::Whoami(args) => whoami::run(args).await,
        Command::Mount(args) => mount::run(args).await,
        Command::Grep(args) => grep::run(args).await,
        Command::Init(args) => init::run(args).await,
        Command::Install(args) => install::run(args).await,
        Command::Logout(args) => logout::run(args).await,
        Command::Unmount(args) => unmount::run(args).await,
        Command::Status(args) => status::run(args).await,
        Command::List => list::run().await,
        Command::Logs(args) => logs::run(args).await,
        Command::Sync(args) => sync::run(args).await,
        Command::DaemonInner(args) => daemon_inner::run(args).await,
    }
}
