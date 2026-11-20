//! `chs`: the spike harness (§4.2).

use chs::scenario::{self, fake, real, win_probe};
use chs::{cli, session};

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match cli::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    // Every start: restore a hooks.json left by a dead run of the codex-home scenario (§4.3).
    real::recover_leftover();
    let cmd = args.positional.first().cloned().unwrap_or_default();
    let result = std::panic::catch_unwind(move || match cmd.as_str() {
        "install-emitter" => scenario::install_emitter(&args),
        "fake" => fake::run(args),
        "real" => real::run(args),
        "print-launch" => real::print_launch(&args),
        "win-probe" => win_probe::run(args),
        "bundle" => scenario::bundle(&args),
        _ => {
            eprintln!("{}", cli::USAGE);
            2
        }
    });
    match result {
        Ok(code) => std::process::exit(code),
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown".into());
            // Sessions were dropped (and killed) during unwinding; the watchdog
            // path kills anything still registered and writes the report.
            session::watchdog_fire("error", &format!("panic: {msg}"), 101);
        }
    }
}
