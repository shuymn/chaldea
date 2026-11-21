//! `chs`: the spike harness (§4.2).

use chs::scenario::{self, fake, real, win_probe};
use chs::{cli, session};

/// `CHS_WAIT_GO`: an observed launch (C-5) waits for `go` on stdin until its
/// parent has assigned it to the observation job; anything else, or no line
/// within 60 s, exits 3 without running.
fn wait_go() {
    if std::env::var_os("CHS_WAIT_GO").is_none() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut l = String::new();
        let _ = std::io::stdin().read_line(&mut l);
        let _ = tx.send(l);
    });
    match rx.recv_timeout(std::time::Duration::from_secs(60)) {
        Ok(l) if l.trim() == "go" => {}
        _ => std::process::exit(3),
    }
}

fn main() {
    wait_go();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match cli::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
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
