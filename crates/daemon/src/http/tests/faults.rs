//! The HTTP fault points (design §7): each is inert until a test arms it.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use tokio::sync::oneshot;

type Hook = Box<dyn FnOnce() + Send>;
/// Tells the test the gate was reached; admission waits for the release.
type Gate = (oneshot::Sender<()>, oneshot::Receiver<()>);

#[derive(Default)]
pub(crate) struct Points {
    /// Polls of request bodies by the application, since the server started.
    pub(crate) body_polls: AtomicUsize,
    /// Run once, on the blocking thread, before a store read starts: after skipping the
    /// given number of reads.
    pub(crate) before_read: Mutex<Option<(usize, Hook)>>,
    /// Run once, on the blocking thread, after a store read returned: after skipping the
    /// given number of reads.
    pub(crate) after_read: Mutex<Option<(usize, Hook)>>,
    /// Returned by the next accepts instead of accepting.
    pub(crate) accept_errors: Mutex<VecDeque<io::ErrorKind>>,
    /// Write jobs `run_write` submitted to the store.
    pub(crate) submits: AtomicUsize,
    /// Held once by an upgrade admission between its final read and the ticket removal.
    pub(crate) removal_gate: Mutex<Option<Gate>>,
    /// Held once by a read's or a write reply's waiter before it first polls its result
    /// against the cutoff.
    pub(crate) cutoff_gate: Mutex<Option<Gate>>,
    /// Told the first time the next registered socket task parks.
    pub(crate) first_park: Mutex<Option<oneshot::Sender<()>>>,
    /// The upgrade steps recorded, in order.
    pub(crate) steps: Mutex<Vec<&'static str>>,
}

/// Arms a gate: the returned receiver fires when the gate is reached, and the waiter goes
/// on once the returned sender is used or dropped.
fn arm(slot: &Mutex<Option<Gate>>) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some((reached_tx, release_rx));
    (reached_rx, release_tx)
}

/// Passes a gate: if armed, tells the test and waits for the release.
async fn pass(slot: &Mutex<Option<Gate>>) {
    let gate = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some((reached, release)) = gate {
        let _ = reached.send(());
        let _ = release.await;
    }
}

/// The armed hook if its skip count is used up; otherwise counts one skip.
fn take(slot: &Mutex<Option<(usize, Hook)>>) -> Option<Hook> {
    let mut s = slot.lock().unwrap_or_else(PoisonError::into_inner);
    match s.as_mut() {
        Some((0, _)) => s.take().map(|(_, f)| f),
        Some((n, _)) => {
            *n -= 1;
            None
        }
        None => None,
    }
}

impl Points {
    pub(crate) fn body_polled(&self) {
        self.body_polls.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn polls(&self) -> usize {
        self.body_polls.load(Ordering::SeqCst)
    }

    pub(crate) fn before_read(&self) {
        if let Some(f) = take(&self.before_read) {
            f();
        }
    }

    pub(crate) fn after_read(&self) {
        if let Some(f) = take(&self.after_read) {
            f();
        }
    }

    /// Runs `f` before the read that follows `skip` other reads.
    pub(crate) fn arm_before_read(&self, skip: usize, f: impl FnOnce() + Send + 'static) {
        *self
            .before_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((skip, Box::new(f)));
    }

    /// Runs `f` after the read that follows `skip` other reads.
    pub(crate) fn arm_after_read(&self, skip: usize, f: impl FnOnce() + Send + 'static) {
        *self
            .after_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((skip, Box::new(f)));
    }

    pub(crate) fn arm_accept_errors(&self, kinds: &[io::ErrorKind]) {
        self.accept_errors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(kinds.iter().copied());
    }

    pub(crate) fn accept_error(&self) -> Option<io::Error> {
        self.accept_errors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .map(io::Error::from)
    }

    pub(crate) fn write_submitted(&self) {
        self.submits.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn submits(&self) -> usize {
        self.submits.load(Ordering::SeqCst)
    }

    /// Arms the gate: the returned receiver fires when an admission reaches it, and the
    /// admission goes on once the returned sender is used or dropped.
    pub(crate) fn arm_removal_gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm(&self.removal_gate)
    }

    pub(crate) async fn before_ticket_removal(&self) {
        pass(&self.removal_gate).await;
    }

    /// Arms the cutoff gate, as [`Points::arm_removal_gate`]; after the release the waiter
    /// also waits (at most 10 s) until its result is in, so it then polls a ready result.
    pub(crate) fn arm_cutoff_gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm(&self.cutoff_gate)
    }

    pub(crate) async fn before_cutoff_wait(&self, ready: impl Fn() -> bool) {
        let armed = self
            .cutoff_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some();
        if !armed {
            return;
        }
        pass(&self.cutoff_gate).await;
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready() {
            assert!(
                std::time::Instant::now() < until,
                "the result never came in"
            );
            tokio::task::yield_now().await;
        }
    }

    pub(crate) fn step(&self, step: &'static str) {
        self.steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(step);
    }

    pub(crate) fn steps(&self) -> Vec<&'static str> {
        self.steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The returned receiver fires the first time the next registered socket task parks.
    pub(crate) fn arm_first_park(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        *self
            .first_park
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(tx);
        rx
    }

    pub(crate) fn signal_first_park<F>(&self, fut: F) -> impl Future<Output = ()> + Send + 'static
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut signal = self
            .first_park
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let mut fut = Box::pin(fut);
        std::future::poll_fn(move |cx| {
            let r = fut.as_mut().poll(cx);
            if r.is_pending()
                && let Some(s) = signal.take()
            {
                let _ = s.send(());
            }
            r
        })
    }
}
