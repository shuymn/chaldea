//! The command line (docs/design/daemon-lifecycle.md §2.2, D14).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Chaldea: a PTY-launched AI coding agent client and control plane.
#[derive(Debug, Parser)]
#[command(name = "chaldea", version)]
pub struct Cli {
    /// The state directory (else `CHALDEA_STATE_DIR`, else `$HOME/.chaldea`, or
    /// `%LOCALAPPDATA%\Chaldea` on Windows).
    #[arg(long, global = true, value_name = "PATH")]
    pub state_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run, start, or stop the daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Open the pairing page in the browser.
    Open {
        /// Print the pairing URL instead of opening it.
        #[arg(long)]
        print_url: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in this terminal until it is stopped.
    Run {
        /// Serve on this port (changes the pairing origin when it differs from the saved one).
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
    },
    /// Start the daemon detached from this terminal.
    Start {
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
    },
    /// Stop the running daemon.
    Stop,
}
