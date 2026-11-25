//! Ownership as a type (`docs/design/supervisor.md` §2.9, SD10): every process the supervisor
//! creates carries a [`TrackToken`] from before its creation, and only the release rule frees
//! one.

use std::io;
use std::process::{Child, Command, ExitStatus};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use chaldea_pty_runtime::{CloseBounds, CloseOutcome, ProcessIdentity, Session};
use tokio::sync::Notify;

use crate::deadline::settle_before;
use crate::lifecycle::spawn_locked_before;

use super::{Pause, Pauses};

#[derive(Debug)]
struct Count {
    accepting: bool,
    count: usize,
}

/// The admission flag and the count of tokens not yet released, under one mutex.
pub(crate) struct Tracker {
    state: Mutex<Count>,
    /// Woken when the count reaches 0.
    zero: Notify,
    /// Test points of `empty_before`: right after the wake-up is enabled, and right after
    /// the count was read.
    #[cfg(test)]
    pub(crate) hooks: TrackerHooks,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TrackerHooks {
    pub(crate) after_enable: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    pub(crate) after_count: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[cfg(test)]
fn run_hook(h: &Mutex<Option<Box<dyn FnOnce() + Send>>>) {
    let f = h.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(f) = f {
        f();
    }
}

impl std::fmt::Debug for Tracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracker")
            .field("state", &*self.lock())
            .finish()
    }
}

impl Default for Tracker {
    fn default() -> Self {
        Tracker {
            state: Mutex::new(Count {
                accepting: true,
                count: 0,
            }),
            zero: Notify::new(),
            #[cfg(test)]
            hooks: TrackerHooks::default(),
        }
    }
}

impl Tracker {
    fn lock(&self) -> MutexGuard<'_, Count> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A token, counted, while admission is open; `None` once it closed.
    pub(crate) fn admit(self: &Arc<Self>) -> Option<TrackToken> {
        let mut s = self.lock();
        if !s.accepting {
            return None;
        }
        s.count += 1;
        Some(TrackToken {
            tracker: self.clone(),
        })
    }

    pub(crate) fn accepting(&self) -> bool {
        self.lock().accepting
    }

    /// Runs `f` under the tracker's mutex, only while admission is open.
    pub(crate) fn while_accepting<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        let s = self.lock();
        if !s.accepting {
            return None;
        }
        let r = f();
        drop(s);
        Some(r)
    }

    /// Closes admission; takes the same mutex as [`Tracker::while_accepting`], so a
    /// publication either completed before or does not happen.
    pub(crate) fn close_admission(&self) {
        self.lock().accepting = false;
    }

    /// Whether the tracker's mutex is held right now (admission is being read or changed).
    #[cfg(test)]
    pub(crate) fn mutex_held(&self) -> bool {
        matches!(
            self.state.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        )
    }

    /// Tokens not yet released.
    pub(crate) fn count(&self) -> usize {
        self.lock().count
    }

    fn release_one(&self) {
        let mut s = self.lock();
        s.count = s.count.saturating_sub(1);
        let zero = s.count == 0;
        drop(s);
        if zero {
            self.zero.notify_waiters();
        }
    }

    /// Whether the count is 0 by `end` (real time): the zero wake-up is created and enabled
    /// first, then the count read, then the wake-up awaited with `settle_before`.
    pub(crate) async fn empty_before(&self, end: Instant) -> bool {
        loop {
            let notified = self.zero.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            #[cfg(test)]
            run_hook(&self.hooks.after_enable);
            let count = self.count();
            #[cfg(test)]
            run_hook(&self.hooks.after_count);
            if count == 0 {
                return true;
            }
            let woke = settle_before(
                Instant::now,
                end,
                tokio::time::Instant::from_std(end),
                notified,
            )
            .await;
            if woke.in_time().is_none() {
                return false;
            }
        }
    }
}

/// Not `Clone`; no constructor but [`Tracker::admit`]. Only [`TrackToken::release`]
/// decrements the count: a token dropped any other way, by an unwind included, stays counted
/// for the rest of the process.
#[derive(Debug)]
pub(crate) struct TrackToken {
    tracker: Arc<Tracker>,
}

impl TrackToken {
    pub(crate) fn release(self) {
        self.tracker.release_one();
    }
}

/// Whether a close released everything: the tree proven dead, every handle released, and no
/// session thread left running (§2.9 "The release rule").
pub(crate) fn released_all(o: &CloseOutcome) -> bool {
    let r = &o.released;
    r.input && r.control && r.output && r.tree_dead && o.abandoned.is_empty()
}

/// A [`Session`] with its token.
pub(crate) struct TrackedSession {
    session: Session,
    token: TrackToken,
}

impl TrackedSession {
    pub(crate) fn new(session: Session, token: TrackToken) -> Self {
        TrackedSession { session, token }
    }

    /// `Session::close`; the token is released only by the release rule.
    pub(crate) fn close(self, extra: &[ProcessIdentity], bounds: CloseBounds) -> CloseOutcome {
        let TrackedSession { session, token } = self;
        let o = session.close(extra, bounds);
        if released_all(&o) {
            token.release();
        }
        o
    }

    /// `Session::close_by` (the stop's close); the same release rule.
    pub(crate) fn close_by(
        self,
        extra: &[ProcessIdentity],
        kill_deadline: Instant,
        bounds: CloseBounds,
    ) -> CloseOutcome {
        let TrackedSession { session, token } = self;
        let o = session.close_by(extra, kill_deadline, bounds);
        if released_all(&o) {
            token.release();
        }
        o
    }
}

impl std::ops::Deref for TrackedSession {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.session
    }
}

/// A child the daemon created outside a PTY (the git probe), with its token.
#[derive(Debug)]
pub(crate) struct TrackedChild {
    child: Option<Child>,
    token: Option<TrackToken>,
}

impl TrackedChild {
    /// Through `spawn_locked_before`: holding the spawn lock, takes a token from
    /// [`Tracker::admit`], then checks the deadline as the last step before the OS spawn,
    /// which runs outside the tracker's mutex. `None`: the spawn lock not taken by the
    /// deadline, admission closed (no token), or the deadline passed after the admit (the
    /// token released); no child in each. On `Err` it releases the token.
    pub(crate) fn spawn(
        cmd: &mut Command,
        tracker: &Arc<Tracker>,
        deadline: Instant,
        pauses: &dyn Pauses,
    ) -> Option<io::Result<Self>> {
        let mut taken: Option<TrackToken> = None;
        let spawned = spawn_locked_before(cmd, deadline, || {
            pauses.blocking(Pause::GitBeforeAdmit);
            let Some(t) = tracker.admit() else {
                return false;
            };
            pauses.blocking(Pause::GitAfterAdmit);
            if chaldea_pty_runtime::host::act_before(deadline, || ()).is_none() {
                t.release();
                return false;
            }
            taken = Some(t);
            true
        })?;
        match spawned {
            Ok(child) => Some(Ok(TrackedChild {
                child: Some(child),
                token: taken,
            })),
            Err(e) => {
                if let Some(t) = taken {
                    t.release();
                }
                Some(Err(e))
            }
        }
    }

    /// `Child::kill` on its own handle; nothing once reaped.
    pub(crate) fn kill(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
        }
    }

    /// The child's stdout, taken once.
    pub(crate) fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.as_mut().and_then(|c| c.stdout.take())
    }

    /// `try_wait` on its handle; on `Some(status)` it drops the `Child` (closing its
    /// handles), then releases the token.
    pub(crate) fn try_reap(&mut self) -> Option<ExitStatus> {
        let status = self.child.as_mut()?.try_wait().ok()??;
        drop(self.child.take());
        if let Some(t) = self.token.take() {
            t.release();
        }
        Some(status)
    }

    /// Whether the child was reaped.
    pub(crate) fn reaped(&self) -> bool {
        self.child.is_none()
    }
}
