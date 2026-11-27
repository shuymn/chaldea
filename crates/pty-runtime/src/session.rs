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
use crate::snapshot::{Line, ScreenMeta, Snapshot, Walk};
use crate::vt::{ModeView, ModeWord, ReplyTarget, Vt};
use crate::writer::{
    Completion, Fence, Fired, Guard, LogTruncated, PendingReply, Queue, SubmitError, WriteRecord,
    WriteTicket, WriteUnit, writer_main,
};

/// How often the waiter polls the child.
const WAIT_POLL: Duration = Duration::from_millis(100);
/// The backend close a resize runs when a close left the control to it.
const HANDOFF_CLOSE: Duration = Duration::from_secs(2);

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
    /// The queue is sealed: the session is closing.
    Closed,
}

/// A subscription's start: the screen's facts at `meta.output_offset` and its cells, unless
/// their §2.6 encoding went over the subscriber's budget (`lines: None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscribed {
    pub id: SubscriptionId,
    pub meta: ScreenMeta,
    pub lines: Option<Vec<Line>>,
}

/// Why a [`SessionHandle::resize`] did nothing.
#[derive(Debug)]
pub enum ResizeError {
    /// The queue is sealed, or the PTY is closed.
    Closed,
    /// The caller's fence word moved.
    FenceMoved,
    /// The backend refused; neither side changed.
    Failed(io::Error),
}

impl fmt::Display for ResizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResizeError::Closed => f.write_str("pty closed"),
            ResizeError::FenceMoved => f.write_str("fence moved"),
            ResizeError::Failed(e) => write!(f, "resize failed: {e}"),
        }
    }
}

impl std::error::Error for ResizeError {}

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

/// What a close that timed out on the control lock left to the lock's holder.
#[derive(Default)]
struct Handoff {
    /// The holder closes the control when it releases the lock.
    abandoned: bool,
}

/// [`Shared::take_control`]'s result.
enum Ctl {
    /// The lock was taken: the control, or `None` when it was closed already.
    Taken(Option<Box<dyn PtyControl>>),
    /// Still held at the deadline: the holder closes it.
    Held,
}

/// A resize's hold of the control lock: on release (also while unwinding), under the
/// handoff lock, it takes the control out when a close left it to the holder, and closes it
/// after releasing both locks.
struct ControlHold<'a> {
    sh: &'a Shared,
    ctl: Option<MutexGuard<'a, Option<Box<dyn PtyControl>>>>,
}

impl Drop for ControlHold<'_> {
    fn drop(&mut self) {
        // The control leaves the lock under the handoff lock; the backend close runs after
        // both are released, so a close waiting for either is never held up by it.
        let h = lock(&self.sh.handoff);
        let abandoned = if h.abandoned {
            self.ctl.as_mut().and_then(|g| g.take())
        } else {
            None
        };
        drop(self.ctl.take());
        drop(h);
        if let Some(c) = abandoned {
            crate::probe!(self.sh, HandoffClose);
            let deadline = deadline_in(HANDOFF_CLOSE);
            let _ = catch_unwind(AssertUnwindSafe(|| c.close(deadline)));
        }
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
    /// Taken by a close that timed out on the control lock, and by a resize releasing the
    /// control lock (inside it): exactly one of them closes the control.
    handoff: Mutex<Handoff>,
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
                handoff: Mutex::new(Handoff::default()),
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

    /// Takes the control by `deadline`. When the lock is still held then (a resize holds it
    /// across its backend call), the holder is told to close the control when it releases
    /// the lock, unless it released it meanwhile: under the handoff lock, a last `try_lock`
    /// decides which.
    fn take_control(&self, deadline: Instant) -> Ctl {
        crate::probe_deadline!(self, ControlWait, deadline);
        if let Some(mut g) = try_lock_until(&self.control, deadline) {
            return Ctl::Taken(g.take());
        }
        let mut h = lock(&self.handoff);
        h.abandoned = true;
        let taken = match self.control.try_lock() {
            Ok(mut g) => Ctl::Taken(g.take()),
            Err(TryLockError::Poisoned(e)) => Ctl::Taken(e.into_inner().take()),
            Err(TryLockError::WouldBlock) => Ctl::Held,
        };
        drop(h);
        taken
    }

    fn sealed(&self) -> bool {
        lock(&self.q).closed
    }

    fn subscribe(
        &self,
        mut sink: OutputSink,
        deadline: Instant,
        budget: impl FnOnce(&ScreenMeta) -> usize,
    ) -> Result<Subscribed, SubscribeError> {
        if self.sealed() {
            return Err(SubscribeError::Closed);
        }
        let mut st = try_lock_until(&self.vt, deadline).ok_or(SubscribeError::VtUnavailable)?;
        if self.vt_failed.load(Ordering::Acquire) {
            return Err(SubscribeError::VtUnavailable);
        }
        // Under the VT lock (VT lock → queue lock): a seal before it is seen here.
        if self.sealed() {
            return Err(SubscribeError::Closed);
        }
        if st.sub_count() >= self.limits.subscribers {
            return Err(SubscribeError::TooManySubscribers);
        }
        let (meta, lines) = match st.vt.snapshot_within(st.output_offset, deadline, budget) {
            Walk::Copied(snap) => (snap.meta(), Some(snap.lines)),
            Walk::OverBudget(meta) => (meta, None),
            Walk::Late => return Err(SubscribeError::VtUnavailable),
        };
        let id = SubscriptionId(st.next_sub);
        st.next_sub += 1;
        let started = Subscribed { id, meta, lines };
        if let Some(end) = self.output_end.get() {
            drop(st);
            let _ = catch_unwind(AssertUnwindSafe(|| sink(OutputEvent::End(end))));
            return Ok(started);
        }
        st.subs.push(Sub { id, sink });
        Ok(started)
    }

    fn unsubscribe(&self, id: SubscriptionId) {
        let mut st = lock(&self.vt);
        // Taken out and dropped after unlocking: a sink's captures may call the session.
        let removed = take_sub(&mut st.subs, id);
        if removed.is_none() && st.dispatching {
            st.pending_remove.push(id);
        }
        drop(st);
        drop(removed);
    }

    /// The PTY first, without the VT lock, then the grid and the size the responder
    /// reports. The permission is decided after the control lock is taken, under the queue
    /// lock, immediately before the backend call: not sealed and the fence unmoved.
    fn resize(&self, size: PtySize, fence: Option<&Fence>) -> Result<Resized, ResizeError> {
        let held = ControlHold {
            sh: self,
            ctl: Some(lock(&self.control)),
        };
        crate::probe!(self, ResizeLocked);
        {
            let q = lock(&self.q);
            if q.closed {
                return Err(ResizeError::Closed);
            }
            if fence.is_some_and(Fence::moved) {
                return Err(ResizeError::FenceMoved);
            }
        }
        let Some(control) = held.ctl.as_ref().and_then(|c| c.as_ref()) else {
            return Err(ResizeError::Closed);
        };
        control.resize(size).map_err(ResizeError::Failed)?;
        let mut st = lock(&self.vt);
        let mut resized = Resized::PtyOnly;
        if !self.vt_failed.load(Ordering::Acquire) {
            let vt = &mut st.vt;
            if catch_unwind(AssertUnwindSafe(|| vt.resize(size))).is_ok() {
                resized = Resized::PtyAndVt;
            } else {
                self.vt_failed.store(true, Ordering::Release);
            }
        }
        drop(st);
        self.size.store(pack_size(size), Ordering::Release);
        crate::probe!(self, ResizeDone);
        drop(held);
        Ok(resized)
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

/// A close step's deadline: now + its bound, capped at the close's one end.
fn step_by(sh: &Shared, bound: Duration, end: Instant) -> Instant {
    let at = deadline_in(bound).min(end);
    crate::probe_deadline!(sh, Step, at);
    #[cfg(not(feature = "fake"))]
    let _ = sh;
    at
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
        self.screen().snapshot(deadline)
    }

    /// The session's screen, to copy without the session (and without whatever lock the
    /// caller holds the session under).
    #[must_use]
    pub fn screen(&self) -> Screen {
        Screen {
            sh: self.sh.clone(),
        }
    }

    /// The session's subscription and resize side, to call without the session (and
    /// without whatever lock the caller holds the session under).
    #[must_use]
    pub fn handle(&self) -> SessionHandle {
        SessionHandle {
            sh: self.sh.clone(),
        }
    }

    /// A diagnostic snapshot plus every output byte after its offset, then one `End`.
    /// Refused `Closed` once the queue is sealed.
    pub fn subscribe(
        &self,
        sink: OutputSink,
        deadline: Instant,
    ) -> Result<(Snapshot, SubscriptionId), SubscribeError> {
        let s = self.sh.subscribe(sink, deadline, |_| usize::MAX)?;
        let m = s.meta;
        let snap = Snapshot {
            cols: m.cols,
            rows: m.rows,
            lines: s.lines.unwrap_or_default(),
            cursor: m.cursor,
            modes: m.modes,
            output_offset: m.output_offset,
        };
        Ok((snap, s.id))
    }

    /// Works after the seal too.
    pub fn unsubscribe(&self, id: SubscriptionId) {
        self.sh.unsubscribe(id);
    }

    /// Resizes the PTY first, without the VT lock, then the grid and the size the
    /// responder reports. On failure neither side changes. After `VtFailed` only the PTY
    /// is resized, and the result says so. Refused once the queue is sealed. Waits for the
    /// control lock: not bounded.
    pub fn resize(&self, size: PtySize) -> io::Result<Resized> {
        self.sh.resize(size, None).map_err(|e| match e {
            ResizeError::Failed(e) => e,
            other => io::Error::other(other.to_string()),
        })
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
        // One absolute end for every lock wait, the backend close, and the cleanup.
        let end = deadline_in(bounds.total());
        crate::probe_deadline!(self.sh, CloseEnd, end);
        let sh = self.sh.clone();
        let _release = Release {
            sh: &sh,
            waiter_stop: &self.waiter_stop,
            end,
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
                let at = at.unwrap_or_else(|| step_by(&sh, bounds.kill, end));
                self.child.kill_tree(extra, at, at)
            }
        };

        // 3. Release the input.
        sh.stop_writer();
        let _ = wait_finished(self.writer.as_ref(), step_by(&sh, bounds.input, end));

        // 4. Close the PTY while the drain keeps reading. A resize holding the control past
        // the step's deadline closes it itself when it returns.
        let step = deadline_in(bounds.close).min(end);
        let close = match sh.take_control(step) {
            Ctl::Taken(Some(c)) => c.close(step),
            Ctl::Taken(None) => CloseReport {
                elapsed: Some(Duration::ZERO),
            },
            Ctl::Held => CloseReport { elapsed: None },
        };

        // 5. Finish the drain.
        let drain_by = step_by(&sh, bounds.drain, end);
        let output = self.drain.take().and_then(|d| d.finish_by(drain_by));

        // 6. Join and snapshot.
        crate::probe!(sh, CloseJoin);
        let deadline = step_by(&sh, bounds.join, end);
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
/// (kill-on-close) even after a kill that panicked. Bounded by the close's own end; a
/// resize holding the control then closes it itself.
struct Release<'a> {
    sh: &'a Shared,
    waiter_stop: &'a AtomicBool,
    end: Instant,
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.sh.stop_writer();
        self.waiter_stop.store(true, Ordering::Release);
        crate::probe!(self.sh, ReleaseControl);
        if let Ctl::Taken(Some(c)) = self.sh.take_control(self.end) {
            let end = self.end;
            let _ = catch_unwind(AssertUnwindSafe(|| c.close(end)));
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

/// A session's screen, copied without the session: it outlives the session and shows the
/// screen as the session last parsed it.
pub struct Screen {
    sh: Arc<Shared>,
}

impl fmt::Debug for Screen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Screen").finish_non_exhaustive()
    }
}

impl Screen {
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

    /// Whether the session's VT has failed: no snapshot will ever be taken again.
    #[must_use]
    pub fn vt_failed(&self) -> bool {
        self.sh.vt_failed.load(Ordering::Acquire)
    }
}

/// A session's subscription and resize side, without the session: a clone of its shared
/// state, so it outlives the session (every call then refuses or does nothing).
#[derive(Clone)]
pub struct SessionHandle {
    sh: Arc<Shared>,
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionHandle").finish_non_exhaustive()
    }
}

impl SessionHandle {
    /// One atomic load of the mode word.
    #[must_use]
    pub fn modes(&self) -> ModeView {
        self.sh.modes.load()
    }

    /// The current size: the spawn size until a resize succeeds.
    #[must_use]
    pub fn size(&self) -> PtySize {
        unpack_size(self.sh.size.load(Ordering::Acquire))
    }

    /// [`Session::subscribe`] whose copy of the cells, under the VT lock, stops once their
    /// §2.6 encoding would pass `grid_budget(meta)` bytes, checked per cell with the cell's
    /// text (zero-width characters included) and the deadline: the subscription is then
    /// registered as usual, with `lines: None` and the same offset.
    pub fn subscribe(
        &self,
        sink: OutputSink,
        deadline: Instant,
        grid_budget: impl FnOnce(&ScreenMeta) -> usize,
    ) -> Result<Subscribed, SubscribeError> {
        self.sh.subscribe(sink, deadline, grid_budget)
    }

    /// Works after the seal too.
    pub fn unsubscribe(&self, id: SubscriptionId) {
        self.sh.unsubscribe(id);
    }

    /// [`Session::resize`] whose permission also needs `fence` unmoved, checked with the
    /// seal immediately before the backend call. Waits for the control lock.
    pub fn resize(&self, size: PtySize, fence: Option<&Fence>) -> Result<Resized, ResizeError> {
        self.sh.resize(size, fence)
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
