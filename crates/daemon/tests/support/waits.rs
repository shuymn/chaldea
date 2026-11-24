//! Test bounds and pauses (`docs/design/deadlines.md` §3 D2). The daemon's `clippy.toml`
//! disallows tokio's timer constructors; tests reach them only through these functions.
//!
//! A deadline claim under test is checked with paused time, an injected clock, or the fault
//! gates, never with these as its oracle. They are plain functions, not `async fn`, so a
//! deadline is computed when the call is evaluated, as tokio's constructors compute it.

use std::future::Future;
use std::time::Duration;

use futures_util::FutureExt;

/// [`tokio::time::timeout`].
#[allow(
    clippy::disallowed_methods,
    reason = "a test's bound on its own wait, never the oracle of a deadline claim"
)]
pub fn bounded<F: Future>(within: Duration, fut: F) -> tokio::time::Timeout<F> {
    tokio::time::timeout(within, fut)
}

/// [`tokio::time::timeout_at`].
#[allow(
    clippy::disallowed_methods,
    reason = "a test's bound on its own wait, never the oracle of a deadline claim"
)]
pub fn bounded_at<F: Future>(at: tokio::time::Instant, fut: F) -> tokio::time::Timeout<F> {
    tokio::time::timeout_at(at, fut)
}

/// [`tokio::time::sleep`]: the timer is set now, awaited later.
pub fn pause(d: Duration) -> impl Future<Output = ()> {
    bounded(d, std::future::pending::<()>()).map(drop)
}

/// [`tokio::time::sleep_until`].
pub fn pause_until(at: tokio::time::Instant) -> impl Future<Output = ()> {
    bounded_at(at, std::future::pending::<()>()).map(drop)
}
