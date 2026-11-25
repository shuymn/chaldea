//! The live map and its entries (design §2.11): one op lock per launch, the observation that
//! never waits, and the interfaces 10b uses.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::Instant;

use chaldea_core::adapter::Capabilities;
use chaldea_core::ids::{Agent, LaunchId, ThreadId};
use chaldea_pty_runtime::{ProcessIdentity, Session, WriteRecord, WriterObserver};
use tokio::sync::OwnedMutexGuard;

use super::tracker::TrackedSession;
use super::{Supervisor, lock};
use crate::deadline::settle_before;

/// The live map: a `std::sync::Mutex` held only for map operations.
#[derive(Default)]
pub(crate) struct LiveMap(Mutex<HashMap<LaunchId, Arc<LiveLaunch>>>);

impl LiveMap {
    pub(crate) fn lock(&self) -> MutexGuard<'_, HashMap<LaunchId, Arc<LiveLaunch>>> {
        lock(&self.0)
    }

    /// The map, or `None` while another holder has it (never waits).
    pub(crate) fn try_lock(&self) -> Option<MutexGuard<'_, HashMap<LaunchId, Arc<LiveLaunch>>>> {
        match self.0.try_lock() {
            Ok(g) => Some(g),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
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
#[allow(dead_code, reason = "`agent` and `caps` are 10b's")]
pub(crate) struct LiveLaunch {
    pub thread_id: ThreadId,
    pub launch_id: LaunchId,
    pub agent: Agent,
    pub caps: &'static Capabilities,
    /// A Codex launch without a first prompt.
    pub manual: bool,
    /// Set before `Session::spawn`; never cleared.
    creation_started: AtomicBool,
    /// Set with the stop's seal, under the op lock.
    stop_requested: AtomicBool,
    published: OnceLock<Published>,
    committed: OnceLock<CommittedStart>,
    /// The records of every seal of this launch.
    settled: Mutex<Vec<WriteRecord>>,
    /// The identities proven to belong to the launch beyond the backend's own (design §2.4):
    /// passed on every kill and close. Empty in the MVP.
    extra: Mutex<Vec<ProcessIdentity>>,
    pub(crate) ops: Ops,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchObservation {
    /// No child creation has begun.
    Reserved,
    /// Published: the reference and the per-class positions.
    Started {
        reference: u64,
        last_started: [u64; 3],
    },
    /// Creation begun and unpublished, contended, or no entry.
    Unobservable,
}

impl LiveLaunch {
    pub(crate) fn new(
        thread_id: ThreadId,
        launch_id: LaunchId,
        agent: Agent,
        caps: &'static Capabilities,
        manual: bool,
    ) -> Self {
        LiveLaunch {
            thread_id,
            launch_id,
            agent,
            caps,
            manual,
            creation_started: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
            published: OnceLock::new(),
            committed: OnceLock::new(),
            settled: Mutex::new(Vec::new()),
            extra: Mutex::new(Vec::new()),
            ops: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub(crate) fn set_creation_started(&self) {
        self.creation_started.store(true, Ordering::SeqCst);
    }

    pub(crate) fn creation_started(&self) -> bool {
        self.creation_started.load(Ordering::SeqCst)
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
            let records = s.seal();
            lock(&self.settled).extend(records);
        }
    }

    /// The stop's seal: seals, sets `stop_requested`, and takes the session out, in one
    /// step with no await (design §2.3 Stop step 3).
    pub(crate) fn seal_for_stop(&self, guard: &mut OpGuard) -> Option<TrackedSession> {
        self.seal(guard);
        self.stop_requested.store(true, Ordering::SeqCst);
        guard.take()
    }

    /// Takes the op lock by `deadline` (supervisor clock) and runs `f` inline; `f` must not
    /// block.
    #[allow(dead_code, reason = "10b's sends use it")]
    pub(crate) async fn with_session<R>(
        &self,
        sup: &Supervisor,
        deadline: Instant,
        f: impl FnOnce(&Session) -> R,
    ) -> Result<R, SessionError> {
        let guard = settle_before(
            || sup.now(),
            deadline,
            sup.timer(deadline),
            self.ops.clone().lock_owned(),
        )
        .await
        .in_time()
        .ok_or(SessionError::Busy)?;
        match guard.as_ref() {
            Some(s) => Ok(f(s)),
            None => Err(SessionError::Gone),
        }
    }
}

impl Supervisor {
    /// Never waits: for the hook intake path (design §2.11 "Observation").
    #[allow(dead_code, reason = "10b's IntakeProbe reads it")]
    pub(crate) fn observe(&self, launch: LaunchId) -> LaunchObservation {
        let Some(map) = self.live.try_lock() else {
            return LaunchObservation::Unobservable;
        };
        let Some(entry) = map.get(&launch).cloned() else {
            return LaunchObservation::Unobservable;
        };
        drop(map);
        if let Some(p) = entry.published() {
            return match p.observer.try_last_started_each() {
                Some(last_started) => LaunchObservation::Started {
                    reference: p.reference,
                    last_started,
                },
                None => LaunchObservation::Unobservable,
            };
        }
        if entry.creation_started() {
            LaunchObservation::Unobservable
        } else {
            LaunchObservation::Reserved
        }
    }
}
