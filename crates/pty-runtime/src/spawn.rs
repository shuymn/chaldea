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

use std::sync::{Mutex, MutexGuard, PoisonError};

static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// Runs `f` while holding the process-wide spawn lock. Every child the daemon creates
/// (`std::process::Command::spawn`, `tokio::process::Command::spawn`, process-wrap) must be
/// created inside such a closure; the native backend's own spawn takes the same lock.
///
/// `f` must not call `with_spawn_lock` or spawn a PTY session itself: the lock is not
/// reentrant. A poisoned lock is taken over.
pub fn with_spawn_lock<R>(f: impl FnOnce() -> R) -> R {
    let _guard = spawn_lock();
    f()
}

/// Proof that the process-wide spawn lock is held: only [`spawn_lock`] builds one, so a
/// function taking `&SpawnGuard` cannot be called under some other mutex.
pub(crate) struct SpawnGuard {
    // Declared first, so the flag clears before the lock is released.
    #[cfg(all(windows, any(test, feature = "fake")))]
    _mark: held_here::Mark,
    _guard: MutexGuard<'static, ()>,
}

pub(crate) fn spawn_lock() -> SpawnGuard {
    let guard = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    SpawnGuard {
        #[cfg(all(windows, any(test, feature = "fake")))]
        _mark: held_here::Mark::new(),
        _guard: guard,
    }
}

/// Whether some thread holds the lock (tests only): `try_lock` returned `WouldBlock`.
#[cfg(all(test, any(target_os = "macos", windows)))]
pub(crate) fn spawn_lock_held() -> bool {
    matches!(
        SPAWN_LOCK.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    )
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
