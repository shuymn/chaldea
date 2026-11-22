//! The HTTP fault points (design §7): each is inert until a test arms it.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

type Hook = Box<dyn FnOnce() + Send>;

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
}
