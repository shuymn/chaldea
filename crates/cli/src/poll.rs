//! Polling state that cannot wake a waiter, against one cutoff (`docs/design/deadlines.md`).

use std::cell::Cell;
use std::time::{Duration, Instant};

/// Polls `observe` every `period` until it decides: `None` at the deadline, also for a result
/// first observed at or after it.
pub(crate) fn poll_until<T>(
    deadline: Instant,
    period: Duration,
    observe: impl FnMut() -> Option<T>,
) -> Option<T> {
    poll_until_cutoff(&Cell::new(deadline), period, observe)
}

/// [`poll_until`] against a cutoff a test seam can bring forward while an observation runs: a
/// result then counts only if it came before the cutoff as it stands after the observation.
/// Nothing moves the cutoff outside tests.
pub(crate) fn poll_until_cutoff<T>(
    cutoff: &Cell<Instant>,
    period: Duration,
    observe: impl FnMut() -> Option<T>,
) -> Option<T> {
    poll_until_with(cutoff, period, &Instant::now, observe)
}

/// [`poll_until_cutoff`] on the clock `now`: the cutoff is checked on it before each
/// observation and again after, so a result first seen at or after the cutoff loses to it and
/// nothing is observed once it has passed. The pause between observations is real time, at
/// most `period` and never past the cutoff as `now` reads it.
pub(crate) fn poll_until_with<T>(
    cutoff: &Cell<Instant>,
    period: Duration,
    now: &dyn Fn() -> Instant,
    mut observe: impl FnMut() -> Option<T>,
) -> Option<T> {
    loop {
        let before = cutoff.get();
        if now() >= before {
            return None;
        }
        let seen = observe();
        if now() >= cutoff.get() {
            return None;
        }
        if let Some(t) = seen {
            return Some(t);
        }
        std::thread::sleep(period.min(cutoff.get().saturating_duration_since(now())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock the test moves.
    fn clock(at: &Cell<Instant>) -> impl Fn() -> Instant + '_ {
        || at.get()
    }

    #[test]
    fn result_before_the_cutoff() {
        let t0 = Instant::now();
        let at = Cell::new(t0);
        let cutoff = Cell::new(t0 + Duration::from_secs(10));
        let mut n = 0;
        let got = poll_until_with(&cutoff, Duration::ZERO, &clock(&at), || {
            n += 1;
            at.set(at.get() + Duration::from_secs(1));
            (n == 3).then_some(n)
        });
        assert_eq!(got, Some(3));
    }

    /// A result first observed when the clock has reached the cutoff loses to it, and nothing
    /// is observed once it has passed.
    #[test]
    fn result_at_the_cutoff_is_not_adopted() {
        let t0 = Instant::now();
        let at = Cell::new(t0);
        let cutoff = Cell::new(t0 + Duration::from_secs(1));
        let mut calls = 0;
        let got = poll_until_with(&cutoff, Duration::ZERO, &clock(&at), || {
            calls += 1;
            at.set(t0 + Duration::from_secs(1));
            Some(())
        });
        assert_eq!((got, calls), (None, 1));
        let got = poll_until_with(&cutoff, Duration::ZERO, &clock(&at), || {
            calls += 1;
            Some(())
        });
        assert_eq!((got, calls), (None, 1));
    }

    /// The cutoff as it stands after the observation decides.
    #[test]
    fn cutoff_brought_forward_during_an_observation() {
        let t0 = Instant::now();
        let at = Cell::new(t0);
        let cutoff = Cell::new(t0 + Duration::from_secs(10));
        let got = poll_until_with(&cutoff, Duration::ZERO, &clock(&at), || {
            cutoff.set(t0);
            Some(())
        });
        assert_eq!(got, None);
    }
}
