//! `attachable`: prints `MARKER-n` every 500 ms and reports its terminal size at start and
//! on each change: on `SIGWINCH` on Unix, and by polling the console size every 20 ms on
//! Windows, which has no such signal (the event reports an observed change).

use std::io::Write;
use std::time::{Duration, Instant};

use super::{Opts, term};

const MARKER_EVERY: Duration = Duration::from_millis(500);
const TICK: Duration = Duration::from_millis(20);

pub(crate) fn run(o: &Opts) -> i32 {
    let raw = super::enter_raw(o, false);
    let tty = term::stdin_is_tty();
    #[cfg(unix)]
    let winch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    #[cfg(unix)]
    if tty && signal_hook::flag::register(signal_hook::consts::SIGWINCH, winch.clone()).is_err() {
        return 1;
    }
    let rep = super::hello(o, raw);
    // On a pipe the size fields are null and no `winch` follows.
    let mut last = term::size();
    rep.send(&term::size_event_of("size", last));
    let mut out = std::io::stdout();
    let mut n = 0u64;
    let mut next = Instant::now();
    loop {
        #[cfg(unix)]
        if tty && winch.swap(false, std::sync::atomic::Ordering::AcqRel) {
            last = term::size();
            rep.send(&term::size_event_of("winch", last));
        }
        #[cfg(not(unix))]
        if tty {
            let now = term::size();
            if now.is_some() && now != last {
                last = now;
                rep.send(&term::size_event_of("winch", last));
            }
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
