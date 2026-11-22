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
        let (reached_tx, reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        *self
            .removal_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((reached_tx, release_rx));
        (reached_rx, release_tx)
    }

    pub(crate) async fn before_ticket_removal(&self) {
        let gate = self
            .removal_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((reached, release)) = gate {
            let _ = reached.send(());
            let _ = release.await;
        }
    }
}
