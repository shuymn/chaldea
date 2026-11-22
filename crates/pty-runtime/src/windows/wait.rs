//! The chunked wait of `WindowsChild::wait_timeout` (`docs/design/pty-windows.md` §2.4),
//! over a clock and a wait function, so the unit tests script both on every OS.

use std::io;
use std::time::{Duration, Instant};

/// The longest single wait (below `INFINITE`).
pub(crate) const MAX_CHUNK_MS: u32 = u32::MAX - 1;

/// Waits up to `timeout` from `start` in chunks of at most `cap` ms: a zero timeout is one
/// poll; otherwise each chunk is the remainder rounded up to whole milliseconds (so a
/// sub-millisecond remainder waits 1 ms, never `INFINITE`). The remainder comes from the
/// elapsed time, so no `Instant` addition can overflow. True when `wait` reported the
/// signal.
pub(crate) fn chunked(
    timeout: Duration,
    start: Instant,
    now: impl Fn() -> Instant,
    cap: u32,
    mut wait: impl FnMut(u32) -> io::Result<bool>,
) -> io::Result<bool> {
    if timeout.is_zero() {
        return wait(0);
    }
    loop {
        let left = timeout.saturating_sub(now().saturating_duration_since(start));
        if left.is_zero() {
            return Ok(false);
        }
        let ceil = left.as_nanos().div_ceil(1_000_000);
        let ms = u32::try_from(ceil).unwrap_or(u32::MAX).clamp(1, cap.max(1));
        if wait(ms)? {
            return Ok(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    /// A scripted clock: each wait advances it by its length; `signal_at` makes the wait
    /// that reaches it report the signal.
    struct Script {
        base: Instant,
        now: Cell<Duration>,
        waits: RefCell<Vec<u32>>,
        signal_at: Option<Duration>,
    }

    impl Script {
        fn new(signal_at: Option<Duration>) -> Script {
            Script {
                base: Instant::now(),
                now: Cell::new(Duration::ZERO),
                waits: RefCell::new(Vec::new()),
                signal_at,
            }
        }

        fn run(&self, timeout: Duration, cap: u32) -> bool {
            chunked(
                timeout,
                self.base,
                || self.base + self.now.get(),
                cap,
                |ms| {
                    self.waits.borrow_mut().push(ms);
                    let end = self.now.get() + Duration::from_millis(u64::from(ms));
                    if let Some(t) = self.signal_at.filter(|t| *t <= end) {
                        self.now.set(t);
                        return Ok(true);
                    }
                    self.now.set(end);
                    Ok(false)
                },
            )
            .unwrap()
        }
    }

    /// PR4-CL-15: a wait continues past one chunk until the timeout, each chunk capped.
    #[test]
    fn chunked_wait_continues() {
        let s = Script::new(None);
        assert!(!s.run(Duration::from_millis(50), 1));
        assert_eq!(*s.waits.borrow(), vec![1; 50]);
        let s = Script::new(None);
        assert!(!s.run(Duration::from_millis(50), 20));
        assert_eq!(*s.waits.borrow(), vec![20, 20, 10]);
    }

    #[test]
    fn signal_ends_the_wait() {
        let s = Script::new(Some(Duration::from_millis(25)));
        assert!(s.run(Duration::from_millis(50), 10));
        assert_eq!(*s.waits.borrow(), vec![10, 10, 10]);
    }

    #[test]
    fn zero_timeout_polls_once() {
        let s = Script::new(None);
        assert!(!s.run(Duration::ZERO, MAX_CHUNK_MS));
        assert_eq!(*s.waits.borrow(), vec![0]);
    }

    #[test]
    fn sub_millisecond_timeout_waits_one_millisecond() {
        let s = Script::new(None);
        assert!(!s.run(Duration::from_micros(500), MAX_CHUNK_MS));
        assert_eq!(*s.waits.borrow(), vec![1]);
    }

    /// `Duration::MAX` waits in capped chunks and never overflows the clock.
    #[test]
    fn maximum_timeout_does_not_overflow() {
        let s = Script::new(Some(Duration::from_secs(3)));
        assert!(s.run(Duration::MAX, 1_000));
        assert_eq!(*s.waits.borrow(), vec![1_000, 1_000, 1_000]);
        let s = Script::new(Some(Duration::from_millis(1)));
        assert!(s.run(Duration::MAX, MAX_CHUNK_MS));
        assert_eq!(*s.waits.borrow(), vec![MAX_CHUNK_MS]);
    }
}
