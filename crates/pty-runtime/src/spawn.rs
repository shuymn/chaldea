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

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

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
        held = SPAWN_LOCK
            .freed
            .wait(held)
            .unwrap_or_else(PoisonError::into_inner);
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
        let (h, woke) = SPAWN_LOCK
            .freed
            .wait_timeout(held, left)
            .unwrap_or_else(PoisonError::into_inner);
        held = h;
        #[cfg(test)]
        {
            drop(waiting);
            probe::woke(woke.timed_out());
        }
        #[cfg(not(test))]
        let _ = woke;
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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
