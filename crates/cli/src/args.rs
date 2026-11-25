//! The command line (docs/design/daemon-lifecycle.md §2.2, D14).

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use chaldea_core::ids::{Agent, ThreadId};
use clap::{Parser, Subcommand};

pub use crate::agent_cmd::Until;

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
    /// Start an agent CLI in a new thread.
    Spawn {
        /// The agent CLI.
        #[arg(long, value_enum)]
        agent: AgentArg,
        /// Its working directory (else the current one).
        #[arg(long, value_name = "PATH")]
        cwd: Option<PathBuf>,
        /// A first prompt, passed to Codex as its positional argument.
        #[arg(long)]
        prompt: Option<String>,
        /// The thread's task.
        #[arg(long)]
        task: Option<String>,
    },
    /// Start a thread's agent CLI again on its session.
    Resume {
        #[arg(value_parser = crate::agent_cmd::thread_id)]
        thread: ThreadId,
        /// A first prompt, passed to Codex as its positional argument.
        #[arg(long)]
        prompt: Option<String>,
    },
    /// Stop a thread's agent CLI and its process tree.
    Stop {
        #[arg(value_parser = crate::agent_cmd::thread_id)]
        thread: ThreadId,
    },
    /// Close a thread whose agent CLI has ended.
    Close {
        #[arg(value_parser = crate::agent_cmd::thread_id)]
        thread: ThreadId,
        /// Stop a running agent CLI first.
        #[arg(long)]
        stop: bool,
    },
    /// Wait until a thread's process exits, its turn is idle, or it needs attention.
    Wait {
        #[arg(value_parser = crate::agent_cmd::thread_id)]
        thread: ThreadId,
        #[arg(long, value_enum, default_value = "exited")]
        until: Until,
        /// How long to wait, such as 30s, 10m, or 500ms.
        #[arg(long, value_parser = crate::agent_cmd::duration, default_value = "10m")]
        timeout: Duration,
    },
}

/// `--agent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AgentArg {
    Claude,
    Codex,
}

impl From<AgentArg> for Agent {
    fn from(a: AgentArg) -> Agent {
        match a {
            AgentArg::Claude => Agent::Claude,
            AgentArg::Codex => Agent::Codex,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in this terminal until it is stopped.
    Run {
        /// Serve on this port (changes the pairing origin when it differs from the saved one).
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
        /// The launchd job's process (`daemon start` on a macOS GUI host): `dispatch` forks
        /// before process setup when it is the first argument after `run`.
        #[arg(long, hide = true, requires = "detached_child")]
        from_launchd: bool,
        /// The child `daemon start` launched: its verdict goes to `--ready`.
        #[arg(long, hide = true, requires = "ready")]
        detached_child: bool,
        /// The readiness channel: a Unix socket path (macOS) or `stdout` (Windows).
        #[arg(long, hide = true, value_name = "CHANNEL", requires = "detached_child")]
        ready: Option<OsString>,
    },
    /// Start the daemon detached from this terminal and wait until it serves.
    Start {
        /// Serve on this port (changes the pairing origin when it differs from the saved one).
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
    },
    /// Stop the running daemon.
    Stop,
}
