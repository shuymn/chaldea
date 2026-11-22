//! `attachable`: prints `MARKER-n` every 500 ms and reports its terminal size, at start and
//! on each `SIGWINCH`.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use super::{Opts, term};

const MARKER_EVERY: Duration = Duration::from_millis(500);
const TICK: Duration = Duration::from_millis(20);

pub(crate) fn run(o: &Opts) -> i32 {
    let raw = super::enter_raw(o, false);
    let winch = Arc::new(AtomicBool::new(false));
    let tty = term::stdin_is_tty();
    #[cfg(unix)]
    if tty && signal_hook::flag::register(signal_hook::consts::SIGWINCH, winch.clone()).is_err() {
        return 1;
    }
    let rep = super::hello(o, raw);
    // On a pipe the size fields are null and no `winch` follows.
    rep.send(&term::size_event("size"));
    let mut out = std::io::stdout();
    let mut n = 0u64;
    let mut next = Instant::now();
    loop {
        if tty && winch.swap(false, std::sync::atomic::Ordering::AcqRel) {
            rep.send(&term::size_event("winch"));
        }
        if Instant::now() >= next {
            let _ = write!(out, "MARKER-{n}\r\n");
            let _ = out.flush();
            n += 1;
            next += MARKER_EVERY;
        }
        std::thread::sleep(TICK);
    }
}
