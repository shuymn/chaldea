//! Child handling shared by the `hook_emit` tests: raw output collected on a thread, and an
//! exit polled every 5 ms up to the caller's deadline, with a bounded kill and reap past it.

use std::io::Read;
use std::process::{Child, ExitStatus};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(5);

/// Reads `r` to its EOF on a thread; the bytes arrive on the returned channel.
pub fn collect(r: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = r {
            let _ = r.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

/// Kills `c` and reaps it, waiting at most `bound`.
pub fn kill_and_reap(c: &mut Child, bound: Duration) {
    let _ = c.kill();
    let end = Instant::now() + bound;
    while matches!(c.try_wait(), Ok(None)) && Instant::now() < end {
        thread::sleep(POLL);
    }
}

/// The exit of `c`, polled until `end`; past it, `c` is killed and reaped (waiting at most
/// `reap`) and the result is `None`.
pub fn wait_until(c: &mut Child, end: Instant, reap: Duration) -> Option<ExitStatus> {
    loop {
        if let Some(s) = c.try_wait().unwrap() {
            return Some(s);
        }
        if Instant::now() >= end {
            kill_and_reap(c, reap);
            return None;
        }
        thread::sleep(POLL);
    }
}
