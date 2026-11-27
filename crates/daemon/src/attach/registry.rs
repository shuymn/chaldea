//! The per-launch attach registry (`docs/design/t1-attach.md` D2, I-2): the operating
//! socket of each launch, its grant, and the launch's fence word for attach writes and
//! resizes. Nothing here calls the session: the caller unsubscribes the replaced socket on
//! the blocking pool after the registry lock is released.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chaldea_core::ids::LaunchId;
use chaldea_pty_runtime::{Fence, SessionHandle, SubscriptionId};
use tokio::sync::Notify;

use super::input::advance_tail;
use crate::supervisor::lock;

/// A socket's replacement signal: set once, then notified.
#[derive(Default)]
pub(crate) struct Replaced {
    set: AtomicBool,
    notify: Notify,
}

impl Replaced {
    fn fire(&self) {
        self.set.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    /// Resolves once the socket was replaced.
    pub(crate) async fn wait(&self) {
        while !self.set.load(Ordering::Acquire) {
            self.notify.notified().await;
        }
    }
}

/// A candidate's parts, registered by [`LaunchAttach::swap`] once its subscription exists;
/// the launch's operating socket from then on.
pub(crate) struct Candidate {
    pub socket: u64,
    pub sub: SubscriptionId,
    /// The socket's output sink accepts bytes while set.
    pub active: Arc<AtomicBool>,
    pub handle: SessionHandle,
    pub replaced: Arc<Replaced>,
}

/// The subscription a swap or an end leaves to the caller to remove.
pub(crate) type Removed = (SessionHandle, SubscriptionId);

/// One launch's attach state.
pub(crate) struct LaunchAttach {
    /// Moved at every swap and by every terminal path of the current socket: a unit or a
    /// resize whose `expect` is an older grant is refused.
    pub word: Arc<AtomicU64>,
    current: Mutex<Option<Candidate>>,
    /// The marker rule's context (R-6), in write order across sockets.
    pub marks: Mutex<Marks>,
    /// Notified when a raw unit settles.
    pub settled: Notify,
}

/// The raw input the PTY received, and the raw units not yet settled.
#[derive(Default)]
pub(crate) struct Marks {
    /// The last bytes of raw input the writer wrote.
    pub tail: Vec<u8>,
    /// Raw units submitted and not yet settled: the next raw unit is decided only once
    /// every earlier one settled, so `tail` is what precedes it in the PTY.
    pub unsettled: usize,
}

impl LaunchAttach {
    /// The candidate becomes current with the next grant; the replaced socket's sink stops,
    /// it is told to close 4003, and its subscription is returned for removal.
    pub(crate) fn swap(&self, c: Candidate) -> (u64, Option<Removed>) {
        let mut cur = lock(&self.current);
        let grant = self.word.load(Ordering::Acquire).wrapping_add(1);
        self.word.store(grant, Ordering::Release);
        let old = cur.replace(c);
        // Under the lock, with the swap: the replaced sink stops and its socket is told.
        let removed = old.map(|o| {
            o.active.store(false, Ordering::Release);
            o.replaced.fire();
            (o.handle, o.sub)
        });
        drop(cur);
        (grant, removed)
    }

    /// Revokes `socket`'s grant if it is still current: its permitted work settles, nothing
    /// else of it reaches the PTY. The entry stays until [`LaunchAttach::end`].
    pub(crate) fn revoke(&self, socket: u64) {
        let cur = lock(&self.current);
        if cur.as_ref().is_some_and(|c| c.socket == socket) {
            self.word.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// `socket`'s exit: when it is still current, the word moves, then its entry is
    /// cleared and its subscription returned for removal. Another socket's entry is never
    /// touched.
    pub(crate) fn end(&self, socket: u64) -> Option<Removed> {
        let mut cur = lock(&self.current);
        if cur.as_ref().is_none_or(|c| c.socket != socket) {
            return None;
        }
        self.word.fetch_add(1, Ordering::AcqRel);
        let c = cur.take()?;
        c.active.store(false, Ordering::Release);
        Some((c.handle, c.sub))
    }

    /// A raw unit's settlement, from its write record (on the writer thread, in write
    /// order): `written` bytes of `unit` reached the PTY.
    pub(crate) fn raw_settled(&self, unit: &[u8], written: u64) {
        let n = usize::try_from(written)
            .unwrap_or(usize::MAX)
            .min(unit.len());
        let mut m = lock(&self.marks);
        advance_tail(&mut m.tail, &unit[..n]);
        m.unsettled = m.unsettled.saturating_sub(1);
        drop(m);
        self.settled.notify_waiters();
    }

    /// A raw unit that was counted and then not queued.
    pub(crate) fn raw_unqueued(&self) {
        let mut m = lock(&self.marks);
        m.unsettled = m.unsettled.saturating_sub(1);
        drop(m);
        self.settled.notify_waiters();
    }

    /// The fence of a socket holding `grant`.
    pub(crate) fn fence(&self, grant: u64) -> Fence {
        Fence {
            word: self.word.clone(),
            expect: grant,
        }
    }

    fn idle(&self) -> bool {
        lock(&self.current).is_none()
    }
}

/// Every launch's attach state, and the socket ids.
#[derive(Default)]
pub(crate) struct Registry {
    launches: Mutex<HashMap<LaunchId, Arc<LaunchAttach>>>,
    next_socket: AtomicU64,
}

impl Registry {
    pub(crate) fn socket_id(&self) -> u64 {
        self.next_socket.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// `l`'s state, created when new. Entries of launches that are no longer live and have
    /// no socket are retired first, so they do not accumulate.
    pub(crate) fn launch(&self, l: LaunchId, live: impl Fn(LaunchId) -> bool) -> Arc<LaunchAttach> {
        let mut map = lock(&self.launches);
        map.retain(|k, la| *k == l || live(*k) || !la.idle());
        map.entry(l)
            .or_insert_with(|| {
                Arc::new(LaunchAttach {
                    word: Arc::new(AtomicU64::new(0)),
                    current: Mutex::new(None),
                    marks: Mutex::new(Marks::default()),
                    settled: Notify::new(),
                })
            })
            .clone()
    }

    /// Retires every entry whose launch is no longer live and has no socket.
    pub(crate) fn retire(&self, live: impl Fn(LaunchId) -> bool) {
        lock(&self.launches).retain(|k, la| live(*k) || !la.idle());
    }

    /// The entries kept (tests).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        lock(&self.launches).len()
    }
}
