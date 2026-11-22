//! The session (`docs/design/pty-runtime.md` §2.2): the drain feeding the VT, the single
//! writer, the waiter, subscriptions, resize, and the bounded close order.

use std::fmt;
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
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

/// The VT lock was not free by the deadline, or the VT failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VtUnavailable;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResponderStatus {
    /// Replies dispatched to the PTY.
    pub written: u64,
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
    pub(crate) written: AtomicU64,
    pub(crate) dropped_overflow: AtomicU64,
    pub(crate) dropped_after_end: AtomicU64,
}

struct Sub {
    id: SubscriptionId,
    /// The first output offset this subscriber receives.
    start: u64,
    sink: OutputSink,
}

/// Everything under the VT lock.
pub(crate) struct VtState {
    vt: Vt,
    output_offset: u64,
    subs: Vec<Sub>,
    /// Changes made while a dispatch runs, applied after it.
    pending_add: Vec<Sub>,
    pending_remove: Vec<SubscriptionId>,
    dispatching: bool,
    /// Subscribers taken out for the dispatch in progress.
    in_dispatch: usize,
    output_end: Option<OutputEnd>,
    next_sub: u64,
}

impl VtState {
    fn sub_count(&self) -> usize {
        self.subs.len() + self.in_dispatch + self.pending_add.len()
    }
}

const EV_EXITED: u8 = 1;
const EV_WAIT_FAILED: u8 = 1 << 1;
const EV_OUTPUT_ENDED: u8 = 1 << 2;
const EV_INPUT_BROKEN: u8 = 1 << 3;
const EV_VT_FAILED: u8 = 1 << 4;

/// State shared by the session, its threads, and its sink.
pub(crate) struct Shared {
    /// Queue lock: both lanes, the slot, the counters, the ring.
    pub(crate) q: Mutex<Queue>,
    pub(crate) cv: Condvar,
    /// VT lock: the `Term`, the parser, the subscribers, the output end.
    vt: Mutex<VtState>,
    /// Control lock.
    control: Mutex<Option<Box<dyn PtyControl>>>,
    pub(crate) modes: ModeWord,
    pub(crate) exited: AtomicBool,
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
                q: Mutex::new(Queue::new(limits.log_records)),
                cv: Condvar::new(),
                vt: Mutex::new(VtState {
                    vt,
                    output_offset: 0,
                    subs: Vec::new(),
                    pending_add: Vec::new(),
                    pending_remove: Vec::new(),
                    dispatching: false,
                    in_dispatch: 0,
                    output_end: None,
                    next_sub: 1,
                }),
                control: Mutex::new(None),
                modes,
                exited: AtomicBool::new(false),
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
    fn enqueue_reply(&self, kind: ReplyKind, bytes: Vec<u8>) {
        {
            let mut q = lock(&self.q);
            if q.closed
                || self.exited.load(Ordering::Acquire)
                || self.input_broken.load(Ordering::Acquire)
            {
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

    fn publish_exit(&self, e: ChildExit) {
        let _ = self.exit.set(e);
        self.exited.store(true, Ordering::Release);
        {
            let mut q = lock(&self.q);
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
                    Ok(()) => self.modes.publish(st.vt.modes()),
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
            if end <= sub.start {
                return true;
            }
            let from = usize::try_from(sub.start.saturating_sub(offset)).unwrap_or(0);
            let ev = OutputEvent::Bytes {
                offset: offset + from as u64,
                bytes: &piece[from..],
            };
            matches!(
                catch_unwind(AssertUnwindSafe(|| (sub.sink)(ev))),
                Ok(Flow::Continue)
            )
        });
        let mut st = lock(&self.vt);
        let removed = std::mem::take(&mut st.pending_remove);
        dispatch.retain(|s| !removed.contains(&s.id));
        let added = std::mem::take(&mut st.pending_add);
        dispatch.extend(added);
        st.subs = dispatch;
        st.dispatching = false;
        st.in_dispatch = 0;
    }

    fn end(&self, end: &OutputEnd) {
        let subs = {
            let mut st = lock(&self.vt);
            st.output_end = Some(end.clone());
            let _ = self.output_end.set(end.clone());
            let mut subs = std::mem::take(&mut st.subs);
            subs.append(&mut st.pending_add);
            st.pending_remove.clear();
            subs
        };
        for mut sub in subs {
            let _ = catch_unwind(AssertUnwindSafe(|| (sub.sink)(OutputEvent::End(end))));
        }
        self.emit(SessionEvent::OutputEnded(end.clone()));
    }
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
    last_kill: Mutex<Option<KillReport>>,
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
        Ok(st.vt.snapshot(st.output_offset))
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
        let snap = st.vt.snapshot(st.output_offset);
        let id = SubscriptionId(st.next_sub);
        st.next_sub += 1;
        if let Some(end) = st.output_end.clone() {
            drop(st);
            let _ = catch_unwind(AssertUnwindSafe(|| sink(OutputEvent::End(&end))));
            return Ok((snap, id));
        }
        let sub = Sub {
            id,
            start: st.output_offset,
            sink,
        };
        if st.dispatching {
            st.pending_add.push(sub);
        } else {
            st.subs.push(sub);
        }
        Ok((snap, id))
    }

    pub fn unsubscribe(&self, id: SubscriptionId) {
        let mut st = lock(&self.sh.vt);
        st.subs.retain(|s| s.id != id);
        st.pending_add.retain(|s| s.id != id);
        if st.dispatching {
            st.pending_remove.push(id);
        }
    }

    /// Resizes the PTY first, without the VT lock, then the grid and the size the
    /// responder reports. On failure neither side changes. Not bounded.
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        let ctl = lock(&self.sh.control);
        let Some(control) = ctl.as_ref() else {
            return Err(io::Error::other("pty closed"));
        };
        control.resize(size)?;
        let mut st = lock(&self.sh.vt);
        if !self.sh.vt_failed.load(Ordering::Acquire) {
            let vt = &mut st.vt;
            if catch_unwind(AssertUnwindSafe(|| vt.resize(size))).is_err() {
                self.sh.vt_failed.store(true, Ordering::Release);
            }
        }
        self.sh.size.store(pack_size(size), Ordering::Release);
        Ok(())
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
                written: r.written.load(Ordering::Relaxed),
                dropped_overflow: r.dropped_overflow.load(Ordering::Relaxed),
                dropped_after_end: r.dropped_after_end.load(Ordering::Relaxed),
                stopped: vt_failed,
            },
        }
    }

    /// Tree kill only; I/O stays up.
    pub fn kill(&self, extra: &[ProcessIdentity], deadline: Instant) -> KillReport {
        let report = self.child.kill_tree(extra, deadline);
        *lock(&self.last_kill) = Some(report.clone());
        report
    }

    /// The bounded close order of §2.2.
    #[must_use = "the outcome says what was confirmed released"]
    pub fn close(mut self, extra: &[ProcessIdentity], bounds: CloseBounds) -> CloseOutcome {
        self.close_inner(extra, bounds)
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

    fn close_inner(&mut self, extra: &[ProcessIdentity], bounds: CloseBounds) -> CloseOutcome {
        self.closed = true;
        let t0 = Instant::now();
        let sh = self.sh.clone();

        // 1. Close the queue and settle everything that has not dispatched.
        let mut fired = Fired::default();
        {
            let mut q = lock(&sh.q);
            let n = q.close(&mut fired);
            sh.responder
                .dropped_after_end
                .fetch_add(n, Ordering::Relaxed);
        }
        sh.cv.notify_all();
        fired.fire(&sh);

        // 2. Kill, unless the tree is already reported dead and nothing new is named.
        let prior = lock(&self.last_kill).clone();
        let kill = match prior {
            Some(k) if k.tree_dead && extra.is_empty() => k,
            _ => self.kill(extra, Instant::now() + bounds.kill),
        };

        // 3. Release the input.
        {
            let mut q = lock(&sh.q);
            q.stop = true;
        }
        sh.cv.notify_all();
        let _ = wait_finished(self.writer.as_ref(), Instant::now() + bounds.input);

        // 4. Close the PTY while the drain keeps reading.
        let close = {
            let mut ctl = lock(&sh.control);
            match ctl.take() {
                Some(c) => c.close(Instant::now() + bounds.close),
                None => CloseReport {
                    elapsed: Some(Duration::ZERO),
                },
            }
        };

        // 5. Finish the drain.
        let output = self.drain.take().and_then(|d| d.finish(bounds.drain));

        // 6. Join and snapshot.
        let deadline = Instant::now() + bounds.join;
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
            match sh.vt.try_lock() {
                Ok(st) => (
                    Some(st.vt.snapshot(st.output_offset)),
                    st.output_end.is_some(),
                ),
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
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.close_inner(&[], CloseBounds::default());
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

/// A bounded byte channel sink: when full it drops bytes, records `Lagged`, and stops;
/// `End` is always recorded.
#[must_use]
pub fn channel_sink(capacity: usize) -> (OutputSink, OutputReceiver) {
    let state = Arc::new((Mutex::new(ChannelState::default()), Condvar::new()));
    let s = state.clone();
    let sink: OutputSink = Box::new(move |ev| {
        let (m, cv) = &*s;
        let mut st = lock(m);
        let flow = match ev {
            OutputEvent::Bytes { offset, bytes } => {
                if st.lagged.is_some() {
                    Flow::Stop
                } else if st.queued + bytes.len() > capacity {
                    st.lagged = Some(offset);
                    Flow::Stop
                } else {
                    st.queued += bytes.len();
                    st.chunks.push_back((offset, bytes.to_vec()));
                    Flow::Continue
                }
            }
            OutputEvent::End(end) => {
                st.end = Some(end.clone());
                Flow::Continue
            }
        };
        drop(st);
        cv.notify_all();
        flow
    });
    (sink, OutputReceiver { state })
}

#[derive(Default)]
struct ChannelState {
    chunks: std::collections::VecDeque<(u64, Vec<u8>)>,
    queued: usize,
    lagged: Option<u64>,
    end: Option<OutputEnd>,
    terminal_taken: bool,
}

/// The receiving end of [`channel_sink`].
pub struct OutputReceiver {
    state: Arc<(Mutex<ChannelState>, Condvar)>,
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
    /// Every queued chunk, then `Lagged` if a byte was dropped, else `End`; then
    /// `Disconnected`. `Timeout` when nothing arrives within `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Received, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let (m, cv) = &*self.state;
        let mut st = lock(m);
        loop {
            if let Some((offset, bytes)) = st.chunks.pop_front() {
                st.queued -= bytes.len();
                return Ok(Received::Bytes { offset, bytes });
            }
            if !st.terminal_taken {
                if let Some(at) = st.lagged {
                    st.terminal_taken = true;
                    return Ok(Received::Lagged { at });
                }
                if let Some(end) = st.end.clone() {
                    st.terminal_taken = true;
                    return Ok(Received::End(end));
                }
            } else {
                return Err(RecvTimeoutError::Disconnected);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            st = cv
                .wait_timeout(st, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
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
