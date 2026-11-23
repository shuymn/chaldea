//! The `chaldea` binary (docs/design/daemon-lifecycle.md §2.2): process setup once, argument
//! parsing whose errors go through the redacting `diag`, the daemon's process entry, and an
//! exit through `std::process::exit` on every path.

mod args;

use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use chaldea_daemon::lifecycle::{
    self, Budgets, DaemonConfig, ExitCode, MvpComponents, RunMode, diag,
};
use clap::Parser;

use crate::args::{Cli, Command, DaemonCommand};

/// How long `main` waits for the log queue before exiting.
const FINAL_FLUSH: Duration = Duration::from_millis(100);

fn main() {
    lifecycle::prepare_process();
    let code = match Cli::try_parse() {
        Err(e) => {
            let text = diag(&e.render().to_string());
            if e.use_stderr() {
                let _ = std::io::stderr().write_all(text.as_bytes());
            } else {
                let _ = std::io::stdout().write_all(text.as_bytes());
            }
            e.exit_code()
        }
        Ok(cli) => catch_unwind(AssertUnwindSafe(|| dispatch(cli)))
            .unwrap_or(ExitCode::Failed)
            .code(),
    };
    lifecycle::flush_process_log(FINAL_FLUSH);
    let _ = std::io::stdout().flush();
    std::process::exit(code);
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

fn dispatch(cli: Cli) -> ExitCode {
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
