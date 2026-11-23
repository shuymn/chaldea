//! The emitter's exit guarantees (hook-ingest §2.13 step 1): a panic hook that exits 0
//! without printing, and a watchdog thread that exits 0 at a fixed instant whatever the main
//! thread is blocked on. Both end the process through `host::exit_now`, which runs no exit
//! handler, stdio flush, or DLL detach, so nothing the process holds can delay the exit.

use std::time::Instant;

use chaldea_pty_runtime::host;

use super::{Clock, SystemClock};

/// Installs the panic hook, then starts the watchdog that exits 0 at `at`. A thread that
/// cannot be started exits 0 at once.
pub(crate) fn start(at: Instant) {
    std::panic::set_hook(Box::new(|_| host::exit_now(0)));
    let started = std::thread::Builder::new()
        .name("hook-emit-watchdog".to_owned())
        .spawn(move || {
            wait_until(&SystemClock, at);
            host::exit_now(0)
        });
    if started.is_err() {
        host::exit_now(0);
    }
}

/// Returns at the first clock reading at or after `at`, however early a sleep wakes.
pub(crate) fn wait_until(clock: &dyn Clock, at: Instant) {
    loop {
        let left = at.saturating_duration_since(clock.now());
        if left.is_zero() {
            return;
        }
        clock.sleep(left);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::time::Duration;

    use super::*;

    /// A clock whose first sleep wakes after a third of the request (a spurious wake).
    struct Early {
        now: Cell<Instant>,
        sleeps: RefCell<Vec<Duration>>,
    }

    impl Clock for Early {
        fn now(&self) -> Instant {
            self.now.get()
        }
        fn sleep(&self, d: Duration) {
            let first = self.sleeps.borrow().is_empty();
            self.sleeps.borrow_mut().push(d);
            self.now.set(self.now.get() + if first { d / 3 } else { d });
        }
    }

    #[test]
    fn the_watchdog_wakes_at_its_target() {
        let t0 = Instant::now();
        let clock = Early {
            now: Cell::new(t0),
            sleeps: RefCell::new(Vec::new()),
        };
        let at = super::super::Budget::from(t0).watchdog;
        wait_until(&clock, at);
        // 3000 ms from T0, restated from the design.
        assert_eq!(clock.now() - t0, Duration::from_millis(3000));
        assert_eq!(
            clock.sleeps.borrow().len(),
            2,
            "slept again after the early wake"
        );
    }
}
