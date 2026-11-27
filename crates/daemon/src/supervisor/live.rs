//! The live map and its entries (design §2.11): one op lock per launch, the observation that
//! takes no lock, and the interfaces 10b uses.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use chaldea_core::adapter::Capabilities;
use chaldea_core::ids::{Agent, InputId, LaunchId, ThreadId};
use chaldea_pty_runtime::{ProcessIdentity, Session, WriteRecord, WriterObserver};
use tokio::sync::OwnedMutexGuard;

use super::tracker::TrackedSession;
use super::{Supervisor, lock};
use crate::deadline::{Backoff, settle_before};

/// The live map: a `std::sync::Mutex` held only for map operations.
#[derive(Default)]
pub(crate) struct LiveMap(Mutex<HashMap<LaunchId, Arc<LiveLaunch>>>);

impl LiveMap {
    pub(crate) fn lock(&self) -> MutexGuard<'_, HashMap<LaunchId, Arc<LiveLaunch>>> {
        lock(&self.0)
    }

    /// Every entry.
    pub(crate) fn all(&self) -> Vec<Arc<LiveLaunch>> {
        self.lock().values().cloned().collect()
    }
}

/// The op lock and what it guards: the launch's session while it is in its slot.
pub(crate) type Ops = Arc<tokio::sync::Mutex<Option<TrackedSession>>>;
/// A held op lock.
pub(crate) type OpGuard = OwnedMutexGuard<Option<TrackedSession>>;

/// One launch the daemon may still own a process of.
pub(crate) struct LiveLaunch {
    pub thread_id: ThreadId,
    pub launch_id: LaunchId,
    pub agent: Agent,
    pub caps: &'static Capabilities,
    /// A Codex launch without a first prompt.
    pub manual: bool,
    /// The launch's positional input, if it has one.
    pub positional: Option<InputId>,
    /// The CLI version the probe read before the reservation (C1 D11), recorded by
    /// `launch_started`.
    pub cli_version: Option<String>,
    /// Set before `Session::spawn`; never cleared.
    creation_started: AtomicBool,
    /// Set by `Supervisor::remove` before the entry leaves the live map; never cleared.
    removed: AtomicBool,
    /// Set with the stop's seal, under the op lock.
    stop_requested: AtomicBool,
    /// Set under the op lock when the session leaves its slot for good: a reader that finds
    /// the lock held then knows there is no session to read.
    detached: Arc<AtomicBool>,
    published: OnceLock<Published>,
    committed: OnceLock<CommittedStart>,
    /// The records of every seal of this launch.
    settled: Mutex<Vec<WriteRecord>>,
    /// The identities proven to belong to the launch beyond the backend's own (design §2.4):
    /// passed on every kill and close. Empty in the MVP.
    extra: Mutex<Vec<ProcessIdentity>>,
    pub(crate) ops: Ops,
    /// The deadline of the last `snapshot_before` (unit tests only).
    #[cfg(test)]
    pub(crate) snapshot_deadline: Mutex<Option<Instant>>,
    #[cfg(test)]
    pub(crate) hooks: LiveHooks,
}

/// Test points of one entry's observation and publication wait.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct LiveHooks {
    /// Runs in `await_published` each time it saw the launch unpublished and not removed.
    pub publication_waiting: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs in `await_published` after a wait that ended at or past the facts deadline,
    /// before the final read.
    pub final_wake: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs in `observe` after the positions were read, before `removed` is read again.
    pub after_positions: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[cfg(test)]
fn run_hook(slot: &Mutex<Option<Box<dyn FnOnce() + Send>>>) {
    let f = lock(slot).take();
    if let Some(f) = f {
        f();
    }
}

/// What the publication set (design §2.3 step 5).
pub(crate) struct Published {
    pub observer: WriterObserver,
    pub reference: u64,
}

/// The committed start: its seq, the identity, and the supervisor-clock instant read after
/// the commit.
#[derive(Debug, Clone)]
#[allow(dead_code, reason = "`seq` is 10b's")]
pub(crate) struct CommittedStart {
    pub seq: u64,
    pub identity: ProcessIdentity,
    pub at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionError {
    /// The session left the slot (exit recorded or being recorded).
    Gone,
    /// The lock was not taken by the deadline.
    Busy,
}

/// What [`LiveLaunch::snapshot_before`] got.
#[derive(Debug)]
pub(crate) enum Snapshot {
    Taken(chaldea_pty_runtime::Snapshot),
    /// No session in the slot: not yet published, or gone (exit recorded or being recorded).
    NoSession,
    /// Past the deadline, the op lock held, the VT lock not taken in time, or no answer in
    /// time.
    Busy,
    /// The VT failed (or the copy panicked): no snapshot will be taken.
    VtFailed,
}

impl Snapshot {
    /// The snapshot, if one was taken.
    pub(crate) fn taken(self) -> Option<chaldea_pty_runtime::Snapshot> {
        match self {
            Snapshot::Taken(s) => Some(s),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchObservation {
    /// No child creation has begun.
    Reserved,
    /// Published: the reference and the per-class positions.
    Started {
        reference: u64,
        last_started: [u64; 3],
    },
    /// Removed, creation begun and unpublished, or positions not read in time.
    Unobservable,
}

impl LiveLaunch {
    pub(crate) fn new(
        thread_id: ThreadId,
        launch_id: LaunchId,
        agent: Agent,
        caps: &'static Capabilities,
        manual: bool,
        positional: Option<InputId>,
    ) -> Self {
        LiveLaunch {
            thread_id,
            launch_id,
            agent,
            caps,
            manual,
            positional,
            cli_version: None,
            creation_started: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
            detached: Arc::new(AtomicBool::new(false)),
            published: OnceLock::new(),
            committed: OnceLock::new(),
            settled: Mutex::new(Vec::new()),
            extra: Mutex::new(Vec::new()),
            ops: Arc::new(tokio::sync::Mutex::new(None)),
            #[cfg(test)]
            snapshot_deadline: Mutex::new(None),
            #[cfg(test)]
            hooks: LiveHooks::default(),
        }
    }

    pub(crate) fn set_creation_started(&self) {
        self.creation_started.store(true, Ordering::SeqCst);
    }

    pub(crate) fn creation_started(&self) -> bool {
        self.creation_started.load(Ordering::SeqCst)
    }

    /// Marks the entry removed; `Supervisor::remove` calls it before the entry leaves the map.
    pub(crate) fn mark_removed(&self) {
        self.removed.store(true, Ordering::SeqCst);
    }

    pub(crate) fn removed(&self) -> bool {
        self.removed.load(Ordering::SeqCst)
    }

    /// The observation the hook intake and the settle read (design §2.11 "Observation"),
    /// taking no lock: removed → `Unobservable`; published → the positions the writer
    /// publishes ([`WriterObserver::published_last_started`] with `deadline`), `Started`
    /// unless they were not read in time or the entry was removed meanwhile; creation begun
    /// → `Unobservable`; else `Reserved`.
    pub(crate) fn observe(&self, deadline: Option<Instant>) -> LaunchObservation {
        if self.removed() {
            return LaunchObservation::Unobservable;
        }
        if let Some(p) = self.published() {
            let Some(last_started) = p.observer.published_last_started(deadline) else {
                return LaunchObservation::Unobservable;
            };
            #[cfg(test)]
            run_hook(&self.hooks.after_positions);
            if self.removed() {
                return LaunchObservation::Unobservable;
            }
            return LaunchObservation::Started {
                reference: p.reference,
                last_started,
            };
        }
        if self.creation_started() {
            LaunchObservation::Unobservable
        } else {
            LaunchObservation::Reserved
        }
    }

    /// Waits, until `f` (real time), while the launch's creation began and it is neither
    /// published nor removed (send §2.5 "Intake facts"). It polls those atomics and takes no
    /// lock; each sleep is capped by the time left to `f`, and the predicate is read once more
    /// after the last one.
    pub(crate) fn await_published(&self, f: Instant) {
        let mut backoff = Backoff::default();
        loop {
            if !self.creation_started() || self.published().is_some() || self.removed() {
                return;
            }
            #[cfg(test)]
            run_hook(&self.hooks.publication_waiting);
            let now = Instant::now();
            if now >= f {
                return;
            }
            backoff.wait(f - now);
            #[cfg(test)]
            if Instant::now() >= f {
                run_hook(&self.hooks.final_wake);
            }
        }
    }

    pub(crate) fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::SeqCst)
    }

    pub(crate) fn published(&self) -> Option<&Published> {
        self.published.get()
    }

    pub(crate) fn set_published(&self, p: Published) {
        let _ = self.published.set(p);
    }

    pub(crate) fn committed(&self) -> Option<&CommittedStart> {
        self.committed.get()
    }

    pub(crate) fn set_committed(&self, c: CommittedStart) {
        let _ = self.committed.set(c);
    }

    /// The launch's full recorded list (design §2.1 rule 5).
    pub(crate) fn extra(&self) -> Vec<ProcessIdentity> {
        lock(&self.extra).clone()
    }

    /// Records an identity proven to belong to the launch.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "a later producer records identities")
    )]
    pub(crate) fn record_extra(&self, id: ProcessIdentity) {
        lock(&self.extra).push(id);
    }

    /// The records of every seal so far.
    pub(crate) fn settled(&self) -> Vec<WriteRecord> {
        lock(&self.settled).clone()
    }

    /// Appends a seal's records.
    pub(crate) fn add_settled(&self, records: Vec<WriteRecord>) {
        lock(&self.settled).extend(records);
    }

    /// Seals the session held in `guard` and appends its records to `settled`.
    pub(crate) fn seal(&self, guard: &OpGuard) {
        if let Some(s) = guard.as_ref() {
            self.add_settled(s.seal());
        }
    }

    /// The stop's seal: seals, sets `stop_requested`, and takes the session out, in one
    /// step with no await (design §2.3 Stop step 3).
    pub(crate) fn seal_for_stop(&self, guard: &mut OpGuard) -> Option<TrackedSession> {
        self.seal(guard);
        self.stop_requested.store(true, Ordering::SeqCst);
        self.take_session(guard)
    }

    /// Takes the session out of its slot for good, marking the launch detached first.
    pub(crate) fn take_session(&self, guard: &mut OpGuard) -> Option<TrackedSession> {
        self.detached.store(true, Ordering::SeqCst);
        guard.take()
    }

    /// The op lock taken by `deadline` (supervisor clock); `None`: not in time.
    pub(crate) async fn lock_before(&self, sup: &Supervisor, deadline: Instant) -> Option<OpGuard> {
        settle_before(
            || sup.now(),
            deadline,
            sup.timer(deadline),
            self.ops.clone().lock_owned(),
        )
        .await
        .in_time()
    }

    /// One VT snapshot by `deadline` (supervisor clock), never waiting for the op lock
    /// (send §2.9 D2): a blocking task that, once running and before `deadline`, takes the op
    /// lock with one `try_lock`, takes the session's screen, releases the lock, and copies the
    /// screen with the session's own deadline; the join is awaited with `settle_before` until
    /// `deadline`.
    pub(crate) async fn snapshot_before(&self, sup: &Supervisor, deadline: Instant) -> Snapshot {
        #[cfg(test)]
        {
            *lock(&self.snapshot_deadline) = Some(deadline);
        }
        // Not yet published: the spawn may hold the op lock, and there is no session to read.
        if self.published().is_none() {
            return Snapshot::NoSession;
        }
        let ops = self.ops.clone();
        let detached = self.detached.clone();
        let real = sup.real(deadline);
        #[cfg(test)]
        let pauses = sup.cfg.pauses.clone();
        let task = sup.rt.spawn_blocking(move || {
            if Instant::now() >= real {
                return Snapshot::Busy;
            }
            // The op lock only while the screen is taken: the copy waits for the VT lock
            // without holding up a stop.
            let screen = match ops.try_lock() {
                // Held by the op that took the session out (a stop, an exit, the shutdown).
                Err(_) if detached.load(Ordering::SeqCst) => return Snapshot::NoSession,
                Err(_) => return Snapshot::Busy,
                Ok(slot) => match slot.as_ref() {
                    None => return Snapshot::NoSession,
                    Some(s) => s.screen(),
                },
            };
            #[cfg(test)]
            pauses.blocking(super::Pause::SnapshotCopy);
            match screen.snapshot(real) {
                Ok(s) => Snapshot::Taken(s),
                Err(_) if screen.vt_failed() => Snapshot::VtFailed,
                Err(_) => Snapshot::Busy,
            }
        });
        match settle_before(|| sup.now(), deadline, sup.timer(deadline), task)
            .await
            .in_time()
        {
            Some(Ok(s)) => s,
            None => Snapshot::Busy,
            // The task panicked.
            Some(Err(_)) => Snapshot::VtFailed,
        }
    }

    /// Takes the op lock by `deadline` (supervisor clock) and runs `f` inline; `f` must not
    /// block.
    pub(crate) async fn with_session<R>(
        &self,
        sup: &Supervisor,
        deadline: Instant,
        f: impl FnOnce(&Session) -> R,
    ) -> Result<R, SessionError> {
        let guard = self
            .lock_before(sup, deadline)
            .await
            .ok_or(SessionError::Busy)?;
        match guard.as_ref() {
            Some(s) => Ok(f(s)),
            None => Err(SessionError::Gone),
        }
    }
}
