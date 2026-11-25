//! The session (`docs/design/pty-runtime.md` §2.2): the drain feeding the VT, the single
//! writer, the waiter, subscriptions, resize, and the bounded close order.

use std::fmt;
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, TryLockError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::backend::{
    ChildExit, CloseReport, DrainSink, KillReport, ProcessIdentity, PtyChild, PtyControl,
    PtyRuntime, PtySize, SinkInner, SpawnError, SpawnSpec,
};
use crate::drain::{Drain, DrainEvent};
use crate::limits::{CloseBounds, Limits};
use crate::origin::{ClassSet, ReplyKind};
use crate::snapshot::Snapshot;
use crate::vt::{ModeView, ModeWord, ReplyTarget, Vt};
use crate::writer::{
    Completion, Fired, Guard, LogTruncated, PendingReply, Queue, SubmitError, WriteRecord,
    WriteTicket, WriteUnit, writer_main,
};

/// How often the waiter polls the child.
const WAIT_POLL: Duration = Duration::from_millis(100);

/// Locks `m`, ignoring poisoning: every callback runs inside `catch_unwind`, so a poisoned
/// lock only means a thread died while holding it, and its state is still consistent.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Takes `m` with `try_lock` until `deadline` (at least one attempt).
fn try_lock_until<T>(m: &Mutex<T>, deadline: Instant) -> Option<MutexGuard<'_, T>> {
    loop {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(TryLockError::Poisoned(e)) => return Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => {}
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_micros(500));
    }
}

/// Session configuration.
pub struct SessionConfig {
    /// Non-blocking; each variant at most once, except `ReplyDropped`.
    pub on_event: Box<dyn Fn(SessionEvent) + Send + Sync>,
    pub limits: Limits,
    /// Fails the creation of one thread in `Session::spawn` (tests only).
    #[cfg(feature = "fake")]
    pub start_fault: Option<StartFault>,
}

impl SessionConfig {
    /// Default limits and the given event handler.
    pub fn new(on_event: impl Fn(SessionEvent) + Send + Sync + 'static) -> SessionConfig {
        SessionConfig {
            on_event: Box::new(on_event),
            limits: Limits::default(),
            #[cfg(feature = "fake")]
            start_fault: None,
        }
    }
}

/// The thread whose creation `Session::spawn` fails.
#[cfg(feature = "fake")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartFault {
    Writer,
    Waiter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Exited(ChildExit),
    WaitFailed(String),
    OutputEnded(OutputEnd),
    InputBroken(String),
    VtFailed,
    ReplyDropped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputEnd {
    Eof,
    Error(String),
}

/// A subscriber; non-blocking; may move between threads.
pub type OutputSink = Box<dyn FnMut(OutputEvent<'_>) -> Flow + Send>;

#[derive(Debug)]
pub enum OutputEvent<'a> {
    Bytes { offset: u64, bytes: &'a [u8] },
    End(&'a OutputEnd),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscribeError {
    VtUnavailable,
    TooManySubscribers,
}

/// What a successful `resize` changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Resized {
    /// The PTY, the grid, and the size the responder reports.
    PtyAndVt,
    /// Only the PTY: the VT has failed, so there is no grid and no responder.
    PtyOnly,
}

/// The VT lock was not free by the deadline, or the VT failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VtUnavailable;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResponderStatus {
    /// Replies dispatched: given their write permission. Delivery is in each reply's
    /// record (`writes_after`): a dispatched reply can still fail in the backend.
    pub dispatched: u64,
    /// Replies the lane could not hold.
    pub dropped_overflow: u64,
    /// Replies discarded after exit, input breakage, or close.
    pub dropped_after_end: u64,
    /// The responder stopped because the VT failed.
    pub stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStatus {
    pub exit: Option<ChildExit>,
    pub wait_failed: Option<String>,
    pub output: Option<OutputEnd>,
    pub input_broken: bool,
    pub vt_failed: bool,
    pub responder: ResponderStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseOutcome {
    /// The kill step's report, or the earlier proving report the close reused.
    pub kill: KillReport,
    pub close: CloseReport,
    pub output: Option<OutputEnd>,
    /// What is confirmed released, not just abandoned.
    pub released: Released,
    /// Threads still running: `pty-writer`, `pty-drain`, `pty-wait`.
    pub abandoned: Vec<&'static str>,
    /// Taken in step 6 only if the VT lock was free.
    pub final_snapshot: Option<Snapshot>,
    /// True when the drain ended before the snapshot was taken.
    pub final_after_end: bool,
    pub elapsed: Duration,
    /// The exit the waiter reported by the end of the close; `None` when it had not.
    pub exit: Option<ChildExit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Released {
    pub input: bool,
    pub control: bool,
    pub output: bool,
    pub tree_dead: bool,
}

#[derive(Default)]
pub(crate) struct ResponderCounters {
    pub(crate) dispatched: AtomicU64,
    pub(crate) dropped_overflow: AtomicU64,
    pub(crate) dropped_after_end: AtomicU64,
}

/// A subscriber. It gets whole pieces only: each piece advances `output_offset` and takes
/// the current subscribers under one VT lock hold, so a subscriber registered after that
/// (its snapshot at the new offset) gets the next piece first.
struct Sub {
    id: SubscriptionId,
    sink: OutputSink,
}

/// Everything under the VT lock.
pub(crate) struct VtState {
    vt: Vt,
    output_offset: u64,
    /// Every subscriber except those of a dispatch in progress; during a dispatch, only
    /// the ones added since it began.
    subs: Vec<Sub>,
    /// Removals asked for during a dispatch, applied after it.
    pending_remove: Vec<SubscriptionId>,
    dispatching: bool,
    /// Subscribers taken out for the dispatch in progress.
    in_dispatch: usize,
    next_sub: u64,
}

impl VtState {
    fn sub_count(&self) -> usize {
        self.subs.len() + self.in_dispatch
    }
}

const EV_EXITED: u8 = 1;
const EV_WAIT_FAILED: u8 = 1 << 1;
const EV_OUTPUT_ENDED: u8 = 1 << 2;
const EV_INPUT_BROKEN: u8 = 1 << 3;
const EV_VT_FAILED: u8 = 1 << 4;

/// State shared by the session, its threads, and its sink.
pub(crate) struct Shared {
    /// Queue lock: both lanes, the slot, the counters, the ring. Shared with every
    /// [`WriterObserver`].
    pub(crate) q: Arc<Mutex<Queue>>,
    pub(crate) cv: Condvar,
    /// VT lock: the `Term`, the parser, the subscribers, the output end.
    vt: Mutex<VtState>,
    /// Control lock.
    control: Mutex<Option<Box<dyn PtyControl>>>,
    pub(crate) modes: ModeWord,
    exit: OnceLock<ChildExit>,
    wait_failed: OnceLock<String>,
    output_end: OnceLock<OutputEnd>,
    pub(crate) input_broken: AtomicBool,
    vt_failed: AtomicBool,
    fired_once: AtomicU8,
    pending_reply_dropped: AtomicU64,
    pub(crate) responder: ResponderCounters,
    size: AtomicU32,
    on_event: Box<dyn Fn(SessionEvent) + Send + Sync>,
    pub(crate) limits: Limits,
    #[cfg(feature = "fake")]
    pub(crate) probe: crate::fake::Probe,
    #[cfg(feature = "fake")]
    fail_vt_next: AtomicBool,
}

fn pack_size(s: PtySize) -> u32 {
    u32::from(s.cols()) << 16 | u32::from(s.rows())
}

fn unpack_size(w: u32) -> PtySize {
    let cols = u16::try_from(w >> 16).unwrap_or(u16::MAX);
    let rows = u16::try_from(w & 0xffff).unwrap_or(u16::MAX);
    PtySize::new(cols, rows).expect("only validated sizes are stored")
}

impl Shared {
    fn new(
        size: PtySize,
        on_event: Box<dyn Fn(SessionEvent) + Send + Sync>,
        limits: Limits,
    ) -> Arc<Shared> {
        Arc::new_cyclic(|weak: &Weak<Shared>| {
            let w = weak.clone();
            let target: ReplyTarget = Arc::new(move |kind, bytes| {
                if let Some(sh) = w.upgrade() {
                    sh.enqueue_reply(kind, bytes);
                }
            });
            let vt = Vt::new(size, target);
            let modes = ModeWord::new(vt.modes());
            Shared {
                q: Arc::new(Mutex::new(Queue::new(limits.log_records))),
                cv: Condvar::new(),
                vt: Mutex::new(VtState {
                    vt,
                    output_offset: 0,
                    subs: Vec::new(),
                    pending_remove: Vec::new(),
                    dispatching: false,
                    in_dispatch: 0,
                    next_sub: 1,
                }),
                control: Mutex::new(None),
                modes,
                exit: OnceLock::new(),
                wait_failed: OnceLock::new(),
                output_end: OnceLock::new(),
                input_broken: AtomicBool::new(false),
                vt_failed: AtomicBool::new(false),
                fired_once: AtomicU8::new(0),
                pending_reply_dropped: AtomicU64::new(0),
                responder: ResponderCounters::default(),
                size: AtomicU32::new(pack_size(size)),
                on_event,
                limits,
                #[cfg(feature = "fake")]
                probe: crate::fake::Probe::default(),
                #[cfg(feature = "fake")]
                fail_vt_next: AtomicBool::new(false),
            }
        })
    }

    /// Calls `on_event`, ignoring a panic.
    pub(crate) fn emit(&self, e: SessionEvent) {
        let bit = match &e {
            SessionEvent::Exited(_) => EV_EXITED,
            SessionEvent::WaitFailed(_) => EV_WAIT_FAILED,
            SessionEvent::OutputEnded(_) => EV_OUTPUT_ENDED,
            SessionEvent::InputBroken(_) => EV_INPUT_BROKEN,
            SessionEvent::VtFailed => EV_VT_FAILED,
            SessionEvent::ReplyDropped => 0,
        };
        if bit != 0 && self.fired_once.fetch_or(bit, Ordering::AcqRel) & bit != 0 {
            return;
        }
        let _ = catch_unwind(AssertUnwindSafe(|| (self.on_event)(e)));
    }

    /// The responder's target; runs on the drain thread under the VT lock (VT lock →
    /// queue lock is the lock order).
    /// Stops the writer: it takes no more units and ends once idle.
    fn stop_writer(&self) {
        lock(&self.q).stop = true;
        self.cv.notify_all();
    }

    fn enqueue_reply(&self, kind: ReplyKind, bytes: Vec<u8>) {
        {
            let mut q = lock(&self.q);
            if q.closed || self.exited() || self.input_broken.load(Ordering::Acquire) {
                self.responder
                    .dropped_after_end
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
            if q.replies.len() >= self.limits.reply_queue {
                self.responder
                    .dropped_overflow
                    .fetch_add(1, Ordering::Relaxed);
                self.pending_reply_dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            q.replies.push_back(PendingReply {
                kind,
                bytes,
                at: Instant::now(),
            });
        }
        self.cv.notify_all();
        crate::probe!(self, ReplyEnqueued);
    }

    /// Fires `ReplyDropped` once per overflowed reply; called with no lock held.
    fn fire_reply_dropped(&self) {
        let n = self.pending_reply_dropped.swap(0, Ordering::AcqRel);
        for _ in 0..n {
            self.emit(SessionEvent::ReplyDropped);
        }
    }

    /// Whether the exit is published. Read under the queue lock wherever a write
    /// permission depends on it.
    pub(crate) fn exited(&self) -> bool {
        self.exit.get().is_some()
    }

    /// Publishes the exit under the queue lock, so it is ordered with every write
    /// permission: a permission granted after it sees it.
    fn publish_exit(&self, e: ChildExit) {
        {
            let mut q = lock(&self.q);
            let _ = self.exit.set(e);
            let n = q.discard_replies();
            self.responder
                .dropped_after_end
                .fetch_add(n, Ordering::Relaxed);
        }
        self.cv.notify_all();
        self.emit(SessionEvent::Exited(e));
    }

    fn publish_wait_failed(&self, why: String) {
        let _ = self.wait_failed.set(why.clone());
        self.emit(SessionEvent::WaitFailed(why));
    }

    /// One piece of output (split before every ESC): parse under the VT lock, publish the
    /// mode word, then dispatch to subscribers without the lock.
    fn piece(&self, offset: u64, piece: &[u8]) {
        let end = offset + piece.len() as u64;
        let mut vt_failed_now = false;
        let mut dispatch = {
            let mut st = lock(&self.vt);
            crate::probe!(self, PieceParsing);
            if !self.vt_failed.load(Ordering::Acquire) {
                #[cfg(feature = "fake")]
                let inject = self.fail_vt_next.swap(false, Ordering::AcqRel);
                #[cfg(not(feature = "fake"))]
                let inject = false;
                let vt = &mut st.vt;
                let r = catch_unwind(AssertUnwindSafe(|| {
                    assert!(!inject, "injected VT failure");
                    vt.feed(piece);
                }));
                match r {
                    // Under the queue lock (VT lock → queue lock), so the word is ordered
                    // with every write permission.
                    Ok(()) => {
                        let modes = st.vt.modes();
                        let _q = lock(&self.q);
                        self.modes.publish(modes);
                    }
                    Err(_) => {
                        self.vt_failed.store(true, Ordering::Release);
                        vt_failed_now = true;
                    }
                }
            }
            st.output_offset = end;
            st.dispatching = true;
            let subs = std::mem::take(&mut st.subs);
            st.in_dispatch = subs.len();
            subs
        };
        self.fire_reply_dropped();
        if vt_failed_now {
            self.emit(SessionEvent::VtFailed);
        }
        dispatch.retain_mut(|sub| {
            let ev = OutputEvent::Bytes {
                offset,
                bytes: piece,
            };
            matches!(
                catch_unwind(AssertUnwindSafe(|| (sub.sink)(ev))),
                Ok(Flow::Continue)
            )
        });
        let mut st = lock(&self.vt);
        let removed = std::mem::take(&mut st.pending_remove);
        let gone: Vec<Sub> = dispatch
            .extract_if(.., |s| removed.contains(&s.id))
            .collect();
        // Survivors first, then the subscribers added during the dispatch.
        dispatch.append(&mut st.subs);
        st.subs = dispatch;
        st.dispatching = false;
        st.in_dispatch = 0;
        drop(st);
        // Dropped after unlocking: a sink's captures may call the session.
        drop(gone);
    }

    fn end(&self, end: &OutputEnd) {
        let subs = {
            // Published under the VT lock, where `subscribe` reads it.
            let mut st = lock(&self.vt);
            let _ = self.output_end.set(end.clone());
            st.pending_remove.clear();
            std::mem::take(&mut st.subs)
        };
        for mut sub in subs {
            let _ = catch_unwind(AssertUnwindSafe(|| (sub.sink)(OutputEvent::End(end))));
        }
        self.emit(SessionEvent::OutputEnded(end.clone()));
    }
}

/// Takes the subscriber `id` out of `subs`.
fn take_sub(subs: &mut Vec<Sub>, id: SubscriptionId) -> Option<Sub> {
    let i = subs.iter().position(|s| s.id == id)?;
    Some(subs.remove(i))
}

/// The session's drain sink.
pub(crate) struct SharedSink(Arc<Shared>);

impl SharedSink {
    pub(crate) fn deliver(&self, event: DrainEvent<'_>) {
        match event {
            DrainEvent::Bytes { offset, bytes } => {
                let mut at = offset;
                for piece in split_before_esc(bytes) {
                    self.0.piece(at, piece);
                    at += piece.len() as u64;
                }
            }
            DrainEvent::End(end) => self.0.end(end),
        }
    }
}

/// Splits `bytes` before every ESC byte.
fn split_before_esc(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = bytes;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let cut = rest[1..]
            .iter()
            .position(|&b| b == 0x1b)
            .map_or(rest.len(), |i| i + 1);
        let (piece, tail) = rest.split_at(cut);
        rest = tail;
        Some(piece)
    })
}

/// A running PTY session.
pub struct Session {
    sh: Arc<Shared>,
    child: Arc<dyn PtyChild>,
    identity: ProcessIdentity,
    evidence: Vec<(&'static str, String)>,
    writer: Option<JoinHandle<()>>,
    waiter: Option<JoinHandle<()>>,
    waiter_stop: Arc<AtomicBool>,
    drain: Option<Drain>,
    /// The last kill report and the `extra` list it was made with.
    last_kill: Mutex<Option<(KillReport, Vec<ProcessIdentity>)>>,
    closed: bool,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("identity", &self.identity)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

fn spawn_thread(
    name: &str,
    #[allow(unused_variables, reason = "used only with the fake feature")] fail: bool,
    f: impl FnOnce() + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    #[cfg(feature = "fake")]
    if fail {
        return Err(io::Error::other(format!("injected {name} thread failure")));
    }
    std::thread::Builder::new().name(name.to_owned()).spawn(f)
}

/// A close step's bound in place of one too long for an `Instant`.
const FAR: Duration = Duration::from_secs(24 * 60 * 60);

/// `now + d`, or [`FAR`] from now when `d` is too long for an `Instant`: no close step's or
/// guard's deadline sum panics (`CloseBounds` with `Duration::MAX`), which in the guard,
/// during an unwind, would abort the process.
pub(crate) fn deadline_in(d: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(d)
        .or_else(|| now.checked_add(FAR))
        .unwrap_or(now)
}

/// Waits until `h` finished or `deadline`; true when it finished.
fn wait_finished(h: Option<&JoinHandle<()>>, deadline: Instant) -> bool {
    let Some(h) = h else {
        return true;
    };
    while !h.is_finished() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

impl Session {
    /// Builds the VT sink and the reply lane, calls `runtime.spawn(spec, sink)`, then
    /// creates the writer and waiter threads and hands the input to the writer. A
    /// thread-creation failure stops the threads already created, runs the failure
    /// teardown, and returns `StartedThenFailed`.
    #[allow(
        clippy::result_large_err,
        reason = "spawn is rare; the error carries its cleanup evidence by value"
    )]
    pub fn spawn(
        runtime: &dyn PtyRuntime,
        spec: &SpawnSpec,
        config: SessionConfig,
    ) -> Result<Session, SpawnError> {
        #[cfg(feature = "fake")]
        let (fail_writer, fail_waiter) = (
            config.start_fault == Some(StartFault::Writer),
            config.start_fault == Some(StartFault::Waiter),
        );
        #[cfg(not(feature = "fake"))]
        let (fail_writer, fail_waiter) = (false, false);

        let sh = Shared::new(spec.size, config.on_event, config.limits);
        let sink = DrainSink {
            inner: SinkInner::Session(SharedSink(sh.clone())),
        };
        let spawned = runtime.spawn(spec, sink)?;
        let parts = spawned.into_parts();
        debug_assert_eq!(parts.size, spec.size, "the PTY is opened at spec.size");
        let identity = parts.child.identity().clone();

        let (handoff_tx, handoff_rx) = mpsc::channel::<Box<dyn Write + Send>>();
        let wsh = sh.clone();
        let writer = match spawn_thread("pty-writer", fail_writer, move || {
            writer_main(&wsh, &handoff_rx);
        }) {
            Ok(h) => h,
            Err(error) => {
                drop(handoff_tx);
                let cleanup = parts.teardown();
                return Err(SpawnError::StartedThenFailed {
                    stage: "writer_thread",
                    error,
                    identity: Some(identity),
                    cleanup,
                });
            }
        };
        let waiter_stop = Arc::new(AtomicBool::new(false));
        let (ash, child, stop) = (sh.clone(), parts.child.clone(), waiter_stop.clone());
        let waiter = match spawn_thread("pty-wait", fail_waiter, move || {
            waiter_main(&ash, &*child, &stop);
        }) {
            Ok(h) => h,
            Err(error) => {
                // The writer never got the input: dropping the handoff ends it.
                drop(handoff_tx);
                let _ = wait_finished(Some(&writer), Instant::now() + Duration::from_secs(1));
                if writer.is_finished() {
                    let _ = writer.join();
                }
                let cleanup = parts.teardown();
                return Err(SpawnError::StartedThenFailed {
                    stage: "waiter_thread",
                    error,
                    identity: Some(identity),
                    cleanup,
                });
            }
        };

        let crate::backend::SpawnedParts {
            child,
            output,
            input,
            control,
            evidence,
            ..
        } = parts;
        *lock(&sh.control) = Some(control);
        // The writer thread is alive and waiting on the channel, so this send succeeds.
        let _ = handoff_tx.send(input);
        Ok(Session {
            sh,
            child,
            identity,
            evidence,
            writer: Some(writer),
            waiter: Some(waiter),
            waiter_stop,
            drain: Some(output),
            last_kill: Mutex::new(None),
            closed: false,
        })
    }

    #[must_use]
    pub fn identity(&self) -> &ProcessIdentity {
        &self.identity
    }

    #[must_use]
    pub fn evidence(&self) -> &[(&'static str, String)] {
        &self.evidence
    }

    /// The current size: the spawn size until a resize succeeds.
    #[must_use]
    pub fn size(&self) -> PtySize {
        unpack_size(self.sh.size.load(Ordering::Acquire))
    }

    /// Queues a unit. Never waits for queue space or PTY I/O.
    pub fn submit(
        &self,
        unit: WriteUnit,
        guard: Guard,
        done: Completion,
    ) -> Result<WriteTicket, SubmitError> {
        self.sh.submit(unit, guard, done)
    }

    /// The highest `seq` dispatched in any of `classes`; 0 before any.
    #[must_use]
    pub fn last_started(&self, classes: ClassSet) -> u64 {
        lock(&self.sh.q).last_started(classes)
    }

    /// A reader of the writer's positions that does not borrow the session and outlives it.
    #[must_use]
    pub fn observer(&self) -> WriterObserver {
        WriterObserver {
            q: self.sh.q.clone(),
        }
    }

    /// Close's step 1 alone: closes the queue, so no unit is permitted after it; settles
    /// every unit not yet permitted `Closed` with zero bytes, runs their completions before
    /// it returns, and returns their records. A later call (or the close's own step 1)
    /// settles nothing more and returns none.
    pub fn seal(&self) -> Vec<WriteRecord> {
        let fired = self.close_queue();
        let records = fired.records();
        fired.fire(&self.sh);
        records
    }

    /// Closes the queue and wakes the writer; the caller fires what it settled.
    fn close_queue(&self) -> Fired {
        let mut fired = Fired::default();
        {
            let mut q = lock(&self.sh.q);
            let n = q.close(&mut fired);
            self.sh
                .responder
                .dropped_after_end
                .fetch_add(n, Ordering::Relaxed);
        }
        self.sh.cv.notify_all();
        fired
    }

    /// The records after `seq` (the in-progress one included), from the diagnostic ring.
    pub fn writes_after(&self, seq: u64) -> Result<Vec<WriteRecord>, LogTruncated> {
        lock(&self.sh.q).writes_after(seq)
    }

    /// One atomic load.
    #[must_use]
    pub fn modes(&self) -> ModeView {
        self.sh.modes.load()
    }

    /// Copies the screen, taking the VT lock with `try_lock` until `deadline`.
    pub fn snapshot(&self, deadline: Instant) -> Result<Snapshot, VtUnavailable> {
        if self.sh.vt_failed.load(Ordering::Acquire) {
            return Err(VtUnavailable);
        }
        let st = try_lock_until(&self.sh.vt, deadline).ok_or(VtUnavailable)?;
        if self.sh.vt_failed.load(Ordering::Acquire) {
            return Err(VtUnavailable);
        }
        st.vt
            .snapshot(st.output_offset, deadline)
            .ok_or(VtUnavailable)
    }

    /// A diagnostic snapshot plus every output byte after its offset, then one `End`.
    pub fn subscribe(
        &self,
        mut sink: OutputSink,
        deadline: Instant,
    ) -> Result<(Snapshot, SubscriptionId), SubscribeError> {
        let mut st = try_lock_until(&self.sh.vt, deadline).ok_or(SubscribeError::VtUnavailable)?;
        if self.sh.vt_failed.load(Ordering::Acquire) {
            return Err(SubscribeError::VtUnavailable);
        }
        if st.sub_count() >= self.sh.limits.subscribers {
            return Err(SubscribeError::TooManySubscribers);
        }
        let Some(snap) = st.vt.snapshot(st.output_offset, deadline) else {
            return Err(SubscribeError::VtUnavailable);
        };
        let id = SubscriptionId(st.next_sub);
        st.next_sub += 1;
        if let Some(end) = self.sh.output_end.get() {
            drop(st);
            let _ = catch_unwind(AssertUnwindSafe(|| sink(OutputEvent::End(end))));
            return Ok((snap, id));
        }
        st.subs.push(Sub { id, sink });
        Ok((snap, id))
    }

    pub fn unsubscribe(&self, id: SubscriptionId) {
        let mut st = lock(&self.sh.vt);
        // Taken out and dropped after unlocking: a sink's captures may call the session.
        let removed = take_sub(&mut st.subs, id);
        if removed.is_none() && st.dispatching {
            st.pending_remove.push(id);
        }
        drop(st);
        drop(removed);
    }

    /// Resizes the PTY first, without the VT lock, then the grid and the size the
    /// responder reports. On failure neither side changes. After `VtFailed` only the PTY
    /// is resized, and the result says so. Not bounded.
    pub fn resize(&self, size: PtySize) -> io::Result<Resized> {
        let ctl = lock(&self.sh.control);
        let Some(control) = ctl.as_ref() else {
            return Err(io::Error::other("pty closed"));
        };
        control.resize(size)?;
        let mut st = lock(&self.sh.vt);
        let mut resized = Resized::PtyOnly;
        if !self.sh.vt_failed.load(Ordering::Acquire) {
            let vt = &mut st.vt;
            if catch_unwind(AssertUnwindSafe(|| vt.resize(size))).is_ok() {
                resized = Resized::PtyAndVt;
            } else {
                self.sh.vt_failed.store(true, Ordering::Release);
            }
        }
        self.sh.size.store(pack_size(size), Ordering::Release);
        Ok(resized)
    }

    #[must_use]
    pub fn status(&self) -> SessionStatus {
        let r = &self.sh.responder;
        let vt_failed = self.sh.vt_failed.load(Ordering::Acquire);
        SessionStatus {
            exit: self.sh.exit.get().copied(),
            wait_failed: self.sh.wait_failed.get().cloned(),
            output: self.sh.output_end.get().cloned(),
            input_broken: self.sh.input_broken.load(Ordering::Acquire),
            vt_failed,
            responder: ResponderStatus {
                dispatched: r.dispatched.load(Ordering::Relaxed),
                dropped_overflow: r.dropped_overflow.load(Ordering::Relaxed),
                dropped_after_end: r.dropped_after_end.load(Ordering::Relaxed),
                stopped: vt_failed,
            },
        }
    }

    /// Tree kill only; I/O stays up. [`Session::kill_by`] with `deadline` as `lock_by`.
    ///
    /// `extra` is the caller's full list of the launch's recorded descendants, on every
    /// call, retries included; the report's survivors are diagnostic, not a list to
    /// resubmit. A supervisor retries through `kill` while the session is available.
    pub fn kill(&self, extra: &[ProcessIdentity], deadline: Instant) -> KillReport {
        self.kill_by(extra, deadline, deadline)
    }

    /// [`Session::kill`] whose wait for a kill-state lock another call holds ends at
    /// `lock_by` with a busy report ([`KillReport::is_busy`], no signal): a lock held at the
    /// first try and obtained at or after `lock_by` counts as not taken. A call made at or
    /// after `deadline` still makes no OS call.
    pub fn kill_by(
        &self,
        extra: &[ProcessIdentity],
        lock_by: Instant,
        deadline: Instant,
    ) -> KillReport {
        let report = self.child.kill_tree(extra, lock_by, deadline);
        *lock(&self.last_kill) = Some((report.clone(), extra.to_vec()));
        report
    }

    /// The bounded close order of §2.2.
    ///
    /// `extra` is the caller's full list of the launch's recorded descendants, as for
    /// [`Session::kill`]. The kill step reuses the last kill report when that proved the
    /// tree dead and `extra` is empty. When this consuming close cannot prove death
    /// (`tree_dead: false`), the caller keeps the launch unresolved.
    #[must_use = "the outcome says what was confirmed released"]
    pub fn close(mut self, extra: &[ProcessIdentity], bounds: CloseBounds) -> CloseOutcome {
        self.close_inner(extra, bounds, None)
    }

    /// [`Session::close`] whose kill step waits only until the absolute `kill_deadline`
    /// (also its `lock_by`) instead of `now + bounds.kill`, and reuses the last kill report
    /// when that proved the tree dead for the same `extra` list, empty or not. The other
    /// steps keep `bounds`.
    #[must_use = "the outcome says what was confirmed released"]
    pub fn close_by(
        mut self,
        extra: &[ProcessIdentity],
        kill_deadline: Instant,
        bounds: CloseBounds,
    ) -> CloseOutcome {
        self.close_inner(extra, bounds, Some(kill_deadline))
    }

    /// Test signals.
    #[cfg(feature = "fake")]
    #[must_use]
    pub fn probe(&self) -> crate::fake::Probe {
        self.sh.probe.clone()
    }

    /// The next piece fed to the VT panics inside the VT.
    #[cfg(feature = "fake")]
    pub fn fail_vt_on_next_piece(&self) {
        self.sh.fail_vt_next.store(true, Ordering::Release);
    }

    fn close_inner(
        &mut self,
        extra: &[ProcessIdentity],
        bounds: CloseBounds,
        kill_deadline: Option<Instant>,
    ) -> CloseOutcome {
        self.closed = true;
        let t0 = Instant::now();
        let sh = self.sh.clone();
        let _release = Release {
            sh: &sh,
            waiter_stop: &self.waiter_stop,
            close: bounds.close,
        };

        // 1. Close the queue and settle everything that has not dispatched.
        self.close_queue().fire(&sh);

        // 2. Kill, unless the tree is already reported dead and nothing new is named
        // (`close`), or the same list is named (`close_by`).
        let prior = self
            .last_kill
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let kill = match (prior, kill_deadline) {
            (Some((k, _)), None) if k.tree_dead && extra.is_empty() => k,
            (Some((k, list)), Some(_)) if k.tree_dead && list == extra => k,
            (_, at) => {
                let at = at.unwrap_or_else(|| deadline_in(bounds.kill));
                self.child.kill_tree(extra, at, at)
            }
        };

        // 3. Release the input.
        sh.stop_writer();
        let _ = wait_finished(self.writer.as_ref(), deadline_in(bounds.input));

        // 4. Close the PTY while the drain keeps reading.
        let close = {
            let mut ctl = lock(&sh.control);
            match ctl.take() {
                Some(c) => c.close(deadline_in(bounds.close)),
                None => CloseReport {
                    elapsed: Some(Duration::ZERO),
                },
            }
        };

        // 5. Finish the drain.
        let output = self.drain.take().and_then(|d| d.finish(bounds.drain));

        // 6. Join and snapshot.
        let deadline = deadline_in(bounds.join);
        self.waiter_stop.store(true, Ordering::Release);
        let writer_done = wait_finished(self.writer.as_ref(), deadline);
        if writer_done && let Some(h) = self.writer.take() {
            let _ = h.join();
        }
        let mut fired = Fired::default();
        lock(&sh.q).abandon_slot(&mut fired);
        fired.fire(&sh);
        let (final_snapshot, final_after_end) = if sh.vt_failed.load(Ordering::Acquire) {
            (None, false)
        } else {
            // One attempt, and the copy stops at the join deadline.
            match sh.vt.try_lock() {
                Ok(st) => {
                    let snap = st.vt.snapshot(st.output_offset, deadline);
                    let after_end = snap.is_some() && sh.output_end.get().is_some();
                    (snap, after_end)
                }
                Err(_) => (None, false),
            }
        };
        let waiter_done = wait_finished(self.waiter.as_ref(), deadline);
        if waiter_done && let Some(h) = self.waiter.take() {
            let _ = h.join();
        }

        let mut abandoned = Vec::new();
        if !writer_done {
            abandoned.push("pty-writer");
        }
        if output.is_none() {
            abandoned.push("pty-drain");
        }
        if !waiter_done {
            abandoned.push("pty-wait");
        }
        CloseOutcome {
            released: Released {
                input: writer_done,
                control: close.elapsed.is_some(),
                output: output.is_some(),
                tree_dead: kill.tree_dead,
            },
            kill,
            close,
            output,
            abandoned,
            final_snapshot,
            final_after_end,
            elapsed: t0.elapsed(),
            exit: sh.exit.get().copied(),
        }
    }
}

/// Runs when `close_inner` returns or unwinds (a step's panic, e.g. a backend's kill or
/// close): ends the writer, stops the waiter, and closes the control unless a step already
/// did, so the threads end and drop their references to the input, the child, and the PTY.
/// On Windows the child's job handle closes with the last of them, which ends the tree
/// (kill-on-close) even after a kill that panicked.
struct Release<'a> {
    sh: &'a Shared,
    waiter_stop: &'a AtomicBool,
    close: Duration,
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.sh.stop_writer();
        self.waiter_stop.store(true, Ordering::Release);
        let control = lock(&self.sh.control).take();
        if let Some(c) = control {
            let deadline = deadline_in(self.close);
            let _ = catch_unwind(AssertUnwindSafe(|| c.close(deadline)));
        }
    }
}

/// An implicit drop closes with an empty `extra`: best-effort cleanup that can never name
/// the recorded descendants, so it is never a death proof. A panic inside that cleanup (a
/// backend's kill or close) is caught and the session's remaining parts drop as values: the
/// drop may run during an unwind, where an escaping panic aborts the process.
impl Drop for Session {
    fn drop(&mut self) {
        if !self.closed {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                self.close_inner(&[], CloseBounds::default(), None)
            }));
        }
    }
}

fn waiter_main(sh: &Shared, child: &dyn PtyChild, stop: &AtomicBool) {
    let r = catch_unwind(AssertUnwindSafe(|| {
        while !stop.load(Ordering::Acquire) {
            match child.wait_timeout(WAIT_POLL) {
                Ok(Some(e)) => {
                    sh.publish_exit(e);
                    return;
                }
                Ok(None) => {}
                Err(e) => {
                    sh.publish_wait_failed(e.to_string());
                    return;
                }
            }
        }
    }));
    if r.is_err() {
        sh.publish_wait_failed("waiter panicked".into());
    }
}

/// Reads the writer's dispatch positions without the session: a clone of the session's
/// queue lock, so it outlives the session. `Clone`.
#[derive(Clone)]
pub struct WriterObserver {
    q: Arc<Mutex<Queue>>,
}

impl fmt::Debug for WriterObserver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WriterObserver")
    }
}

impl WriterObserver {
    /// The highest `seq` dispatched in any of `classes`; 0 before any.
    #[must_use]
    pub fn last_started(&self, classes: ClassSet) -> u64 {
        lock(&self.q).last_started(classes)
    }

    /// The highest `seq` dispatched per class, indexed by `OriginClass::index`, under one
    /// queue lock.
    #[must_use]
    pub fn last_started_each(&self) -> [u64; 3] {
        lock(&self.q).last_started_each()
    }

    /// [`WriterObserver::last_started_each`] without waiting: `None` while another thread
    /// holds the queue lock.
    #[must_use]
    pub fn try_last_started_each(&self) -> Option<[u64; 3]> {
        match self.q.try_lock() {
            Ok(q) => Some(q.last_started_each()),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner().last_started_each()),
            Err(TryLockError::WouldBlock) => None,
        }
    }
}

/// A byte channel sink bounded by `capacity` queued bytes. Its producer never waits for
/// the receiver: when the bytes do not fit it drops them, records `Lagged`, and stops;
/// `End` is always recorded.
#[must_use]
pub fn channel_sink(capacity: usize) -> (OutputSink, OutputReceiver) {
    // Unbounded in messages, bounded in bytes by `queued`: `send` never blocks.
    let (tx, rx) = mpsc::channel::<Received>();
    let queued = Arc::new(AtomicUsize::new(0));
    let q = queued.clone();
    let mut stopped = false;
    let sink: OutputSink = Box::new(move |ev| match ev {
        OutputEvent::Bytes { offset, bytes } => {
            if stopped {
                return Flow::Stop;
            }
            // The drain is the only producer, so the check and the add do not race.
            if q.load(Ordering::Acquire) + bytes.len() > capacity {
                stopped = true;
                let _ = tx.send(Received::Lagged { at: offset });
                return Flow::Stop;
            }
            q.fetch_add(bytes.len(), Ordering::AcqRel);
            let sent = tx.send(Received::Bytes {
                offset,
                bytes: bytes.to_vec(),
            });
            if sent.is_ok() {
                Flow::Continue
            } else {
                Flow::Stop
            }
        }
        OutputEvent::End(end) => {
            let _ = tx.send(Received::End(end.clone()));
            Flow::Continue
        }
    });
    let receiver = OutputReceiver {
        rx: Mutex::new(rx),
        queued,
        done: AtomicBool::new(false),
    };
    (sink, receiver)
}

/// The receiving end of [`channel_sink`].
pub struct OutputReceiver {
    rx: Mutex<mpsc::Receiver<Received>>,
    queued: Arc<AtomicUsize>,
    /// The terminal item (`Lagged` or `End`) was returned.
    done: AtomicBool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Received {
    Bytes {
        offset: u64,
        bytes: Vec<u8>,
    },
    End(OutputEnd),
    /// A byte was dropped; `at` is the offset of the first dropped byte.
    Lagged {
        at: u64,
    },
}

impl OutputReceiver {
    /// Holds the receiver's side of the channel until dropped (tests only): the sink must
    /// still make progress.
    #[cfg(feature = "fake")]
    pub fn hold(&self) -> impl Sized + '_ {
        lock(&self.rx)
    }

    /// Every queued chunk, then `Lagged` if a byte was dropped, else `End`; then
    /// `Disconnected`. `Timeout` when nothing arrives within `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Received, RecvTimeoutError> {
        if self.done.load(Ordering::Acquire) {
            return Err(RecvTimeoutError::Disconnected);
        }
        // One deadline for both: another receiver may hold the receiving side.
        let deadline = Instant::now() + timeout;
        let rx = try_lock_until(&self.rx, deadline).ok_or(RecvTimeoutError::Timeout)?;
        let item = rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
        drop(rx);
        match &item {
            Received::Bytes { bytes, .. } => {
                self.queued.fetch_sub(bytes.len(), Ordering::AcqRel);
            }
            Received::End(_) | Received::Lagged { .. } => self.done.store(true, Ordering::Release),
        }
        Ok(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_before_every_esc() {
        let v: Vec<&[u8]> = split_before_esc(b"ab\x1b[1m\x1bc\x1b").collect();
        assert_eq!(v, vec![&b"ab"[..], b"\x1b[1m", b"\x1bc", b"\x1b"]);
        let v: Vec<&[u8]> = split_before_esc(b"\x1b[?2004h").collect();
        assert_eq!(v, vec![&b"\x1b[?2004h"[..]]);
        assert_eq!(split_before_esc(b"").count(), 0);
    }
}
