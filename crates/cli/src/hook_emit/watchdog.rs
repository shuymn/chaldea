//! The emitter's exit guarantees (hook-ingest §2.13 step 1): a panic hook that exits 0
//! without printing, and a watchdog thread that exits 0 at a fixed instant whatever the main
//! thread is blocked on.

use std::time::Instant;

/// Installs the panic hook, then starts the watchdog that calls `exit(0)` at `at`. A thread
/// that cannot be started exits 0 at once.
pub(crate) fn start(at: Instant) {
    std::panic::set_hook(Box::new(|_| std::process::exit(0)));
    let started = std::thread::Builder::new()
        .name("hook-emit-watchdog".to_owned())
        .spawn(move || {
            loop {
                let left = at.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    std::process::exit(0);
                }
                std::thread::sleep(left);
            }
        });
    if started.is_err() {
        std::process::exit(0);
    }
}
