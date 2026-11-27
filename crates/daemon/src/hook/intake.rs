//! Intake: receipt ordinals, intake facts, and the binding order
//! (`docs/design/hook-ingest.md` §2.4).

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chaldea_core::ids::LaunchId;

use super::adapter::TurnMark;
use crate::http::{ApiError, MonotonicClock, Shared, until_deadline};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, OwnedMutexGuard};

/// The facts PR 10 needs at a hook's intake (the PTY writer's position, dispatched
/// writes), opaque to PR 9.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntakeFacts(pub serde_json::Value);

/// Supplies a hook's intake facts and learns when the intake is finished.
pub trait IntakeProbe: Send + Sync + 'static {
    /// The facts at the intake of the hook with receipt ordinal `ordinal`, or `None` (facts
    /// missing). It runs on a blocking thread with the launch's intake lock held, after the
    /// ordinal was taken. Its waits end by the facts deadline `f` (real time), each followed
    /// by one more attempt, except a wait on a short section with no await, I/O, or other
    /// lock inside it (the send side's launch map: one launch constructed and inserted or one
    /// removed). An implementation whose `finished` must run arms `arm` when the intake is
    /// taken in; an unarmed guard finishes nothing.
    fn facts(
        &self,
        launch: LaunchId,
        ordinal: u64,
        f: Instant,
        arm: &IntakeGuard,
    ) -> Option<IntakeFacts>;

    /// The intake `ordinal` of `launch` is finished: its job committed with `marks` (its own
    /// delivery's and, in a binding job, each promoted delivery's, by receipt ordinal) at
    /// `committed_at` (the clock read right after a successful SQLite `COMMIT`), or decided
    /// nothing (`marks` empty and no instant: rolled back, refused, or never submitted).
    /// Called at most once per armed intake through its [`IntakeGuard`], and never for an
    /// intake whose job's outcome stays unknown. It waits only on short sections with no
    /// await, I/O, or other lock inside them, with no deadline of its own.
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
    fn facts(&self, _: LaunchId, _: u64, _: Instant, _: &IntakeGuard) -> Option<IntakeFacts> {
        None
    }
}

/// Finishes one intake exactly once (supervisor-send §2.9 P2): owned by the hook request
/// from its intake, then by the write observer from its registration. Created unarmed; the
/// probe arms it when the intake is taken in (the fence advance), and only an armed guard
/// finishes anything. Dropped armed before its job was submitted, it finishes with no marks;
/// after a reply that decides the job (committed, rolled back, refused) it finishes with the
/// committed marks or none; after `CommitUnknown` or `WriterGone`, or dropped after the
/// submission without a reply (the observer aborted at shutdown), it leaves the intake
/// unfinished.
pub struct IntakeGuard {
    probe: Arc<dyn IntakeProbe>,
    launch: LaunchId,
    ordinal: u64,
    /// The intake was taken in: from now on the guard finishes it.
    armed: AtomicBool,
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
            .field("armed", &self.armed())
            .field("submitted", &self.submitted)
            .finish_non_exhaustive()
    }
}

impl IntakeGuard {
    /// An unarmed guard.
    #[must_use]
    pub fn new(probe: Arc<dyn IntakeProbe>, launch: LaunchId, ordinal: u64) -> Self {
        IntakeGuard {
            probe,
            launch,
            ordinal,
            armed: AtomicBool::new(false),
            submitted: false,
            done: false,
        }
    }

    /// The intake was taken in: the guard finishes it from now on.
    pub fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }

    /// The job was submitted: from now on only its reply finishes the intake.
    pub(crate) fn submitted(&mut self) {
        self.submitted = true;
    }

    /// The writer's answer: committed with `marks` at the job's instant, or `None` for a
    /// reply that decided nothing; `unknown` for an outcome that stays unknown (the intake
    /// stays unfinished). Unarmed, nothing.
    pub(crate) fn replied(
        mut self,
        committed: Option<(&[(u64, TurnMark)], Instant)>,
        unknown: bool,
    ) {
        self.done = true;
        if !unknown && self.armed() {
            let (marks, at) = committed.map_or((&[][..], None), |(m, at)| (m, Some(at)));
            self.probe.finished(self.launch, self.ordinal, marks, at);
        }
    }
}

impl Drop for IntakeGuard {
    fn drop(&mut self) {
        if !self.done && !self.submitted && self.armed() {
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

/// One launch's intake: the async lock an intake holds from before its ordinal to its
/// intake instant, and the short-section state.
struct LaunchEntry {
    intake: Arc<tokio::sync::Mutex<()>>,
    /// The only lock ticket drops and `earlier_released` take with the outer map's.
    state: Mutex<LaunchIntake>,
}

/// Per launch: the next receipt ordinal (seeded from the stored maximum), the ordinals taken
/// but not yet submitted to the writer or released, and the intake lock. Memory holds
/// nothing else. The outer map and each `state` are held for short sections only.
#[derive(Default)]
pub struct IntakeOrder {
    launches: Mutex<HashMap<LaunchId, Arc<LaunchEntry>>>,
    /// Run once between a waiter's check of `pending` and its await (tests only).
    #[cfg(test)]
    pub(crate) between_check_and_await: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Run once by `intake` before it takes the intake lock (tests only).
    #[cfg(test)]
    pub(crate) before_lock: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Run once after the intake lock was taken, before the dispatch check (tests only).
    #[cfg(test)]
    pub(crate) after_lock: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Run once at the start of the intake's blocking task, before its checks (tests only).
    #[cfg(test)]
    pub(crate) task_started: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Run once at the start point, under `state`, before the claim (tests only).
    #[cfg(test)]
    pub(crate) before_claim: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Run once right after the ordinal was taken, before the facts (tests only).
    #[cfg(test)]
    pub(crate) after_ordinal: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Blocking tasks the intake started (tests only).
    #[cfg(test)]
    pub(crate) tasks: std::sync::atomic::AtomicUsize,
}

/// Runs and clears a test hook.
#[cfg(test)]
fn run_once(slot: &Mutex<Option<Box<dyn FnOnce() + Send>>>) {
    let f = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(f) = f {
        f();
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
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
        let e = lock(&self.order.launches).get(&self.launch).cloned();
        if let Some(e) = e {
            let st = &mut *lock(&e.state);
            st.pending.remove(&self.ordinal);
            st.released.notify_waiters();
        }
    }
}

/// What an intake took: its ticket, its facts, its guard, and its instant (the pid wait's
/// origin).
pub(crate) struct Taken {
    pub ticket: IntakeTicket,
    pub facts: Option<IntakeFacts>,
    pub guard: IntakeGuard,
    pub at: Instant,
}

/// The request's start cell: the request's drop and the intake's start point each swap it
/// from `PENDING`, and the first one decides (hook-ingest §2.4 "Start point").
const PENDING: u8 = 0;
const STARTED: u8 = 1;
const CANCELLED: u8 = 2;

/// Cancels a pending start when the request future is dropped.
struct CancelOnDrop(Arc<AtomicU8>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let _ = self
            .0
            .compare_exchange(PENDING, CANCELLED, Ordering::SeqCst, Ordering::SeqCst);
    }
}

/// How the intake's blocking task ended.
enum Start {
    Started(Taken),
    /// The request deadline had passed at the claim.
    Expired,
    /// The request was dropped before the start point.
    Cancelled,
}

impl IntakeOrder {
    /// `l`'s entry, created with its next ordinal at `stored_max + 1` when absent.
    fn entry(&self, l: LaunchId, stored_max: u64) -> Arc<LaunchEntry> {
        lock(&self.launches)
            .entry(l)
            .or_insert_with(|| {
                Arc::new(LaunchEntry {
                    intake: Arc::new(tokio::sync::Mutex::new(())),
                    state: Mutex::new(LaunchIntake {
                        next: stored_max.saturating_add(1),
                        pending: BTreeSet::new(),
                        released: Arc::new(Notify::new()),
                    }),
                })
            })
            .clone()
    }

    /// Design §2.4: the next ordinal of `l`, `probe`'s facts with the intake's
    /// [`IntakeGuard`], and the intake instant from `now` (the pid wait's origin), all under
    /// `l`'s intake lock, so receipt order, the order of intake facts, and the pid wait's
    /// start agree. The request deadline (server clock) is checked on entry, after the lock,
    /// and at the start point, where the ordinal is taken; each is 408 with no ordinal. The
    /// facts deadline `F = min(now + wait, real(deadline))` (real time) bounds the probe's
    /// waits and never answers 408. From the start point the intake runs to its end; a 408
    /// or a drop after it leaves its ordinal a gap and finishes it with no marks.
    ///
    /// # Errors
    ///
    /// 408 as above; 500 when the blocking task panicked (its ticket and guard have been
    /// dropped by then).
    pub(crate) async fn intake(
        self: &Arc<Self>,
        shared: &Shared,
        deadline: Instant,
        l: LaunchId,
        stored_max: u64,
        probe: &Arc<dyn IntakeProbe>,
        wait: Duration,
    ) -> Result<Taken, ApiError> {
        // 1. Entry.
        if shared.passed(deadline) {
            return Err(ApiError::request_timeout());
        }
        let f = (Instant::now() + wait).min(shared.real(deadline));
        let cell = Arc::new(AtomicU8::new(PENDING));
        let _cancel = CancelOnDrop(cell.clone());
        #[cfg(test)]
        run_once(&self.before_lock);
        // 2. The launch's intake lock.
        let e = self.entry(l, stored_max);
        let g = until_deadline(shared, deadline, || e.intake.clone().lock_owned()).await?;
        #[cfg(test)]
        run_once(&self.after_lock);
        // 3. Dispatch.
        if shared.passed(deadline) {
            return Err(ApiError::request_timeout());
        }
        #[cfg(test)]
        self.tasks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (me, probe, clock) = (self.clone(), probe.clone(), shared.cfg.clock.clone());
        let task = tokio::task::spawn_blocking(move || {
            me.start(&e, g, &cell, &*clock, deadline, l, &probe, f)
        });
        match until_deadline(shared, deadline, || task).await? {
            Ok(Start::Started(t)) => Ok(t),
            Ok(Start::Expired | Start::Cancelled) => Err(ApiError::request_timeout()),
            Err(_) => Err(ApiError::internal()),
        }
    }

    /// Steps 4–8 on the blocking pool, with the intake lock `g` held until the intake
    /// instant.
    #[allow(
        clippy::too_many_arguments,
        reason = "the intake's inputs, passed once"
    )]
    fn start(
        self: &Arc<Self>,
        e: &LaunchEntry,
        g: OwnedMutexGuard<()>,
        cell: &AtomicU8,
        clock: &dyn MonotonicClock,
        deadline: Instant,
        l: LaunchId,
        probe: &Arc<dyn IntakeProbe>,
        f: Instant,
    ) -> Start {
        #[cfg(test)]
        run_once(&self.task_started);
        // 4. The start point and the ordinal, under `state`.
        let ticket = {
            let mut st = lock(&e.state);
            #[cfg(test)]
            run_once(&self.before_claim);
            // The claim, then the request clock: a claim that finds the deadline passed
            // takes nothing, as a drop would, so only an in-time claim takes an ordinal.
            if cell
                .compare_exchange(PENDING, STARTED, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return Start::Cancelled;
            }
            if clock.now() >= deadline {
                return Start::Expired;
            }
            let ordinal = st.next;
            st.next = st.next.saturating_add(1);
            st.pending.insert(ordinal);
            IntakeTicket {
                launch: l,
                ordinal,
                order: self.clone(),
            }
        };
        #[cfg(test)]
        run_once(&self.after_ordinal);
        // 5–7. The probe's waits, `SendState`, and the observation.
        let guard = IntakeGuard::new(probe.clone(), l, ticket.ordinal);
        let facts = probe.facts(l, ticket.ordinal, f, &guard);
        // 8. The intake instant.
        let at = clock.now();
        drop(g);
        Start::Started(Taken {
            ticket,
            facts,
            guard,
            at,
        })
    }

    /// Takes the next ordinal of `l` with no probe (tests of the binding order only).
    #[cfg(test)]
    pub(crate) fn ticket(self: &Arc<Self>, l: LaunchId, stored_max: u64) -> IntakeTicket {
        let e = self.entry(l, stored_max);
        let mut st = lock(&e.state);
        let ordinal = st.next;
        st.next = st.next.saturating_add(1);
        st.pending.insert(ordinal);
        IntakeTicket {
            launch: l,
            ordinal,
            order: self.clone(),
        }
    }

    /// Whether some intake of `l` holds its intake lock now (tests only).
    #[cfg(test)]
    pub(crate) fn locked(&self, l: LaunchId) -> bool {
        lock(&self.launches)
            .get(&l)
            .is_some_and(|e| e.intake.try_lock().is_err())
    }

    /// The next ordinal `l` would take (tests only).
    #[cfg(test)]
    pub(crate) fn next(&self, l: LaunchId) -> Option<u64> {
        let e = lock(&self.launches).get(&l).cloned()?;
        let next = lock(&e.state).next;
        Some(next)
    }

    /// Resolves once no ordinal lower than `t`'s is pending for its launch. The
    /// `Notified` future is created and enabled before `pending` is checked, so a release
    /// between the check and the await wakes it.
    pub async fn earlier_released(&self, t: &IntakeTicket) {
        loop {
            let Some(e) = lock(&self.launches).get(&t.launch).cloned() else {
                return;
            };
            let released = lock(&e.state).released.clone();
            let notified = released.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let lower_pending = lock(&e.state).pending.range(..t.ordinal).next().is_some();
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
        let Some(e) = lock(&self.launches).get(&l).cloned() else {
            return false;
        };
        lock(&e.state).pending.contains(&ordinal)
    }
}
