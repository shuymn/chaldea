//! The one rule for deciding an awaited result against a deadline
//! (`docs/design/deadlines.md` §3 D1).
//!
//! `tokio::time::timeout`, `timeout_at`, `sleep`, and `sleep_until` are disallowed in this
//! crate (`clippy.toml`): a new deadline decision goes through [`settle_before`], and any
//! other timer through a private function that constructs only that timer and names the
//! contract its callers follow.

use std::future::Future;
use std::time::Instant;

/// An awaited result classified against its deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Settled<T> {
    /// Ready, and the clock read once after it was taken was before the deadline.
    InTime(T),
    /// Ready, but the clock read once after it was taken was at or after the deadline.
    Late(T),
    /// The timer fired first; the future was dropped unfinished.
    Elapsed,
}

impl<T> Settled<T> {
    /// A state claim or a result: counts only in time.
    pub(crate) fn in_time(self) -> Option<T> {
        match self {
            Settled::InTime(v) => Some(v),
            Settled::Late(_) | Settled::Elapsed => None,
        }
    }
}

/// Awaits `fut` under a timer at `timer`, then classifies its output with one reading of
/// `now`, taken after `fut` was dropped: at or after `deadline` it is [`Settled::Late`]. A
/// fired timer is [`Settled::Elapsed`], with no reading. The caller computes `timer` where
/// the conversion of `deadline` belongs; nothing here converts.
pub(crate) async fn settle_before<F: Future>(
    now: impl Fn() -> Instant,
    deadline: Instant,
    timer: tokio::time::Instant,
    fut: F,
) -> Settled<F::Output> {
    // The `Timeout` owns `fut`, so `fut` is dropped when this await completes.
    let r = bound(timer, fut).await;
    match r {
        Err(_) => Settled::Elapsed,
        Ok(v) if now() >= deadline => Settled::Late(v),
        Ok(v) => Settled::InTime(v),
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "the helper's own timer: its output is classified by settle_before"
)]
fn bound<F: Future>(timer: tokio::time::Instant, fut: F) -> tokio::time::Timeout<F> {
    tokio::time::timeout_at(timer, fut)
}

#[cfg(test)]
mod tests;
