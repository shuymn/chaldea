//! `settle_before` (docs/design/deadlines.md §3 D4): a counting test clock, futures that
//! count their polls, and paused time for the timer; no real-time sleeps.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use super::{Settled, settle_before};
use crate::store::tests::support::waits;

/// A clock the test sets, counting its readings.
struct TestClock {
    state: Mutex<(Instant, usize)>,
}

impl TestClock {
    fn at(at: Instant) -> Arc<Self> {
        Arc::new(TestClock {
            state: Mutex::new((at, 0)),
        })
    }

    fn set(&self, at: Instant) {
        self.state.lock().unwrap().0 = at;
    }

    fn now(&self) -> Instant {
        let mut s = self.state.lock().unwrap();
        s.1 += 1;
        s.0
    }

    fn readings(&self) -> usize {
        self.state.lock().unwrap().1
    }
}

/// A timer that does not fire while the test runs.
fn never() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(24 * 60 * 60)
}

/// A future ready on its first poll with `v`, counting its polls, that runs `on_poll`
/// before it returns and `on_drop` when dropped.
struct Probe<T: Unpin> {
    v: Option<T>,
    polls: Arc<Mutex<usize>>,
    on_poll: Box<dyn FnMut() + Send>,
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl<T: Unpin> Probe<T> {
    fn ready(v: T) -> (Self, Arc<Mutex<usize>>) {
        let polls = Arc::new(Mutex::new(0));
        (
            Probe {
                v: Some(v),
                polls: polls.clone(),
                on_poll: Box::new(|| {}),
                on_drop: None,
            },
            polls,
        )
    }
}

impl<T: Unpin> Future for Probe<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<T> {
        *self.polls.lock().unwrap() += 1;
        (self.on_poll)();
        Poll::Ready(self.v.take().expect("polled after Ready"))
    }
}

impl<T: Unpin> Drop for Probe<T> {
    fn drop(&mut self) {
        if let Some(f) = self.on_drop.take() {
            f();
        }
    }
}

#[tokio::test]
async fn ready_in_time_takes_one_sample() {
    let deadline = Instant::now() + Duration::from_secs(10);
    let clock = TestClock::at(deadline - Duration::from_nanos(1));
    let c = clock.clone();
    let (fut, polls) = Probe::ready(7);
    let r = settle_before(move || c.now(), deadline, never(), fut).await;
    assert_eq!(r, Settled::InTime(7));
    assert_eq!(clock.readings(), 1);
    assert_eq!(*polls.lock().unwrap(), 1);
}

#[tokio::test]
async fn ready_at_the_deadline_is_late() {
    let deadline = Instant::now() + Duration::from_secs(10);
    let clock = TestClock::at(deadline - Duration::from_secs(1));
    let (mut fut, _) = Probe::ready(7);
    let c = clock.clone();
    fut.on_poll = Box::new(move || c.set(deadline));
    let c = clock.clone();
    let r = settle_before(move || c.now(), deadline, never(), fut).await;
    assert_eq!(r, Settled::Late(7));
    assert_eq!(clock.readings(), 1);
}

#[tokio::test]
async fn polled_first_after_the_deadline_is_late() {
    let deadline = Instant::now() + Duration::from_secs(10);
    let clock = TestClock::at(deadline + Duration::from_secs(1));
    let c = clock.clone();
    let (fut, polls) = Probe::ready(7);
    let r = settle_before(move || c.now(), deadline, never(), fut).await;
    assert_eq!(r, Settled::Late(7));
    assert_eq!(*polls.lock().unwrap(), 1);
}

/// The future is dropped before the clock is read: a destructor that runs to the deadline
/// makes the result late.
#[tokio::test]
async fn sample_follows_the_drop() {
    let deadline = Instant::now() + Duration::from_secs(10);
    let clock = TestClock::at(deadline - Duration::from_secs(1));
    let (mut fut, _) = Probe::ready(7);
    let c = clock.clone();
    fut.on_drop = Some(Box::new(move || c.set(deadline)));
    let c = clock.clone();
    let r = settle_before(move || c.now(), deadline, never(), fut).await;
    assert_eq!(r, Settled::Late(7));
}

/// The caller's timer ends a pending wait, whatever the deciding clock reads: the logical
/// clock stays before its (distinct) deadline, the timer fires at its own instant, and the
/// clock is not read. The waiter runs in its own task, so the watchdog never polls it.
#[tokio::test(start_paused = true)]
async fn timer_ends_a_pending_wait() {
    // Both instants from one sample of the paused clock.
    let start = tokio::time::Instant::now();
    let deadline = start.into_std() + Duration::from_secs(5);
    let clock = TestClock::at(start.into_std());
    let timer = start + Duration::from_secs(10);
    let c = clock.clone();
    let waiter = tokio::spawn(async move {
        let r = settle_before(
            move || c.now(),
            deadline,
            timer,
            std::future::pending::<()>(),
        )
        .await;
        (r, tokio::time::Instant::now())
    });
    let (r, at) = waits::bounded(Duration::from_secs(60), waiter)
        .await
        .expect("the timer ended the wait")
        .unwrap();
    assert_eq!(r, Settled::Elapsed);
    assert_eq!(at, timer);
    assert_eq!(clock.readings(), 0);
}

#[test]
fn in_time_counts_only_in_time() {
    assert_eq!(Settled::InTime(1).in_time(), Some(1));
    assert_eq!(Settled::Late(1).in_time(), None);
    assert_eq!(Settled::<i32>::Elapsed.in_time(), None);
}
