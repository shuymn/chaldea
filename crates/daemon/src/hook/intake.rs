//! Intake: receipt ordinals, intake facts, and the binding order
//! (`docs/design/hook-ingest.md` §2.4).

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use chaldea_core::ids::LaunchId;

use super::adapter::TurnMark;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

/// The facts PR 10 needs at a hook's intake (the PTY writer's position, dispatched
/// writes), opaque to PR 9.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntakeFacts(pub serde_json::Value);

/// Supplies a hook's intake facts and learns when the intake is finished.
pub trait IntakeProbe: Send + Sync + 'static {
    /// The facts at the intake of the hook with receipt ordinal `ordinal`, or `None` (no
    /// session yet). Non-blocking: it runs under the intake mutex.
    fn facts(&self, launch: LaunchId, ordinal: u64) -> Option<IntakeFacts>;

    /// The intake `ordinal` of `launch` is finished: its job committed with `marks` (its own
    /// delivery's and, in a binding job, each promoted delivery's, by receipt ordinal) at
    /// `committed_at` (the clock read at the end of the committing job), or decided nothing
    /// (`marks` empty and no instant: rolled back, refused, or never submitted). Called at
    /// most once per intake through its [`IntakeGuard`], and never for an intake whose job's
    /// outcome stays unknown. Non-blocking.
    fn finished(
        &self,
        launch: LaunchId,
        ordinal: u64,
        marks: &[(u64, TurnMark)],
        committed_at: Option<Instant>,
    ) {
        let _ = (launch, ordinal, marks, committed_at);
    }
}

/// The default probe: no facts.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoIntakeFacts;

impl IntakeProbe for NoIntakeFacts {
    fn facts(&self, _launch: LaunchId, _ordinal: u64) -> Option<IntakeFacts> {
        None
    }
}

/// Finishes one intake exactly once (supervisor-send §2.9 P2): owned by the hook request
/// from its intake, then by the write observer from its registration. Dropped before its
/// job was submitted, it finishes with no marks; after a reply that decides the job
/// (committed, rolled back, refused) it finishes with the committed marks or none; after
/// `CommitUnknown` or `WriterGone`, or dropped after the submission without a reply (the
/// observer aborted at shutdown), it leaves the intake unfinished.
pub struct IntakeGuard {
    probe: Arc<dyn IntakeProbe>,
    launch: LaunchId,
    ordinal: u64,
    /// `Store::submit` accepted the job.
    submitted: bool,
    /// Finished, or left unfinished on purpose.
    done: bool,
}

impl std::fmt::Debug for IntakeGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntakeGuard")
            .field("launch", &self.launch)
            .field("ordinal", &self.ordinal)
            .field("submitted", &self.submitted)
            .finish_non_exhaustive()
    }
}

impl IntakeGuard {
    pub(crate) fn new(probe: Arc<dyn IntakeProbe>, launch: LaunchId, ordinal: u64) -> Self {
        IntakeGuard {
            probe,
            launch,
            ordinal,
            submitted: false,
            done: false,
        }
    }

    /// The job was submitted: from now on only its reply finishes the intake.
    pub(crate) fn submitted(&mut self) {
        self.submitted = true;
    }

    /// The writer's answer: committed with `marks` at the job's instant, or `None` for a
    /// reply that decided nothing; `unknown` for an outcome that stays unknown (the intake
    /// stays unfinished).
    pub(crate) fn replied(
        mut self,
        committed: Option<(&[(u64, TurnMark)], Instant)>,
        unknown: bool,
    ) {
        self.done = true;
        if !unknown {
            let (marks, at) = committed.map_or((&[][..], None), |(m, at)| (m, Some(at)));
            self.probe.finished(self.launch, self.ordinal, marks, at);
        }
    }
}

impl Drop for IntakeGuard {
    fn drop(&mut self) {
        if !self.done && !self.submitted {
            self.done = true;
            self.probe.finished(self.launch, self.ordinal, &[], None);
        }
    }
}

struct LaunchIntake {
    next: u64,
    pending: BTreeSet<u64>,
    released: Arc<Notify>,
}

/// Per launch: the next receipt ordinal (seeded from the stored maximum) and the ordinals
/// taken but not yet submitted to the writer or released. Memory holds nothing else.
#[derive(Default)]
pub struct IntakeOrder {
    launches: Mutex<HashMap<LaunchId, LaunchIntake>>,
    /// Run once between a waiter's check of `pending` and its await (tests only).
    #[cfg(test)]
    pub(crate) between_check_and_await: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Run once by `intake` before it takes the lock (tests only).
    #[cfg(test)]
    pub(crate) before_lock: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// Runs and clears a test hook.
#[cfg(test)]
fn run_once(slot: &Mutex<Option<Box<dyn FnOnce() + Send>>>) {
    let f = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(f) = f {
        f();
    }
}

/// One taken ordinal; dropping it releases the ordinal for the binding order.
pub struct IntakeTicket {
    launch: LaunchId,
    ordinal: u64,
    order: Arc<IntakeOrder>,
}

impl std::fmt::Debug for IntakeTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntakeTicket")
            .field("launch", &self.launch)
            .field("ordinal", &self.ordinal)
            .finish_non_exhaustive()
    }
}

impl IntakeTicket {
    #[must_use]
    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }
}

impl Drop for IntakeTicket {
    fn drop(&mut self) {
        let mut map = self
            .order
            .launches
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(e) = map.get_mut(&self.launch) {
            e.pending.remove(&self.ordinal);
            e.released.notify_waiters();
        }
    }
}

impl IntakeOrder {
    /// The next ordinal of `l` (an absent entry starts at `stored_max + 1`), `probe`'s facts
    /// with the intake's [`IntakeGuard`], and the intake instant from `now` (the pid wait's
    /// origin), in one critical section, so receipt order, the order of intake facts, and the
    /// pid wait's start agree.
    pub fn intake(
        self: &Arc<Self>,
        l: LaunchId,
        stored_max: u64,
        probe: &Arc<dyn IntakeProbe>,
        now: impl FnOnce() -> Instant,
    ) -> (IntakeTicket, Option<IntakeFacts>, IntakeGuard, Instant) {
        #[cfg(test)]
        run_once(&self.before_lock);
        let mut map = self.launches.lock().unwrap_or_else(PoisonError::into_inner);
        let e = map.entry(l).or_insert_with(|| LaunchIntake {
            next: stored_max.saturating_add(1),
            pending: BTreeSet::new(),
            released: Arc::new(Notify::new()),
        });
        let ordinal = e.next;
        e.next = e.next.saturating_add(1);
        e.pending.insert(ordinal);
        let facts = probe.facts(l, ordinal);
        let guard = IntakeGuard::new(probe.clone(), l, ordinal);
        let at = now();
        drop(map);
        (
            IntakeTicket {
                launch: l,
                ordinal,
                order: self.clone(),
            },
            facts,
            guard,
            at,
        )
    }

    /// Whether some thread holds the intake lock now (tests only).
    #[cfg(test)]
    pub(crate) fn locked(&self) -> bool {
        matches!(
            self.launches.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        )
    }

    /// Resolves once no ordinal lower than `t`'s is pending for its launch. The
    /// `Notified` future is created and enabled before `pending` is checked, so a release
    /// between the check and the await wakes it.
    pub async fn earlier_released(&self, t: &IntakeTicket) {
        loop {
            let released = {
                let map = self.launches.lock().unwrap_or_else(PoisonError::into_inner);
                match map.get(&t.launch) {
                    Some(e) => e.released.clone(),
                    None => return,
                }
            };
            let notified = released.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let lower_pending = {
                let map = self.launches.lock().unwrap_or_else(PoisonError::into_inner);
                map.get(&t.launch)
                    .is_some_and(|e| e.pending.range(..t.ordinal).next().is_some())
            };
            if !lower_pending {
                return;
            }
            #[cfg(test)]
            run_once(&self.between_check_and_await);
            notified.await;
        }
    }

    /// Whether `ordinal` of `l` is pending (for tests).
    #[cfg(test)]
    pub(crate) fn is_pending(&self, l: LaunchId, ordinal: u64) -> bool {
        self.launches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&l)
            .is_some_and(|e| e.pending.contains(&ordinal))
    }
}
