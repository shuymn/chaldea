//! The `chaldea` binary's library (docs/design/daemon-lifecycle.md §2.2): [`dispatch`] is its
//! single entry. It decides `hook-emit` first, before any process setup (hook-ingest §1,
//! §2.13); every other command gets process setup once, argument parsing whose errors go
//! through the redacting `diag`, the daemon's process entry, and an exit through
//! `std::process::exit` on every path.

mod args;

use std::ffi::OsString;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use chaldea_daemon::lifecycle::{
    self, Budgets, DaemonConfig, ExitCode, MvpComponents, RunMode, diag,
};
use clap::Parser;

use crate::args::{Cli, Command, DaemonCommand};

/// How long [`dispatch`] waits for the log queue before exiting.
const FINAL_FLUSH: Duration = Duration::from_millis(100);

/// The `chaldea` process from its arguments (`argv[0]` first) to its exit; `main` is this one
/// call.
///
/// `hook-emit` is decided first, before process setup, the runtime, or any configuration
/// (hook-ingest §2.13, mvp §3 "Single binary"). PR 9a's emitter goes in that branch, and 9a
/// extends this signature with the emitter's test seam (`Stall`); until then the branch
/// answers as clap does for an unknown subcommand (exit 2), without the setup.
pub fn dispatch(args: &[OsString]) -> ! {
    if args.get(1).is_some_and(|a| a == "hook-emit") {
        let code = parse(args).map_or_else(|e| usage_error(&e), |_| ExitCode::Usage.code());
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }
    lifecycle::prepare_process();
    let code = match parse(args) {
        Err(e) => usage_error(&e),
        Ok(cli) => catch_unwind(AssertUnwindSafe(|| command(cli)))
            .unwrap_or(ExitCode::Failed)
            .code(),
    };
    lifecycle::flush_process_log(FINAL_FLUSH);
    let _ = std::io::stdout().flush();
    std::process::exit(code);
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

fn not_yet(what: &str) -> ExitCode {
    let _ = writeln!(
        std::io::stderr(),
        "chaldea: `{what}` is not implemented yet; use `chaldea daemon run`"
    );
    ExitCode::Usage
}

fn command(cli: Cli) -> ExitCode {
    match cli.command {
        Command::Daemon {
            command: DaemonCommand::Run { port },
        } => {
            let state_dir = match lifecycle::resolve_state_dir(cli.state_dir) {
                Ok(d) => d,
                Err(e) => return fail(&format!("the state directory cannot be resolved: {e}")),
            };
            let report = lifecycle::run(
                DaemonConfig {
                    state_dir,
                    port,
                    mode: RunMode::Foreground,
                    budgets: Budgets::DEFAULT,
                },
                Box::new(MvpComponents),
            );
            report.code
        }
        Command::Daemon {
            command: DaemonCommand::Start { .. },
        } => not_yet("daemon start"),
        Command::Daemon {
            command: DaemonCommand::Stop,
        } => not_yet("daemon stop"),
        Command::Open { .. } => not_yet("open"),
    }
}
