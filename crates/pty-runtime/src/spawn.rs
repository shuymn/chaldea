//! The process-wide spawn lock (`docs/design/pty-unix.md` §2.1).
//!
//! std on macOS creates its exec-error pipe and marks it close-on-exec in two steps; a
//! fork by another thread in between gives that child the pipe's write end across its
//! exec, and the first spawn then waits until that child exits. Every child creation in
//! the process therefore runs under one lock: the native backend's spawn and every
//! closure passed to [`with_spawn_lock`].

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

pub(crate) fn spawn_lock() -> MutexGuard<'static, ()> {
    SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether some thread holds the lock (tests only).
#[cfg(all(test, target_os = "macos"))]
pub(crate) fn spawn_lock_held() -> bool {
    matches!(
        SPAWN_LOCK.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    )
}
