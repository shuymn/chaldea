//! `TaskRegistry`: the one owner of every task that outlives a request (design §2.2).
//!
//! A task's entry is inserted under the mutex before the task is spawned, refused once the
//! registry is closing; the closing flag is set in the same critical section that snapshots
//! the entries, so no task registers after the snapshot. Each spawned task owns an exit
//! guard that removes its entry after everything the task's future owns has dropped, also
//! when the future is dropped before its first poll.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Instant;

use futures_util::future::{AbortHandle, AbortRegistration, Abortable};
use tokio::sync::Notify;

/// What a registered task is, for the shutdown report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    /// The accept loop; it owns the listener.
    Accept,
    /// One accepted connection.
    Connection,
    /// One upgraded socket.
    Socket,
    /// The observer of one plain write.
    Write,
    /// One command runner (PR 7c).
    Runner,
}

/// Task counts per kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KindCounts {
    pub accept: usize,
    pub connection: usize,
    pub socket: usize,
    pub write: usize,
    pub runner: usize,
}

impl KindCounts {
    fn slot(&mut self, kind: TaskKind) -> &mut usize {
        match kind {
            TaskKind::Accept => &mut self.accept,
            TaskKind::Connection => &mut self.connection,
            TaskKind::Socket => &mut self.socket,
            TaskKind::Write => &mut self.write,
            TaskKind::Runner => &mut self.runner,
        }
    }

    pub(crate) fn add(&mut self, kind: TaskKind) {
        *self.slot(kind) += 1;
    }

    /// The count of one kind.
    #[must_use]
    pub fn get(&self, kind: TaskKind) -> usize {
        let mut c = *self;
        *c.slot(kind)
    }

    #[must_use]
    pub fn total(&self) -> usize {
        self.accept + self.connection + self.socket + self.write + self.runner
    }

    /// `self` less `other`, kind by kind; `other` is a subset (the tasks of a snapshot still
    /// running), so nothing goes below zero.
    pub(crate) fn minus(&self, other: &KindCounts) -> KindCounts {
        let sub = |a: usize, b: usize| {
            debug_assert!(b <= a, "{b} tasks left of {a}");
            a.saturating_sub(b)
        };
        KindCounts {
            accept: sub(self.accept, other.accept),
            connection: sub(self.connection, other.connection),
            socket: sub(self.socket, other.socket),
            write: sub(self.write, other.write),
            runner: sub(self.runner, other.runner),
        }
    }
}

/// Registration refused: the registry is closing (shutdown began).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closing;

impl fmt::Display for Closing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the server is shutting down")
    }
}

impl std::error::Error for Closing {}

struct RegistryState {
    closing: bool,
    next_id: u64,
    tasks: HashMap<u64, (TaskKind, AbortHandle)>,
}

struct Inner {
    state: Mutex<RegistryState>,
    /// Notified (one stored permit) when the last entry leaves a closing registry.
    empty: Notify,
    #[cfg(test)]
    pub(crate) hooks: TestHooks,
}

/// Test-only points in `spawn` and `wait_empty_until`.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestHooks {
    pub(crate) between_insert_and_spawn: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    pub(crate) after_spawn: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    pub(crate) after_empty_check: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[cfg(test)]
fn run_hook(slot: &Mutex<Option<Box<dyn FnOnce() + Send>>>) {
    let f = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(f) = f {
        f();
    }
}

/// The registry. Cheap to clone; clones share it.
#[derive(Clone)]
pub struct TaskRegistry {
    inner: Arc<Inner>,
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for TaskRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.lock();
        f.debug_struct("TaskRegistry")
            .field("closing", &s.closing)
            .field("tasks", &s.tasks.len())
            .finish()
    }
}

impl TaskRegistry {
    #[must_use]
    pub fn new() -> TaskRegistry {
        TaskRegistry {
            inner: Arc::new(Inner {
                state: Mutex::new(RegistryState {
                    closing: false,
                    next_id: 0,
                    tasks: HashMap::new(),
                }),
                empty: Notify::new(),
                #[cfg(test)]
                hooks: TestHooks::default(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, RegistryState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Spawns `fut` as a registered task, or refuses once closing. The entry exists before
    /// the task is spawned, so it precedes the task's first action; the mutex is never held
    /// while spawning. Must be called inside a tokio runtime.
    ///
    /// # Errors
    ///
    /// [`Closing`] once shutdown began; `fut` is dropped unpolled.
    pub fn spawn(
        &self,
        kind: TaskKind,
        fut: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), Closing> {
        let (handle, registration) = AbortHandle::new_pair();
        let id = {
            let mut s = self.lock();
            if s.closing {
                return Err(Closing);
            }
            let id = s.next_id;
            s.next_id += 1;
            s.tasks.insert(id, (kind, handle));
            id
        };
        #[cfg(test)]
        run_hook(&self.inner.hooks.between_insert_and_spawn);
        tokio::spawn(Registered::new(
            Box::pin(fut),
            kind,
            registration,
            self.clone(),
            id,
        ));
        #[cfg(test)]
        run_hook(&self.inner.hooks.after_spawn);
        Ok(())
    }

    /// Sets `closing` and snapshots the entries, in one critical section; returns the
    /// snapshot's counts per kind.
    pub(crate) fn close(&self) -> KindCounts {
        let mut s = self.lock();
        s.closing = true;
        let mut counts = KindCounts::default();
        for (kind, _) in s.tasks.values() {
            counts.add(*kind);
        }
        counts
    }

    /// Waits until no entry is left or `deadline` passes; true when empty.
    pub(crate) async fn wait_empty_until(&self, deadline: Instant) -> bool {
        let deadline = tokio::time::Instant::from_std(deadline);
        loop {
            if self.is_empty() {
                return true;
            }
            #[cfg(test)]
            run_hook(&self.inner.hooks.after_empty_check);
            // A last exit between the check and this wait leaves a permit, so the wait
            // returns at once.
            tokio::select! {
                () = self.inner.empty.notified() => {}
                () = drain_timer(deadline) => return self.is_empty(),
            }
        }
    }

    /// Requests abort of every remaining entry (copied under the lock, aborted outside it)
    /// and returns their counts per kind.
    pub(crate) fn abort_remaining(&self) -> KindCounts {
        let handles: Vec<(TaskKind, AbortHandle)> = self
            .lock()
            .tasks
            .values()
            .map(|(k, h)| (*k, h.clone()))
            .collect();
        let mut counts = KindCounts::default();
        for (kind, handle) in handles {
            handle.abort();
            counts.add(kind);
        }
        counts
    }

    /// The registered tasks per kind (tests).
    #[cfg(test)]
    pub(crate) fn counts(&self) -> KindCounts {
        let mut counts = KindCounts::default();
        for (kind, _) in self.lock().tasks.values() {
            counts.add(*kind);
        }
        counts
    }

    /// Registered tasks that have not ended.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().tasks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether shutdown has closed the registry.
    #[must_use]
    pub fn is_closing(&self) -> bool {
        self.lock().closing
    }

    #[cfg(test)]
    pub(crate) fn hooks(&self) -> &TestHooks {
        &self.inner.hooks
    }

    fn remove(&self, id: u64) {
        let mut s = self.lock();
        s.tasks.remove(&id);
        let notify = s.closing && s.tasks.is_empty();
        drop(s);
        if notify {
            self.inner.empty.notify_one();
        }
    }
}

/// The drain's timer, one arm of [`TaskRegistry::wait_empty_until`]'s unbiased `select!`;
/// `ServeHandle::shutdown` decides from the registry, not from the wait (design §2.2).
#[allow(
    clippy::disallowed_methods,
    reason = "the drain wait's bound; shutdown counts from the registry at its abort pass (http-surface §2.2)"
)]
fn drain_timer(deadline: tokio::time::Instant) -> tokio::time::Sleep {
    tokio::time::sleep_until(deadline)
}

/// Removes its task's entry when dropped.
struct ExitGuard {
    registry: TaskRegistry,
    id: u64,
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.registry.remove(self.id);
    }
}

type BoxedTask = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The spawned future. Fields drop in declaration order: the task's future first, then the
/// guard, so the entry outlives everything the task owns.
struct Registered {
    fut: Abortable<BoxedTask>,
    /// Set as the thread's task kind for each poll, for the panic hook
    /// (`docs/design/daemon-lifecycle.md` §2.13).
    kind: TaskKind,
    _guard: ExitGuard,
}

impl Registered {
    fn new(
        fut: BoxedTask,
        kind: TaskKind,
        registration: AbortRegistration,
        registry: TaskRegistry,
        id: u64,
    ) -> Self {
        Registered {
            fut: Abortable::new(fut, registration),
            kind,
            _guard: ExitGuard { registry, id },
        }
    }
}

impl Future for Registered {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let _kind = crate::lifecycle::TaskKindGuard::enter(self.kind);
        Pin::new(&mut self.fut).poll(cx).map(|_| ())
    }
}
