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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError, Weak};
use std::time::{Duration, Instant};

use chaldea_core::adapter::{Capabilities, ReadinessMode};
use chaldea_core::ids::{Agent, ApprovalId, InputId, LaunchId, ThreadId};
use chaldea_pty_runtime::{WriteRecord, WriteTicket};
use tokio::sync::{Notify, oneshot};

use super::live::LiveLaunch;
use super::{Supervisor, SupervisorConfig, lock};
use crate::hook::TurnMark;

pub(crate) use commands::register;
pub(crate) use hooks::SupervisorHooks;
pub use hooks::{HookDecoder, HookFact, NoDecoder};
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
    pub ticket: Option<WriteTicket>,
    pub submitted_at: Instant,
    /// The write stall was shown (`unconfirmed`).
    pub stalled: bool,
}

/// What the worker reports to a key's effect once it decided the unit's record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyReport {
    /// The `human_write` committed.
    Recorded { complete: bool, bytes: u64 },
    /// The unit wrote zero bytes.
    NotStarted,
}

/// One completion the writer fired, with the key effect waiting for it, if any.
pub(crate) struct Fired {
    pub record: WriteRecord,
    pub report: Option<oneshot::Sender<KeyReport>>,
}

/// The turn log (send §2.4 "Settle completion"): each interpreted hook's mark by receipt
/// ordinal, with its commit instant.
#[derive(Debug, Default, Clone)]
pub(crate) struct TurnLog(pub BTreeMap<u64, (TurnMark, Instant)>);

impl TurnLog {
    fn boundary(m: &TurnMark) -> bool {
        matches!(m, TurnMark::Start | TurnMark::End { .. })
    }

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
                .find(|(_, (m, _))| Self::boundary(m))
                .map(|(o, _)| *o)
        {
            self.0 = self.0.split_off(&b);
        }
    }

    /// The settle candidate: the highest entry, when its ordinal is `latest`, it is
    /// `End { settle: true }`, the boundary below it is a `Start`, and the boundary below
    /// that start is an `End` or none. Returns its ordinal and commit instant.
    pub(crate) fn candidate(&self, latest: u64) -> Option<(u64, Instant)> {
        let (&s, &(mark, at)) = self.0.iter().next_back()?;
        if s != latest || mark != (TurnMark::End { settle: true }) {
            return None;
        }
        let mut below = self
            .0
            .range(..s)
            .rev()
            .filter(|(_, (m, _))| Self::boundary(m));
        let (&t, (start, _)) = below.next()?;
        if *start != TurnMark::Start {
            return None;
        }
        let _ = t;
        match below.next() {
            None => Some((s, at)),
            Some((_, (TurnMark::End { .. }, _))) => Some((s, at)),
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
    pub human_unsettled: u32,
    /// Each open card's human writer position at its observation; `None` when unreliable.
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
    pub exit_set: Option<Vec<WriteRecord>>,
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
    /// Runs inside the settle job (on the store writer).
    pub settle_job: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs inside the open job (on the store writer).
    pub open_job: Mutex<Option<Box<dyn FnOnce() + Send>>>,
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
    /// Mirrors `state.running.is_some()` for jobs on the store writer.
    pub running_flag: AtomicBool,
    pub state: Mutex<SendState>,
    /// Completions fired and not yet taken by the worker.
    pub fired: Mutex<Vec<Fired>>,
    /// Units submitted whose completion has not fired.
    pub outstanding: AtomicU64,
    pub wake: Notify,
    /// Passes finished (tests wait on it).
    pub passes: AtomicU64,
    pub passed: Notify,
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
            running_flag: AtomicBool::new(false),
            state: Mutex::new(SendState::default()),
            fired: Mutex::new(Vec::new()),
            outstanding: AtomicU64::new(0),
            wake: Notify::new(),
            passes: AtomicU64::new(0),
            passed: Notify::new(),
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

    /// A completion that hands its record to the worker and never blocks; `written` is set
    /// first for a `Written` outcome.
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
                w.store(true, Ordering::Release);
            }
            lock(&me.fired).push(Fired {
                record: r.clone(),
                report,
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
        self.state().human_unsettled += 1;
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        let done = self.completion(None, report);
        s.submit(unit, guard, done).inspect_err(|_| {
            self.outstanding.fetch_sub(1, Ordering::AcqRel);
            let mut st = self.state();
            st.human_unsettled = st.human_unsettled.saturating_sub(1);
        })
    }

    /// Sets the running unit and its flag.
    pub(crate) fn set_running(&self, st: &mut SendState, r: Option<RunningUnit>) {
        self.running_flag.store(r.is_some(), Ordering::SeqCst);
        st.running = r;
    }
}

/// The send side's shared parts: the launches, the decoder, and the driver's channel.
pub(crate) struct SendSide {
    pub map: Mutex<HashMap<LaunchId, Arc<SendLaunch>>>,
    pub decoder: Arc<dyn HookDecoder>,
    /// The readiness mode of every row, when a test sets one.
    pub mode: Option<ReadinessMode>,
    pub ticks: tokio::sync::watch::Sender<Option<Instant>>,
    /// The exit job's next attempt rolls back after it recorded `exit_set` (a test point).
    #[cfg(test)]
    pub exit_rollback_once: AtomicBool,
}

impl SendSide {
    pub(crate) fn launch(&self, l: LaunchId) -> Option<Arc<SendLaunch>> {
        lock(&self.map).get(&l).cloned()
    }

    pub(crate) fn all(&self) -> Vec<Arc<SendLaunch>> {
        lock(&self.map).values().cloned().collect()
    }

    pub(crate) fn mode(&self, caps: &Capabilities) -> ReadinessMode {
        self.mode.unwrap_or_else(|| caps.readiness_mode())
    }
}

/// Creates the send side and registers its exit and tick steps in `cfg` (send §2.9 D4).
pub(crate) fn install(cfg: &mut SupervisorConfig) -> Arc<SendSide> {
    let side = Arc::new(SendSide {
        map: Mutex::new(HashMap::new()),
        decoder: cfg.decoder.clone(),
        mode: cfg.readiness_mode,
        ticks: tokio::sync::watch::channel(None).0,
        #[cfg(test)]
        exit_rollback_once: AtomicBool::new(false),
    });
    cfg.exit_steps
        .push(Arc::new(exit::SendExitStep(side.clone())));
    cfg.tick_steps.push(Arc::new(tick::SendTick(side.clone())));
    side
}

impl Supervisor {
    /// Creates `entry`'s send side and its worker once (the reservation job).
    pub(crate) fn send_launch(self: &Arc<Self>, entry: &LiveLaunch) {
        let mut map = lock(&self.send.map);
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
