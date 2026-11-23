//! The `chaldea` binary's library (docs/design/daemon-lifecycle.md §2.2): [`dispatch`] is its
//! single entry. It decides `hook-emit` first, before any process setup (hook-ingest §1,
//! §2.13), and runs the emitter there; every other command gets process setup once, argument
//! parsing whose errors go through the redacting `diag`, the daemon's process entry, and an
//! exit through `std::process::exit` on every path.

mod args;
mod client;
mod daemon_cmd;
mod hook_emit;
mod open;

#[doc(hidden)]
pub use hook_emit::Stall;

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use chaldea_daemon::lifecycle::{self, Components, ExitCode, FailReason, diag};
use chaldea_pty_runtime::host::{self, LaunchctlOutput, ReadyArg};
use clap::Parser;

use crate::args::{Cli, Command, DaemonCommand};

/// How long [`dispatch`] waits for the log queue before exiting.
const FINAL_FLUSH: Duration = Duration::from_millis(100);

/// Test seams of the binary tests' fault entry: `dispatch` passes none. Never set by `main`.
#[doc(hidden)]
#[derive(Default)]
#[allow(
    clippy::type_complexity,
    reason = "test seams named for what they replace"
)]
pub struct Seams {
    /// `daemon run`'s components instead of `MvpComponents`.
    pub components: Option<Box<dyn FnOnce() -> Box<dyn Components>>>,
    /// The starter's `launchctl` instead of `host::launchctl`.
    pub launchctl: Option<Box<dyn Fn(&[&OsStr], Instant) -> std::io::Result<LaunchctlOutput>>>,
    /// The launchd label prefix instead of `dev.chaldea.daemon.`.
    pub label_prefix: Option<String>,
    /// Runs once `daemon stop` sent its request, before it waits for completion.
    pub after_stop_sent: Option<Box<dyn FnOnce()>>,
    /// Runs in a detached child at step 1, before it opens its readiness writer.
    pub before_ready: Option<Box<dyn FnOnce() + Send>>,
    /// Runs once `daemon start` read a verdict line (after the launchd job's removal), before
    /// it acts on it.
    pub after_verdict: Option<Box<dyn Fn()>>,
    /// Receives the exit code just before the process exits.
    pub on_exit: Option<Box<dyn FnOnce(i32)>>,
}

/// The `chaldea` process from its arguments (`argv[0]` first) to its exit; `main` is this one
/// call, with [`Stall::Off`].
///
/// `hook-emit` is decided first, before process setup, the runtime, or any configuration
/// (hook-ingest §2.13, mvp §3 "Single binary"): the emitter runs there, exits 0, and prints
/// nothing. `stall` is the emitter's test seam; only the test binary `hook_emit_stall`
/// passes anything but [`Stall::Off`].
pub fn dispatch(args: &[OsString], stall: Stall) -> ! {
    dispatch_with(args, stall, Seams::default())
}

/// [`dispatch`] with test seams (the binary tests' fault entry).
#[doc(hidden)]
pub fn dispatch_with(args: &[OsString], stall: Stall, mut seams: Seams) -> ! {
    if args.get(1).is_some_and(|a| a == "hook-emit") {
        hook_emit::main(args, stall);
    }
    // The launchd job's process forks the daemon while it has one thread (design §2.2), and
    // only for arguments that parse: an invalid invocation exits 2 from this process.
    let fork_error = if from_launchd(args) && parse(args).is_ok() {
        host::leave_launchd_job().err()
    } else {
        None
    };
    lifecycle::prepare_process();
    let code = match parse(args) {
        Err(e) => usage_error(&e),
        Ok(cli) => match fork_error {
            Some(e) => fork_failed(&cli, &e),
            None => catch_unwind(AssertUnwindSafe(|| command(cli, &mut seams)))
                .unwrap_or(ExitCode::Failed)
                .code(),
        },
    };
    lifecycle::flush_process_log(FINAL_FLUSH);
    let _ = std::io::stdout().flush();
    if let Some(record) = seams.on_exit.take() {
        record(code);
    }
    std::process::exit(code);
}

/// `argv[1..4]` is exactly `daemon run --from-launchd`, the shape the starter writes.
fn from_launchd(args: &[OsString]) -> bool {
    args.len() >= 4 && args[1] == "daemon" && args[2] == "run" && args[3] == "--from-launchd"
}

/// The job's process could not fork: it reports on the readiness channel itself.
fn fork_failed(cli: &Cli, e: &std::io::Error) -> i32 {
    let message = format!("fork failed: {e}");
    if let Command::Daemon {
        command: DaemonCommand::Run {
            ready: Some(ready), ..
        },
    } = &cli.command
        && let Ok(arg) = ReadyArg::parse(ready)
        && let Ok(mut w) = host::ready_writer(&arg)
    {
        let _ = w.write_line(&lifecycle::early_fail_line(FailReason::Internal, &message));
        w.close();
    }
    let _ = writeln!(std::io::stderr(), "chaldea: {}", diag(&message));
    ExitCode::Failed.code()
}
fn parse(args: &[OsString]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(args)
}

/// Prints a `clap::Error` through `diag` (stdout for `--help` and `--version`) and returns its
/// exit code.
fn usage_error(e: &clap::Error) -> i32 {
    let text = diag(&e.render().to_string());
    if e.use_stderr() {
        let _ = std::io::stderr().write_all(text.as_bytes());
    } else {
        let _ = std::io::stdout().write_all(text.as_bytes());
    }
    e.exit_code()
}

fn fail(msg: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "chaldea: {}", diag(msg));
    ExitCode::Failed
}

fn command(cli: Cli, seams: &mut Seams) -> ExitCode {
    let state_dir = match lifecycle::resolve_state_dir(cli.state_dir) {
        Ok(d) => d,
        Err(e) => return fail(&format!("the state directory cannot be resolved: {e}")),
    };
    match cli.command {
        Command::Daemon {
            command:
                DaemonCommand::Run {
                    port,
                    from_launchd,
                    detached_child,
                    ready,
                },
        } => daemon_cmd::run(
            state_dir,
            &daemon_cmd::RunArgs {
                port,
                from_launchd,
                detached_child,
                ready,
            },
            seams,
        ),
        Command::Daemon {
            command: DaemonCommand::Start { port },
        } => daemon_cmd::start(&state_dir, port, seams),
        Command::Daemon {
            command: DaemonCommand::Stop,
        } => daemon_cmd::stop(&state_dir, seams),
        Command::Open { print_url } => open::run(&state_dir, print_url, &host::open_url),
    }
}
