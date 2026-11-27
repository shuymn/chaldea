//! The send and ack side (`docs/design/supervisor-send.md`, cited as "send §N"): the
//! readiness gate and its evidence, the writer integration, ack attribution, keys, human
//! writes, cards, and timers.
//!
//! The rules (send §2.1):
//!
//! 1. **The store decides; memory holds what the store cannot give in time** ([`SendLaunch`]).
//! 2. **One synchronous boundary orders hooks against writes**: the fence word.
//! 3. **Positions, not wall time, order writes against hooks.**
//! 4. **Zero bytes or unknown, never guessed.**
//! 5. **Ambiguity fails closed.**
//! 6. **One owner per record**: the launch's worker applies every write completion.

mod attribution;
pub mod claude;
mod commands;
mod completion;
mod driver;
mod exit;
pub mod fake_decoder;
mod hooks;
mod probe;
mod tick;

#[cfg(test)]
pub(crate) mod tests;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, TryLockError, Weak};
use std::time::{Duration, Instant};

use chaldea_core::adapter::{Capabilities, ReadinessMode};
use chaldea_core::ids::{Agent, ApprovalId, InputId, LaunchId, ThreadId};
use chaldea_pty_runtime::{WriteRecord, WriteTicket};
use tokio::sync::{Notify, oneshot};

use super::live::LiveLaunch;
use super::{Supervisor, SupervisorConfig, lock};
use crate::hook::TurnMark;

pub use claude::ClaudeDecoder;
pub(crate) use commands::register;
pub(crate) use hooks::SupervisorHooks;
pub use hooks::{Decoders, HookDecoder, HookFact, NoDecoder};
pub(crate) use probe::SupervisorProbe;

/// A send unit's dispatch deadline: the writer refuses it with zero bytes after this.
pub(crate) const DISPATCH_WAIT: Duration = Duration::from_secs(10);
/// A permitted unit with no record this long after its first permission shows its input
/// `unconfirmed`.
pub(crate) const WRITE_STALL: Duration = Duration::from_secs(30);
/// A `sent` input no hook acked this long after its completion becomes `unconfirmed`.
pub(crate) const ACK_WINDOW: Duration = Duration::from_secs(30);
/// A `queued` input this long after it was first observed raises `input_gate_timeout`.
pub(crate) const GATE_TIMEOUT: Duration = Duration::from_secs(60);
/// A `Stop` settles this long after its commit.
pub(crate) const SETTLE: Duration = Duration::from_secs(2);
/// A key unit's dispatch deadline.
pub(crate) const KEY_WAIT: Duration = Duration::from_secs(2);
/// The worker's reads and the samples' bounds.
pub(crate) const READ_WAIT: Duration = Duration::from_secs(1);
pub(crate) const SAMPLE_WAIT: Duration = Duration::from_millis(250);

// ---- the fence word -------------------------------------------------------------------------

/// The low bits count the intakes not yet finished; the high bits count every intake.
pub(crate) const OPEN_BITS: u32 = 20;
pub(crate) const OPEN_MASK: u64 = (1 << OPEN_BITS) - 1;
const COUNT_MAX: u64 = (1 << (64 - OPEN_BITS)) - 1;
/// The closed word: every send and key of the launch is refused until it exits.
pub(crate) const FENCE_CLOSED: u64 = u64::MAX;

/// One intake taken in: both counts up by one with one compare-and-swap; an add that would
/// overflow either field closes the word instead. The closed word never changes.
pub(crate) fn fence_advance(word: &AtomicU64) {
    let mut cur = word.load(Ordering::Acquire);
    loop {
        if cur == FENCE_CLOSED {
            return;
        }
        let next = if cur & OPEN_MASK == OPEN_MASK || cur >> OPEN_BITS == COUNT_MAX {
            FENCE_CLOSED
        } else {
            cur + (1 << OPEN_BITS) + 1
        };
        match word.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(seen) => cur = seen,
        }
    }
}

/// One intake finished: the unfinished count down by one, with `Release` ordering. The
/// closed word never changes.
pub(crate) fn fence_release(word: &AtomicU64) {
    let mut cur = word.load(Ordering::Acquire);
    loop {
        if cur == FENCE_CLOSED || cur & OPEN_MASK == 0 {
            return;
        }
        match word.compare_exchange_weak(cur, cur - 1, Ordering::Release, Ordering::Acquire) {
            Ok(_) => return,
            Err(seen) => cur = seen,
        }
    }
}

/// Whether a loaded word lets a unit open: no unfinished intake and not closed.
pub(crate) fn fence_quiet(word: u64) -> bool {
    word != FENCE_CLOSED && word & OPEN_MASK == 0
}

// ---- per-launch memory ------------------------------------------------------------------------

/// The ack facts of the input last dispatched (send §2.3 step 2), kept until it leaves
/// flight or the next dispatch.
#[derive(Debug, Clone)]
pub(crate) struct Dispatch {
    pub input_id: InputId,
    /// `last_started[input]` before the unit's submission.
    pub before: u64,
    /// Set by the unit's completion when its outcome is `Written`.
    pub written: Arc<AtomicBool>,
    /// A gate-closing human write followed the launch's last turn start at the open.
    pub competing: bool,
}

/// The launch's running input unit: set before `submit`, cleared by its record.
pub(crate) struct RunningUnit {
    pub input_id: InputId,
    /// Set on the store writer right after the `COMMIT` of the unit's completion job, before
    /// its reply and the writer's next job: from then on the intake probe reports the unit
    /// ended, though the worker clears `running` only once it applies the reply (send §2.3
    /// step 4).
    pub recorded: Arc<AtomicBool>,
    pub ticket: Option<WriteTicket>,
    pub submitted_at: Instant,
    /// The write stall was shown (`unconfirmed`).
    pub stalled: bool,
}

/// What holds the launch lease (`docs/design/t1-attach.md` D7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeaseKind {
    /// An input's open and submission, until it becomes the running unit.
    Send(InputId),
    /// An attach socket's resize, until its backend call returns.
    Resize(u64),
}

/// The launch lease: one holder at a time orders a send's open and submission against a
/// resize. Only the holder, by `id`, converts or releases it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lease {
    pub id: u64,
    pub kind: LeaseKind,
}

/// What the worker reports to a key's effect once it decided the unit's record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyReport {
    /// The `human_write` committed.
    Recorded { complete: bool, bytes: u64 },
    /// The unit wrote zero bytes, refused by the writer for this reason.
    NotStarted(chaldea_pty_runtime::Refusal),
}

/// An attach record's `human_write` decision, set once by the first committed job that made
/// it: the interval key it appended, or `None` when its interval already had its event.
pub(crate) type Decided = Arc<OnceLock<Option<chaldea_core::ids::IdempotencyKey>>>;

/// One completion the writer fired, with the key effect waiting for it, if any.
pub(crate) struct Fired {
    pub record: WriteRecord,
    pub report: Option<oneshot::Sender<KeyReport>>,
    pub decided: Decided,
}

/// The turn log (send §2.4 "Settle completion"): each interpreted hook's mark by receipt
/// ordinal, with its commit instant.
#[derive(Debug, Default, Clone)]
pub(crate) struct TurnLog(pub BTreeMap<u64, (TurnMark, Instant)>);

impl TurnLog {
    /// Inserts a mark, then drops the entries below the boundary under the current turn's
    /// start.
    pub(crate) fn insert(&mut self, ordinal: u64, mark: TurnMark, at: Instant) {
        self.0.entry(ordinal).or_insert((mark, at));
        let start = self
            .0
            .iter()
            .rev()
            .find(|(_, (m, _))| *m == TurnMark::Start)
            .map(|(o, _)| *o);
        if let Some(t) = start
            && let Some(b) = self
                .0
                .range(..t)
                .rev()
                .find(|(_, (m, _))| m.is_boundary())
                .map(|(o, _)| *o)
        {
            self.0 = self.0.split_off(&b);
        }
    }

    /// The settle candidate: the highest entry, when its ordinal is `latest`, it is
    /// `End { settle: true }`, the boundary below it is a `Start`, and the boundary below
    /// that start is an `End` or none. Returns its commit instant (its ordinal is `latest`).
    pub(crate) fn candidate(&self, latest: u64) -> Option<Instant> {
        let (&s, &(mark, at)) = self.0.iter().next_back()?;
        if s != latest || mark != (TurnMark::End { settle: true }) {
            return None;
        }
        let mut below = self
            .0
            .range(..s)
            .rev()
            .filter(|(_, (m, _))| m.is_boundary());
        let (_, (start, _)) = below.next()?;
        if *start != TurnMark::Start {
            return None;
        }
        match below.next() {
            None | Some((_, (TurnMark::End { .. }, _))) => Some(at),
            Some(_) => None,
        }
    }
}

/// Everything the worker, the probe, and the commands share about one launch under one
/// mutex (send §2.1 rule 1). The probe only `try_lock`s it.
#[derive(Default)]
pub(crate) struct SendState {
    pub dispatch: Option<Dispatch>,
    pub running: Option<RunningUnit>,
    /// The launch lease; a running unit holds it as `running` (send §2.3 step 1).
    pub lease: Option<Lease>,
    pub human_unsettled: u32,
    /// Each card's human writer position at its observation (`None` when unreliable).
    /// Dropped at the card's resolution, when a key finds the card closed in the store, or
    /// with the launch's side; a card a turn boundary closed keeps its entry until then.
    pub cards: HashMap<ApprovalId, Option<u64>>,
    pub log: TurnLog,
    /// The worker's first read of each input as `queued`.
    pub queued_at: HashMap<InputId, Instant>,
    /// Each `sent` input's completion commit.
    pub sent_at: HashMap<InputId, Instant>,
    /// Inputs whose gate timeout was raised.
    pub timed_out: std::collections::HashSet<InputId>,
    /// Human records taken from the writer and not yet recorded, in `seq` order.
    pub held: Vec<Fired>,
    /// The records the exit job copied at its first attempt (send §2.3 "After exit").
    pub exit_set: Option<Vec<(WriteRecord, Decided)>>,
}

/// Test points inside a launch's send side (send §6 "Host speed").
#[cfg(test)]
#[derive(Default)]
pub(crate) struct SendHooks {
    /// Runs inside a unit's completion (on the writer thread, after its last write call)
    /// before `written` is set and the record is handed to the worker.
    pub completion: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs between the open job's commit and the unit's submission (async-free).
    pub before_submit: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs right after `submit` returned (inside `with_session`).
    pub after_submit: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs before each completion or human-write job.
    pub record_job: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs on the worker after a human record's job committed, before the record settles.
    pub after_human_record: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs inside the settle job (on the store writer).
    pub settle_job: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs inside the open job (on the store writer).
    pub open_job: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs inside the open job after its first check of the running and human units, before
    /// its read (on the store writer).
    pub open_after_flags: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs in the probe's `facts` after `written` was read, before the fence advance (the
    /// launch's state held).
    pub after_written_read: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs at the start of the probe's `finished` (on the write's observer), before the
    /// hook's turn marks are applied.
    pub finished: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs on the worker right after the pass's read, before its timers.
    pub after_read: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs on the worker after an input's completion job was recorded, before its result
    /// is applied.
    pub after_record: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Takes the place of the probe's observation of the launch at one intake (on the
    /// hook's request task, synchronously).
    #[allow(clippy::type_complexity, reason = "a one-shot test point")]
    pub observe: Mutex<
        Option<
            Box<dyn FnOnce(&super::Supervisor, LaunchId) -> super::live::LaunchObservation + Send>,
        >,
    >,
}

#[cfg(test)]
pub(crate) fn run_hook(slot: &Mutex<Option<Box<dyn FnOnce() + Send>>>) {
    let f = lock(slot).take();
    if let Some(f) = f {
        f();
    }
}

/// One launch's send side; lives until its exit committed and its writer fired every
/// completion.
pub(crate) struct SendLaunch {
    pub thread_id: ThreadId,
    pub launch_id: LaunchId,
    pub agent: Agent,
    pub caps: &'static Capabilities,
    pub positional: Option<InputId>,
    pub fence: Arc<AtomicU64>,
    pub latest_intake: AtomicU64,
    /// The highest receipt ordinal among the launch's committed turn boundaries (§2.4),
    /// raised only by the store writer after a delivery job's `COMMIT`.
    pub turn_watermark: Arc<AtomicU64>,
    /// Mirrors `state.running.is_some()` for jobs on the store writer.
    pub running_flag: AtomicBool,
    /// Mirrors `state.human_unsettled` for jobs on the store writer.
    pub human_flag: AtomicU32,
    pub state: Mutex<SendState>,
    /// Completions fired and not yet taken by the worker.
    pub fired: Mutex<Vec<Fired>>,
    /// Units submitted whose completion has not fired.
    pub outstanding: AtomicU64,
    pub wake: Notify,
    /// Lease ids.
    next_lease: AtomicU64,
    /// Notified at every release of the lease, for the resizes waiting for it.
    pub lease_free: Notify,
    /// Waits for the lease begun (tests poll it).
    #[cfg(test)]
    pub lease_waits: AtomicU64,
    /// Passes finished (tests poll it).
    #[cfg(test)]
    pub passes: AtomicU64,
    /// The latest tick's instant (supervisor clock).
    pub tick: Mutex<Option<Instant>>,
    #[cfg(test)]
    pub hooks: SendHooks,
}

impl SendLaunch {
    fn new(e: &LiveLaunch) -> SendLaunch {
        SendLaunch {
            thread_id: e.thread_id,
            launch_id: e.launch_id,
            agent: e.agent,
            caps: e.caps,
            positional: e.positional,
            fence: Arc::new(AtomicU64::new(0)),
            latest_intake: AtomicU64::new(0),
            turn_watermark: Arc::new(AtomicU64::new(0)),
            running_flag: AtomicBool::new(false),
            human_flag: AtomicU32::new(0),
            state: Mutex::new(SendState::default()),
            fired: Mutex::new(Vec::new()),
            outstanding: AtomicU64::new(0),
            wake: Notify::new(),
            next_lease: AtomicU64::new(1),
            lease_free: Notify::new(),
            #[cfg(test)]
            lease_waits: AtomicU64::new(0),
            #[cfg(test)]
            passes: AtomicU64::new(0),
            tick: Mutex::new(None),
            #[cfg(test)]
            hooks: SendHooks::default(),
        }
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, SendState> {
        lock(&self.state)
    }

    /// The state, or `None` while another holder has it (never waits).
    pub(crate) fn try_state(&self) -> Option<MutexGuard<'_, SendState>> {
        match self.state.try_lock() {
            Ok(g) => Some(g),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// A completion that hands its record to the worker; `written` is set first for a
    /// `Written` outcome, under the launch's state, so the probe's one observation point
    /// (send §2.5) orders it against the intake. That is its only wait: the state is never
    /// held across a write call, and a cancel fired under it is never `Written`.
    pub(crate) fn completion(
        self: &Arc<Self>,
        written: Option<Arc<AtomicBool>>,
        report: Option<oneshot::Sender<KeyReport>>,
    ) -> chaldea_pty_runtime::Completion {
        let me = Arc::downgrade(self);
        Box::new(move |r: &WriteRecord| {
            let Some(me) = Weak::upgrade(&me) else {
                return;
            };
            #[cfg(test)]
            run_hook(&me.hooks.completion);
            if r.outcome == chaldea_pty_runtime::WriteOutcome::Written
                && let Some(w) = &written
            {
                let _st = me.state();
                w.store(true, Ordering::Release);
            }
            lock(&me.fired).push(Fired {
                record: r.clone(),
                report,
                decided: Decided::default(),
            });
            me.outstanding.fetch_sub(1, Ordering::AcqRel);
            me.wake.notify_one();
        })
    }

    /// Submits a human unit (`Key`, `CardKey`, `Attach`) inside `with_session` (send §2.6
    /// "Human units"): `human_unsettled` goes up just before `submit` and back down at once
    /// when `submit` fails; the worker records the unit's completion.
    ///
    /// # Errors
    ///
    /// The session refused the unit (nothing was queued).
    pub(crate) fn submit_human(
        self: &Arc<Self>,
        s: &chaldea_pty_runtime::Session,
        unit: chaldea_pty_runtime::WriteUnit,
        guard: chaldea_pty_runtime::Guard,
        report: Option<oneshot::Sender<KeyReport>>,
    ) -> Result<WriteTicket, chaldea_pty_runtime::SubmitError> {
        self.human_up(&mut self.state());
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        let done = self.completion(None, report);
        s.submit(unit, guard, done).inspect_err(|_| {
            self.outstanding.fetch_sub(1, Ordering::AcqRel);
            self.human_down(&mut self.state());
        })
    }

    /// One more human unit submitted and not yet recorded, and its flag.
    pub(crate) fn human_up(&self, st: &mut SendState) {
        st.human_unsettled += 1;
        self.human_flag.store(st.human_unsettled, Ordering::SeqCst);
    }

    /// One human unit recorded (or never queued), and its flag.
    pub(crate) fn human_down(&self, st: &mut SendState) {
        st.human_unsettled = st.human_unsettled.saturating_sub(1);
        self.human_flag.store(st.human_unsettled, Ordering::SeqCst);
    }

    /// Sets the running unit and its flag. Clearing it releases the lease the unit held.
    pub(crate) fn set_running(&self, st: &mut SendState, r: Option<RunningUnit>) {
        let cleared = r.is_none() && st.running.is_some();
        self.running_flag.store(r.is_some(), Ordering::SeqCst);
        st.running = r;
        if cleared {
            self.lease_released();
        }
    }

    /// Takes the lease for `kind` when it is free and no unit runs; its id.
    pub(crate) fn take_lease(&self, st: &mut SendState, kind: LeaseKind) -> Option<u64> {
        if st.lease.is_some() || st.running.is_some() {
            return None;
        }
        let id = self.next_lease.fetch_add(1, Ordering::Relaxed);
        st.lease = Some(Lease { id, kind });
        Some(id)
    }

    /// Releases the lease when `id` still holds it; true when it did.
    pub(crate) fn release_lease(&self, st: &mut SendState, id: u64) -> bool {
        if st.lease.is_none_or(|l| l.id != id) {
            return false;
        }
        st.lease = None;
        self.lease_released();
        true
    }

    /// Clears whatever holds the lease (the launch's exit, the shutdown).
    pub(crate) fn clear_lease(&self, st: &mut SendState) {
        if st.lease.take().is_some() {
            self.lease_released();
        }
    }

    /// Every release wakes the worker and the waiting resizes.
    fn lease_released(&self) {
        self.wake.notify_one();
        self.lease_free.notify_waiters();
    }
}

/// The send side's shared parts: the launches, the decoders, and the driver's channel.
pub(crate) struct SendSide {
    /// Readers (the intake probe, commands, the driver, and hook jobs on the store writer)
    /// never wait on each other; a writer holds it only for one insert or remove. Nothing
    /// holding it waits on the store writer.
    pub map: RwLock<HashMap<LaunchId, Arc<SendLaunch>>>,
    /// Each agent's decoder (send §2.5).
    pub decoders: Decoders,
    /// The readiness mode of every row, when a test sets one.
    pub mode: Option<ReadinessMode>,
    pub ticks: tokio::sync::watch::Sender<Option<Instant>>,
}

impl SendSide {
    pub(crate) fn launch(&self, l: LaunchId) -> Option<Arc<SendLaunch>> {
        self.map
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&l)
            .cloned()
    }

    pub(crate) fn all(&self) -> Vec<Arc<SendLaunch>> {
        self.map
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn mode(&self, caps: &Capabilities) -> ReadinessMode {
        self.mode.unwrap_or_else(|| caps.readiness_mode())
    }
}

/// Creates the send side and registers its exit and tick steps in `cfg` (send §2.9 D4).
pub(crate) fn install(cfg: &mut SupervisorConfig) -> Arc<SendSide> {
    let side = Arc::new(SendSide {
        map: RwLock::new(HashMap::new()),
        decoders: cfg.decoders.clone(),
        mode: cfg.readiness_mode,
        ticks: tokio::sync::watch::channel(None).0,
    });
    cfg.exit_steps
        .push(Arc::new(exit::SendExitStep(side.clone())));
    cfg.tick_steps.push(Arc::new(tick::SendTick(side.clone())));
    side
}

impl Supervisor {
    /// Creates `entry`'s send side and its worker once (the reservation job).
    pub(crate) fn send_launch(self: &Arc<Self>, entry: &LiveLaunch) {
        let mut map = self
            .send
            .map
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if map.contains_key(&entry.launch_id) {
            return;
        }
        let sl = Arc::new(SendLaunch::new(entry));
        map.insert(entry.launch_id, sl.clone());
        drop(map);
        if self.cfg.send_workers {
            self.rt.spawn(driver::worker(self.clone(), sl));
        }
    }

    /// Starts the driver that wakes the workers on commits and ticks.
    pub(crate) fn start_send_driver(self: &Arc<Self>) {
        if self.cfg.send_workers {
            self.rt.spawn(driver::driver(self.clone()));
        }
    }
}
