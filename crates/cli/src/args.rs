//! The command line (docs/design/daemon-lifecycle.md §2.2, D14).

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use chaldea_core::ids::{Agent, LaunchId, ThreadId};
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
        /// A first prompt: Codex's positional argument, or a queued input Claude is sent once
        /// it is ready.
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
        /// A first prompt: Codex's positional argument, or a queued input Claude is sent once
        /// it is ready.
        #[arg(long)]
        prompt: Option<String>,
        /// Resume only if this exited launch is still the thread's current launch (else
        /// nothing is done and the command exits 1); without it, the current launch is read
        /// first.
        #[arg(long, value_parser = crate::agent_cmd::launch_id)]
        launch: Option<LaunchId>,
    },
    /// Queue a prompt for a thread's agent CLI; it is written once the CLI is ready.
    ///
    /// Prints the input and its state. A prompt written and not confirmed by the CLI is shown
    /// `unconfirmed` and never sent again.
    Send {
        #[arg(value_parser = crate::agent_cmd::thread_id)]
        thread: ThreadId,
        /// The prompt.
        text: String,
    },
    /// Assert that a thread's agent CLI is idle with an empty composer, so a queued prompt
    /// may be written.
    Ready {
        #[arg(value_parser = crate::agent_cmd::thread_id)]
        thread: ThreadId,
        /// Act only if this is still the thread's current launch (else nothing is done and
        /// the command exits 1); without it, the current launch is read first.
        #[arg(long, value_parser = crate::agent_cmd::launch_id)]
        launch: Option<LaunchId>,
    },
    /// Write keys to a thread's agent CLI: enter, escape, tab, backspace, ctrl_c, up, down,
    /// left, right, or one printable character.
    Key {
        #[arg(value_parser = crate::agent_cmd::thread_id)]
        thread: ThreadId,
        #[arg(required = true, value_parser = crate::agent_cmd::key_token)]
        keys: Vec<chaldea_core::input::key::KeyToken>,
        /// Answer this approval card: navigation keys, then at most one deciding key, last.
        #[arg(long, value_parser = crate::agent_cmd::approval_id)]
        approval: Option<chaldea_core::ids::ApprovalId>,
        /// Act only if this is still the thread's current launch (else nothing is done and
        /// the command exits 1); without it, the current launch is read first.
        #[arg(long, value_parser = crate::agent_cmd::launch_id)]
        launch: Option<LaunchId>,
    },
    /// Resolve or cancel an input.
    Input {
        #[command(subcommand)]
        command: InputCommand,
    },
    /// Stop a thread's agent CLI and its process tree.
    ///
    /// Exits 0 when the tree's end was confirmed within 5 s of the stop request the daemon
    /// accepted, 4 when it was confirmed later, and 1 when it could not be confirmed. A
    /// request retried after a refused attempt counts from the attempt the daemon accepted.
    ///
    /// With `--launch` alone, the stop is done only if that launch is still the thread's
    /// current launch (else nothing is done and the command exits 1).
    ///
    /// With `--assume-dead`, an exited launch whose end could not be confirmed: without
    /// `--launch`, print what was checked of the current launch and send nothing (exit 0, or
    /// 1 when the history could not be read completely); with `--launch`, confirm as the
    /// owner that its processes ended (exit 0).
    Stop {
        #[arg(value_parser = crate::agent_cmd::thread_id)]
        thread: ThreadId,
        /// Review, or with `--launch` confirm, the end of the current launch's processes.
        #[arg(long)]
        assume_dead: bool,
        /// The current launch's id: the launch to stop, or the one the confirmation names.
        #[arg(long, value_parser = crate::agent_cmd::launch_id)]
        launch: Option<LaunchId>,
    },
    /// Close a thread whose agent CLI has ended.
    Close {
        #[arg(value_parser = crate::agent_cmd::thread_id)]
        thread: ThreadId,
        /// Stop a running agent CLI first.
        #[arg(long)]
        stop: bool,
        /// Act only if this is still the thread's current launch (else nothing is done and
        /// the command exits 1); without it, the current launch is read first.
        #[arg(long, value_parser = crate::agent_cmd::launch_id)]
        launch: Option<LaunchId>,
    },
    /// Show a thread's state, attention, launch, session, inputs, open cards, recent events,
    /// and live screen.
    ///
    /// Exits 0 when the thread's state was read (a missing screen included), 1 when it could
    /// not be read, and 4 when its summary read did not finish in time.
    Inspect {
        #[arg(value_parser = crate::agent_cmd::thread_id)]
        thread: ThreadId,
        /// Print one JSON object (`InspectOutput`).
        #[arg(long)]
        json: bool,
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

/// `chaldea input`.
#[derive(Debug, Subcommand)]
pub enum InputCommand {
    /// Record what became of an `unconfirmed` input.
    Resolve {
        #[arg(value_parser = crate::agent_cmd::input_id)]
        input: chaldea_core::ids::InputId,
        /// Whether the CLI received it.
        #[arg(long, value_enum)]
        outcome: Resolution,
    },
    /// Cancel a `queued` input.
    Cancel {
        #[arg(value_parser = crate::agent_cmd::input_id)]
        input: chaldea_core::ids::InputId,
    },
}

/// `chaldea input resolve --outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Resolution {
    /// The CLI received it (`acked`).
    Delivered,
    /// It did not reach the CLI (`dismissed`).
    Dismissed,
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
