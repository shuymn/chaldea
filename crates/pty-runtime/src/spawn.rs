//! The process-wide spawn lock (`docs/design/pty-unix.md` §2.1, `docs/design/pty-windows.md`
//! §2.1).
//!
//! The lock matters on macOS (exec-error pipe) and on Windows (inheritable handles during
//! `CreatePseudoConsole`). std on macOS creates its exec-error pipe and marks it
//! close-on-exec in two steps; a fork by another thread in between gives that child the
//! pipe's write end across its exec, and the first spawn then waits until that child exits.
//! On Windows, `CreatePseudoConsole` holds inheritable duplicates of conhost's pipe ends
//! while it starts conhost; a concurrent `CreateProcess` with `bInheritHandles = TRUE` would
//! give them to an unrelated child, which would then keep the session's output open. Every
//! child creation in the process therefore runs under one lock: the native backend's spawn
//! and every closure passed to [`with_spawn_lock`].

#[cfg(not(test))]
use std::sync::Condvar;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

#[cfg(test)]
use probe::ExactCondvar as Condvar;

/// The lock: whether a thread holds it, and the wake-up of its waiters. The inner mutex is
/// held only to read or flip the flag, so a waiter can give up at a deadline.
struct SpawnLock {
    held: Mutex<bool>,
    freed: Condvar,
}

static SPAWN_LOCK: SpawnLock = SpawnLock {
    held: Mutex::new(false),
    freed: Condvar::new(),
};

impl SpawnLock {
    fn flag(&self) -> MutexGuard<'_, bool> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Releases `held` and blocks until a release wakes this thread, or for at most
    /// `timeout`; `true` when the wait timed out.
    fn wait(
        &'static self,
        held: MutexGuard<'static, bool>,
        timeout: Option<Duration>,
    ) -> (MutexGuard<'static, bool>, bool) {
        #[cfg(not(test))]
        match timeout {
            None => (
                self.freed
                    .wait(held)
                    .unwrap_or_else(PoisonError::into_inner),
                false,
            ),
            Some(t) => {
                let (h, woke) = self
                    .freed
                    .wait_timeout(held, t)
                    .unwrap_or_else(PoisonError::into_inner);
                (h, woke.timed_out())
            }
        }
        #[cfg(test)]
        self.freed.wait(held, timeout, || self.flag())
    }
}

/// Runs `f` while holding the process-wide spawn lock. Every child the daemon creates
/// (`std::process::Command::spawn`, `tokio::process::Command::spawn`, process-wrap) must be
/// created inside such a closure; the native backend's own spawn takes the same lock.
///
/// `f` must not call `with_spawn_lock` or spawn a PTY session itself: the lock is not
/// reentrant. A thread that panics while holding it releases it.
pub fn with_spawn_lock<R>(f: impl FnOnce() -> R) -> R {
    let _guard = spawn_lock();
    f()
}

/// Proof that the process-wide spawn lock is held: only [`spawn_lock`] and
/// [`spawn_lock_before`] build one, so a function taking `&SpawnGuard` cannot be called under
/// some other mutex.
///
/// It is `!Send`: the thread that acquired the lock releases it, and the Windows test marker
/// of that thread clears with it.
pub(crate) struct SpawnGuard {
    // Declared first, so the flag clears before the lock is released.
    #[cfg(all(windows, any(test, feature = "fake")))]
    _mark: held_here::Mark,
    _held: Held,
    _not_send: std::marker::PhantomData<*const ()>,
}

/// Releases the lock when dropped (also while unwinding).
struct Held;

impl Drop for Held {
    fn drop(&mut self) {
        *SPAWN_LOCK.flag() = false;
        // Every waiter wakes: one woken alone could find its deadline passed and return without
        // the lock, and the waiters behind it would sleep on with the lock free.
        SPAWN_LOCK.freed.notify_all();
    }
}

fn guard() -> SpawnGuard {
    SpawnGuard {
        #[cfg(all(windows, any(test, feature = "fake")))]
        _mark: held_here::Mark::new(),
        _held: Held,
        _not_send: std::marker::PhantomData,
    }
}

pub(crate) fn spawn_lock() -> SpawnGuard {
    let mut held = SPAWN_LOCK.flag();
    while *held {
        #[cfg(test)]
        let _waiting = probe::Waiting::new();
        held = SPAWN_LOCK.wait(held, None).0;
    }
    *held = true;
    drop(held);
    guard()
}

/// The lock, if it is acquired before `deadline`; `None` once the deadline has passed, even
/// with the lock free (the caller then creates nothing).
pub(crate) fn spawn_lock_before(deadline: Instant) -> Option<SpawnGuard> {
    let mut held = SPAWN_LOCK.flag();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        if !*held {
            break;
        }
        #[cfg(test)]
        let waiting = probe::Waiting::new();
        let (h, timed_out) = SPAWN_LOCK.wait(held, Some(left));
        held = h;
        #[cfg(test)]
        {
            drop(waiting);
            probe::woke(timed_out);
        }
        #[cfg(not(test))]
        let _ = timed_out;
    }
    *held = true;
    drop(held);
    Some(guard())
}

// `SpawnGuard` is not `Send`: were it, both impls below would apply and the call would be
// ambiguous, which fails the build.
#[cfg(test)]
const _: fn() = || {
    trait AmbiguousIfSend<A> {
        fn some_item() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}
    let _ = <SpawnGuard as AmbiguousIfSend<_>>::some_item;
};

/// Whether some thread holds the lock (tests only).
#[cfg(all(test, any(target_os = "macos", windows)))]
pub(crate) fn spawn_lock_held() -> bool {
    *SPAWN_LOCK.flag()
}

/// Whether the calling thread holds the lock (the Windows call trace records it).
#[cfg(all(windows, any(test, feature = "fake")))]
pub(crate) fn spawn_lock_held_here() -> bool {
    held_here::get()
}

/// A thread-local flag set while a `SpawnGuard` of this thread lives.
#[cfg(all(windows, any(test, feature = "fake")))]
mod held_here {
    use std::cell::Cell;

    thread_local! {
        static HELD: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) struct Mark;

    impl Mark {
        pub(super) fn new() -> Mark {
            HELD.with(|h| h.set(true));
            Mark
        }
    }

    impl Drop for Mark {
        fn drop(&mut self) {
            HELD.with(|h| h.set(false));
        }
    }

    pub(super) fn get() -> bool {
        HELD.with(Cell::get)
    }
}

/// Test probes of the lock's waiters.
#[cfg(test)]
pub(crate) mod probe {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
    use std::time::{Duration, Instant};

    /// The lock's condition variable in tests: waiters queue in order and wake only when a
    /// notification reaches them, never spuriously, so `notify_one` wakes exactly one waiter
    /// whatever the interleaving (std's futex condition variable can also return a waiter that
    /// was between releasing the mutex and blocking).
    pub(crate) struct ExactCondvar {
        queue: Mutex<VecDeque<Arc<Token>>>,
    }

    #[derive(Default)]
    struct Token {
        woken: Mutex<bool>,
        cv: Condvar,
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }

    impl ExactCondvar {
        pub(crate) const fn new() -> ExactCondvar {
            ExactCondvar {
                queue: Mutex::new(VecDeque::new()),
            }
        }

        /// Queues this thread while `held` is still locked (a release flips the flag under it,
        /// so no notification is missed), releases it, and blocks until notified or
        /// `timeout`; then locks again through `relock`. `true`: timed out.
        pub(crate) fn wait<'a>(
            &self,
            held: MutexGuard<'a, bool>,
            timeout: Option<Duration>,
            relock: impl FnOnce() -> MutexGuard<'a, bool>,
        ) -> (MutexGuard<'a, bool>, bool) {
            let token = Arc::new(Token::default());
            lock(&self.queue).push_back(token.clone());
            drop(held);
            let end = timeout.map(|t| Instant::now() + t);
            let mut woken = lock(&token.woken);
            while !*woken {
                match end {
                    None => woken = token.cv.wait(woken).unwrap_or_else(PoisonError::into_inner),
                    Some(end) => {
                        let left = end.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            break;
                        }
                        woken = token
                            .cv
                            .wait_timeout(woken, left)
                            .unwrap_or_else(PoisonError::into_inner)
                            .0;
                    }
                }
            }
            let mut timed_out = !*woken;
            drop(woken);
            if timed_out {
                let mut q = lock(&self.queue);
                match q.iter().position(|t| Arc::ptr_eq(t, &token)) {
                    Some(i) => {
                        q.remove(i);
                    }
                    // A notification took it off the queue in the meantime: it was woken.
                    None => timed_out = false,
                }
            }
            (relock(), timed_out)
        }

        fn wake(token: &Token) {
            *lock(&token.woken) = true;
            token.cv.notify_one();
        }

        pub(crate) fn notify_all(&self) {
            for t in lock(&self.queue).drain(..) {
                Self::wake(&t);
            }
        }

        #[allow(
            dead_code,
            reason = "the single-waiter wake, for comparison in reviews"
        )]
        pub(crate) fn notify_one(&self) {
            if let Some(t) = lock(&self.queue).pop_front() {
                Self::wake(&t);
            }
        }
    }

    /// How many threads wait on the condition variable.
    pub(crate) static WAITING: AtomicUsize = AtomicUsize::new(0);

    /// Counts one waiter while it lives.
    pub(super) struct Waiting;

    impl Waiting {
        pub(super) fn new() -> Waiting {
            WAITING.fetch_add(1, Ordering::SeqCst);
            Waiting
        }
    }

    impl Drop for Waiting {
        fn drop(&mut self) {
            WAITING.fetch_sub(1, Ordering::SeqCst);
        }
    }

    type Hook = Box<dyn FnMut(bool)>;

    thread_local! {
        static AFTER_WAKE: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Runs `f` in this thread after each wake of `spawn_lock_before`, still holding the inner
    /// mutex, with whether the wait timed out (a test's pause before the deadline check).
    pub(crate) fn after_wake(f: impl FnMut(bool) + 'static) {
        AFTER_WAKE.with(|h| *h.borrow_mut() = Some(Box::new(f)));
    }

    pub(super) fn woke(timed_out: bool) {
        AFTER_WAKE.with(|h| {
            if let Some(f) = h.borrow_mut().as_mut() {
                f(timed_out);
            }
        });
    }
}
