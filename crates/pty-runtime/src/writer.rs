//! The single writer and its origin log (`docs/design/pty-runtime.md` §2.3).
//!
//! One `pty-writer` thread per session is the only code that writes to the PTY input.
//! Callers submit units; the writer serves the reply lane in sampled batches at unit
//! boundaries and the user lane in FIFO order, so a unit's bytes are one contiguous span.
//! Every write call is committed when it obtains permission under the queue lock; the
//! call itself runs after unlocking.

use std::collections::VecDeque;
use std::fmt;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, MutexGuard, OnceLock, TryLockError, Weak};
use std::time::{Duration, Instant};

use crate::origin::{ClassSet, OriginClass, ReplyKind, WriteOrigin};
use crate::session::{SessionEvent, Shared, lock};
use crate::vt::{ModeView, ModeWord};

/// Most parts in one unit.
pub const MAX_PARTS: usize = 8;

/// A unit of writes: nothing else reaches the PTY while it is in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteUnit {
    pub origin: WriteOrigin,
    /// 1–8 parts, first and last `Bytes`, no empty `Bytes`, no two adjacent `Pause`s.
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// Non-empty; one write call on whole acceptance. An encoder keeps every escape
    /// sequence inside one part.
    Bytes(Vec<u8>),
    /// Between two `Bytes` parts only.
    Pause(Duration),
}

impl WriteUnit {
    /// A unit of one `Bytes` part.
    #[must_use]
    pub fn bytes(origin: WriteOrigin, bytes: impl Into<Vec<u8>>) -> WriteUnit {
        WriteUnit {
            origin,
            parts: vec![Part::Bytes(bytes.into())],
        }
    }

    fn byte_len(&self) -> usize {
        self.parts
            .iter()
            .map(|p| match p {
                Part::Bytes(b) => b.len(),
                Part::Pause(_) => 0,
            })
            .sum()
    }
}

/// The daemon's recheck: runs on the writer thread with every earlier unit finished.
pub type Recheck = Box<dyn FnOnce(&WriteContext) -> Result<(), String> + Send>;

/// Conditions the writer checks before a unit's first byte.
pub struct Guard {
    /// Dispatch deadline: refuse if the first write call has no permission by then.
    pub not_after: Instant,
    /// Refuse if a counted write started after the basis.
    pub basis: Option<Basis>,
    /// Refuse unless 2004 is on with this epoch.
    pub bracketed_paste: Option<u64>,
    /// The daemon's gate or card recheck.
    pub recheck: Option<Recheck>,
    /// Refuse unless the caller's word still holds the value it read.
    pub fence: Option<Fence>,
}

impl Guard {
    /// A guard with only a dispatch deadline.
    #[must_use]
    pub fn until(not_after: Instant) -> Guard {
        Guard {
            not_after,
            basis: None,
            bracketed_paste: None,
            recheck: None,
            fence: None,
        }
    }
}

/// A word the caller advances on events the writer cannot see: the writer refuses
/// [`Refusal::FenceMoved`] when `word` no longer equals `expect`, checked before the recheck
/// and again at the dispatch boundary under the queue lock, as the last check before the
/// unit's first write permission.
#[derive(Debug, Clone)]
pub struct Fence {
    pub word: Arc<AtomicU64>,
    pub expect: u64,
}

impl Fence {
    fn moved(&self) -> bool {
        self.word.load(Ordering::Acquire) != self.expect
    }
}

impl fmt::Debug for Guard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Guard")
            .field("not_after", &self.not_after)
            .field("basis", &self.basis)
            .field("bracketed_paste", &self.bracketed_paste)
            .field("recheck", &self.recheck.is_some())
            .field("fence", &self.fence)
            .finish()
    }
}

/// A `last_started(counted)` value the daemon captured; `counted` is its policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Basis {
    pub seq: u64,
    pub counted: ClassSet,
}

/// What a recheck sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteContext {
    pub seq: u64,
    pub modes: ModeView,
    /// Per `OriginClass::index`.
    pub last_started: [u64; 3],
}

/// Called exactly once per submitted unit; must not block.
pub type Completion = Box<dyn FnOnce(&WriteRecord) + Send>;

/// A completion that sends the record to a receiver.
#[must_use]
pub fn completion_channel() -> (Completion, CompletionReceiver) {
    let (tx, rx) = mpsc::channel();
    let done: Completion = Box::new(move |r: &WriteRecord| {
        let _ = tx.send(r.clone());
    });
    (done, CompletionReceiver { rx })
}

pub struct CompletionReceiver {
    rx: Receiver<WriteRecord>,
}

impl CompletionReceiver {
    /// The record, or `Timeout` past `deadline`.
    pub fn recv_deadline(&self, deadline: Instant) -> Result<WriteRecord, RecvTimeoutError> {
        self.rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
    }
}

/// One write unit's record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRecord {
    /// Per session, from 1, gapless: assigned at dequeue, or at settling for a
    /// never-dequeued unit.
    pub seq: u64,
    pub origin: WriteOrigin,
    pub submitted_at: Instant,
    pub started_at: Option<Instant>,
    pub finished_at: Option<Instant>,
    /// Input bytes written before this unit.
    pub byte_start: u64,
    /// The unit's span is `byte_start..byte_start + bytes_written`.
    pub bytes_written: u64,
    /// `write()` calls that accepted less than their part.
    pub split_writes: u32,
    /// `InProgress` until finished.
    pub outcome: WriteOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    InProgress,
    Written,
    NotStarted(Refusal),
    Incomplete { error: String },
}

/// Why a unit was refused before its first byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Cancelled,
    Expired,
    Closed,
    ChildExited,
    InputBroken,
    BasisUnknown {
        basis: u64,
        head: u64,
    },
    WriteSinceBasis {
        seq: u64,
    },
    BracketedPasteChanged,
    Recheck(String),
    RecheckPanicked,
    /// The guard's fence word moved after the caller read it.
    FenceMoved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    Closed,
    QueueFull,
    BadShape(&'static str),
    UnitTooLarge,
    ReplyOrigin,
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "submit refused: {self:?}")
    }
}

impl std::error::Error for SubmitError {}

/// `writes_after` asked for records the ring no longer holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogTruncated {
    /// Records up to this `seq` may have been evicted.
    pub evicted_through: u64,
}

/// A submitted unit.
pub struct WriteTicket {
    shared: Weak<Shared>,
    id: u64,
    started: Arc<OnceLock<Instant>>,
}

impl WriteTicket {
    /// Cancels the unit if it has not dispatched: true means it never dispatches and its
    /// completion fires `Cancelled`.
    #[must_use = "false means the unit already dispatched or was settled"]
    pub fn cancel(&self) -> bool {
        let Some(sh) = self.shared.upgrade() else {
            return false;
        };
        let mut fired = Fired::default();
        let cancelled = {
            let mut q = lock(&sh.q);
            q.cancel(self.id, &mut fired)
        };
        fired.fire(&sh);
        cancelled
    }

    /// [`WriteTicket::cancel`] without waiting for the queue lock: `None` while another
    /// thread holds it (the writer at its dispatch boundary, for one).
    #[must_use = "Some(false) means the unit already dispatched or was settled"]
    pub fn try_cancel(&self) -> Option<bool> {
        let Some(sh) = self.shared.upgrade() else {
            return Some(false);
        };
        let mut fired = Fired::default();
        let cancelled = {
            let mut q = match sh.q.try_lock() {
                Ok(q) => q,
                Err(TryLockError::Poisoned(e)) => e.into_inner(),
                Err(TryLockError::WouldBlock) => return None,
            };
            q.cancel(self.id, &mut fired)
        };
        fired.fire(&sh);
        Some(cancelled)
    }

    /// The instant of the unit's first write permission, once granted; read without the
    /// queue lock.
    #[must_use]
    pub fn started_at(&self) -> Option<Instant> {
        self.started.get().copied()
    }
}

impl fmt::Debug for WriteTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriteTicket").field("id", &self.id).finish()
    }
}

/// Completions and events to fire after unlocking.
#[derive(Default)]
pub(crate) struct Fired {
    completions: Vec<(Completion, WriteRecord)>,
    pub(crate) events: Vec<SessionEvent>,
    /// Guards of settled units: a recheck capture may call the session when dropped, so
    /// it is dropped here, after unlocking.
    guards: Vec<Guard>,
}

impl Fired {
    /// The records of the units this settled, in order.
    pub(crate) fn records(&self) -> Vec<WriteRecord> {
        self.completions.iter().map(|(_, r)| r.clone()).collect()
    }

    pub(crate) fn fire(self, sh: &Shared) {
        drop(self.guards);
        for (done, rec) in self.completions {
            let _ = catch_unwind(AssertUnwindSafe(move || done(&rec)));
        }
        for e in self.events {
            sh.emit(e);
        }
    }
}

struct Queued {
    id: u64,
    unit: WriteUnit,
    guard: Guard,
    done: Completion,
    submitted_at: Instant,
    bytes: usize,
    /// The ticket's cell for the first permission's instant.
    started: Arc<OnceLock<Instant>>,
}

pub(crate) struct PendingReply {
    pub(crate) kind: ReplyKind,
    pub(crate) bytes: Vec<u8>,
    pub(crate) at: Instant,
}

/// The take-once slot: the unit (or reply) from dequeue to finish.
struct Slot {
    /// The ticket id; `REPLY` for a reply.
    id: u64,
    record: WriteRecord,
    done: Option<Completion>,
}

impl Slot {
    /// Whether the first write permission was granted (`started_at` is set with it).
    fn dispatched(&self) -> bool {
        self.record.started_at.is_some()
    }
}

const REPLY: u64 = 0;

/// Everything under the queue lock.
pub(crate) struct Queue {
    pub(crate) closed: bool,
    /// Close step 3: the writer drops the input and exits.
    pub(crate) stop: bool,
    user: VecDeque<Queued>,
    user_bytes: usize,
    pub(crate) replies: VecDeque<PendingReply>,
    slot: Option<Slot>,
    /// The highest `seq` assigned.
    head: u64,
    last_started: [u64; 3],
    /// Input bytes written so far.
    input_pos: u64,
    ring: VecDeque<WriteRecord>,
    ring_cap: usize,
    evicted_through: u64,
    next_id: u64,
}

impl Queue {
    pub(crate) fn new(ring_cap: usize) -> Queue {
        Queue {
            closed: false,
            stop: false,
            user: VecDeque::new(),
            user_bytes: 0,
            replies: VecDeque::new(),
            slot: None,
            head: 0,
            last_started: [0; 3],
            input_pos: 0,
            ring: VecDeque::new(),
            ring_cap: ring_cap.max(1),
            evicted_through: 0,
            next_id: REPLY + 1,
        }
    }

    /// The highest dispatched `seq` per class, indexed by `OriginClass::index`.
    pub(crate) fn last_started_each(&self) -> [u64; 3] {
        self.last_started
    }

    pub(crate) fn last_started(&self, classes: ClassSet) -> u64 {
        classes
            .iter()
            .map(|c| self.last_started[c.index()])
            .max()
            .unwrap_or(0)
    }

    fn next_seq(&mut self) -> u64 {
        self.head += 1;
        self.head
    }

    /// Inserts a finished record in `seq` order and evicts the oldest past the cap.
    fn log(&mut self, rec: WriteRecord) {
        let at = self.ring.partition_point(|r| r.seq < rec.seq);
        self.ring.insert(at, rec);
        while self.ring.len() > self.ring_cap {
            if let Some(old) = self.ring.pop_front() {
                self.evicted_through = self.evicted_through.max(old.seq);
            }
        }
    }

    pub(crate) fn writes_after(&self, seq: u64) -> Result<Vec<WriteRecord>, LogTruncated> {
        if self.evicted_through > seq {
            return Err(LogTruncated {
                evicted_through: self.evicted_through,
            });
        }
        let from = self.ring.partition_point(|r| r.seq <= seq);
        let mut out: Vec<WriteRecord> = self.ring.range(from..).cloned().collect();
        if let Some(s) = &self.slot
            && s.record.seq > seq
        {
            let at = out.partition_point(|r| r.seq < s.record.seq);
            out.insert(at, s.record.clone());
        }
        Ok(out)
    }

    fn slot_is(&self, seq: u64) -> bool {
        self.slot.as_ref().is_some_and(|s| s.record.seq == seq)
    }

    /// Takes the slot's unit out and settles it with `outcome`.
    fn settle_slot(&mut self, outcome: WriteOutcome, fired: &mut Fired) {
        if let Some(mut s) = self.slot.take() {
            if !matches!(outcome, WriteOutcome::NotStarted(_)) {
                s.record.finished_at = Some(Instant::now());
            }
            s.record.outcome = outcome;
            match s.done {
                Some(done) => {
                    self.log(s.record.clone());
                    fired.completions.push((done, s.record));
                }
                // A reply has no completion: its record moves into the ring.
                None => self.log(s.record),
            }
        }
    }

    /// Settles a never-dequeued unit, giving it its `seq` now.
    fn settle_queued(&mut self, item: Queued, refusal: Refusal, fired: &mut Fired) {
        let rec = WriteRecord {
            seq: self.next_seq(),
            origin: item.unit.origin,
            submitted_at: item.submitted_at,
            started_at: None,
            finished_at: None,
            byte_start: self.input_pos,
            bytes_written: 0,
            split_writes: 0,
            outcome: WriteOutcome::NotStarted(refusal),
        };
        self.log(rec.clone());
        fired.completions.push((item.done, rec));
        fired.guards.push(item.guard);
    }

    fn cancel(&mut self, id: u64, fired: &mut Fired) -> bool {
        if let Some(i) = self.user.iter().position(|u| u.id == id) {
            let item = self.user.remove(i).expect("position is in range");
            self.user_bytes -= item.bytes;
            self.settle_queued(item, Refusal::Cancelled, fired);
            return true;
        }
        if self
            .slot
            .as_ref()
            .is_some_and(|s| s.id == id && !s.dispatched())
        {
            self.settle_slot(WriteOutcome::NotStarted(Refusal::Cancelled), fired);
            return true;
        }
        false
    }

    /// Close step 1: no further permission; every queued and undispatched unit settles
    /// `Closed`; queued replies are discarded. Returns the replies discarded.
    pub(crate) fn close(&mut self, fired: &mut Fired) -> u64 {
        self.closed = true;
        while let Some(item) = self.user.pop_front() {
            self.user_bytes -= item.bytes;
            self.settle_queued(item, Refusal::Closed, fired);
        }
        if self.slot.as_ref().is_some_and(|s| !s.dispatched()) {
            self.settle_slot(WriteOutcome::NotStarted(Refusal::Closed), fired);
        }
        self.discard_replies()
    }

    /// Close step 6: a dispatched unit still in the slot is settled by the closer.
    pub(crate) fn abandon_slot(&mut self, fired: &mut Fired) {
        if self.slot.as_ref().is_some_and(Slot::dispatched) {
            self.settle_slot(
                WriteOutcome::Incomplete {
                    error: "unknown_after_close".into(),
                },
                fired,
            );
        }
    }

    pub(crate) fn discard_replies(&mut self) -> u64 {
        let n = self.replies.len() as u64;
        self.replies.clear();
        n
    }
}

/// Checks a unit's shape and size at submit.
pub(crate) fn validate(
    unit: &WriteUnit,
    max_pause: Duration,
    max_bytes: usize,
) -> Result<usize, SubmitError> {
    if matches!(unit.origin, WriteOrigin::VtReply { .. }) {
        return Err(SubmitError::ReplyOrigin);
    }
    let parts = &unit.parts;
    if parts.is_empty() || parts.len() > MAX_PARTS {
        return Err(SubmitError::BadShape("1 to 8 parts"));
    }
    if !matches!(parts.first(), Some(Part::Bytes(_)))
        || !matches!(parts.last(), Some(Part::Bytes(_)))
    {
        return Err(SubmitError::BadShape("first and last part are bytes"));
    }
    let mut pause = Duration::ZERO;
    for w in parts.windows(2) {
        if matches!(w, [Part::Pause(_), Part::Pause(_)]) {
            return Err(SubmitError::BadShape("adjacent pauses"));
        }
    }
    for p in parts {
        match p {
            Part::Bytes(b) if b.is_empty() => {
                return Err(SubmitError::BadShape("empty bytes part"));
            }
            Part::Bytes(_) => {}
            Part::Pause(d) => {
                pause = pause
                    .checked_add(*d)
                    .ok_or(SubmitError::BadShape("total pause over the limit"))?;
            }
        }
    }
    if pause > max_pause {
        return Err(SubmitError::BadShape("total pause over the limit"));
    }
    let bytes = unit.byte_len();
    if bytes > max_bytes {
        return Err(SubmitError::UnitTooLarge);
    }
    Ok(bytes)
}

impl Shared {
    pub(crate) fn submit(
        self: &Arc<Self>,
        unit: WriteUnit,
        guard: Guard,
        done: Completion,
    ) -> Result<WriteTicket, SubmitError> {
        let bytes = validate(&unit, self.limits.max_pause, self.limits.max_unit_bytes)?;
        let mut q = lock(&self.q);
        if q.closed {
            return Err(SubmitError::Closed);
        }
        if q.user.len() >= self.limits.queue_units || q.user_bytes + bytes > self.limits.queue_bytes
        {
            return Err(SubmitError::QueueFull);
        }
        let id = q.next_id;
        q.next_id += 1;
        q.user_bytes += bytes;
        let started = Arc::new(OnceLock::new());
        q.user.push_back(Queued {
            id,
            unit,
            guard,
            done,
            submitted_at: Instant::now(),
            bytes,
            started: started.clone(),
        });
        drop(q);
        self.cv.notify_all();
        Ok(WriteTicket {
            shared: Arc::downgrade(self),
            id,
            started,
        })
    }

    /// Marks the input broken under the queue lock: queued replies are discarded and
    /// `InputBroken` fires once after unlocking.
    fn break_input(&self, q: &mut Queue, error: &str, fired: &mut Fired) {
        if !self.input_broken.swap(true, Ordering::AcqRel) {
            fired
                .events
                .push(SessionEvent::InputBroken(error.to_owned()));
        }
        let n = q.discard_replies();
        self.responder
            .dropped_after_end
            .fetch_add(n, Ordering::Relaxed);
    }
}

/// The facts a unit's checks read, compared again at the dispatch boundary.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Facts {
    exited: bool,
    broken: bool,
    word: u64,
}

fn facts(sh: &Shared) -> Facts {
    Facts {
        exited: sh.exited(),
        broken: sh.input_broken.load(Ordering::Acquire),
        word: sh.modes.raw(),
    }
}

/// The caller has checked `closed` under the same queue lock.
fn pre_check(q: &Queue, f: Facts, guard: &Guard, seq: u64) -> Option<Refusal> {
    if f.exited {
        return Some(Refusal::ChildExited);
    }
    if f.broken {
        return Some(Refusal::InputBroken);
    }
    if Instant::now() >= guard.not_after {
        return Some(Refusal::Expired);
    }
    if let Some(b) = guard.basis {
        let head = seq - 1;
        if b.seq > head {
            return Some(Refusal::BasisUnknown { basis: b.seq, head });
        }
        let last = q.last_started(b.counted);
        if last > b.seq {
            return Some(Refusal::WriteSinceBasis { seq: last });
        }
    }
    if let Some(epoch) = guard.bracketed_paste {
        let v = ModeWord::view(f.word);
        if !v.modes.bracketed_paste || v.bracketed_paste_epoch != epoch {
            return Some(Refusal::BracketedPasteChanged);
        }
    }
    if guard.fence.as_ref().is_some_and(Fence::moved) {
        return Some(Refusal::FenceMoved);
    }
    None
}

/// The dispatch boundary after a recheck: the facts it saw must still hold.
fn boundary_check(
    before: Facts,
    now: Facts,
    not_after: Instant,
    paste: Option<u64>,
) -> Option<Refusal> {
    if now.exited != before.exited {
        return Some(Refusal::ChildExited);
    }
    if now.broken != before.broken {
        return Some(Refusal::InputBroken);
    }
    if Instant::now() >= not_after {
        return Some(Refusal::Expired);
    }
    if now.word != before.word {
        let (a, b) = (ModeWord::view(before.word), ModeWord::view(now.word));
        let paste_moved = a.bracketed_paste_epoch != b.bracketed_paste_epoch
            || a.modes.bracketed_paste != b.modes.bracketed_paste;
        if paste.is_some() && paste_moved {
            return Some(Refusal::BracketedPasteChanged);
        }
        return Some(Refusal::Recheck("modes_changed".into()));
    }
    None
}

/// The writer thread: waits for the input handoff, then serves both lanes until close
/// step 3. A panic (e.g. in a backend `write`) closes the queue, settles every unit, and
/// marks the input broken.
pub(crate) fn writer_main(sh: &Arc<Shared>, handoff: &Receiver<Box<dyn Write + Send>>) {
    let Ok(mut input) = handoff.recv() else {
        return;
    };
    let r = catch_unwind(AssertUnwindSafe(|| serve(sh, &mut *input)));
    drop(input);
    if r.is_err() {
        let mut fired = Fired::default();
        {
            let mut q = lock(&sh.q);
            // A dispatched unit panicked inside a permitted backend call, which may have
            // written more than the confirmed prefix: its span is uncertain, as after close.
            if q.slot.as_ref().is_some_and(Slot::dispatched) {
                q.settle_slot(
                    WriteOutcome::Incomplete {
                        error: "unknown_after_panic".into(),
                    },
                    &mut fired,
                );
            }
            let n = q.close(&mut fired);
            sh.responder
                .dropped_after_end
                .fetch_add(n, Ordering::Relaxed);
            sh.break_input(&mut q, "writer_panicked", &mut fired);
        }
        sh.cv.notify_all();
        fired.fire(sh);
    }
}

fn wait<'a>(sh: &Shared, q: MutexGuard<'a, Queue>) -> MutexGuard<'a, Queue> {
    sh.cv
        .wait(q)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn serve(sh: &Shared, input: &mut dyn Write) {
    loop {
        let batch = {
            let mut q = lock(&sh.q);
            loop {
                if q.stop {
                    return;
                }
                if !q.closed && (!q.replies.is_empty() || !q.user.is_empty()) {
                    break;
                }
                q = wait(sh, q);
            }
            q.replies.len()
        };
        for _ in 0..batch {
            write_reply(sh, input);
        }
        write_next_unit(sh, input);
    }
}

fn write_reply(sh: &Shared, input: &mut dyn Write) {
    let (seq, bytes) = {
        let mut q = lock(&sh.q);
        // Close, exit, and input breakage each clear the lane under the queue lock, and
        // enqueue refuses after them, so a reply popped here may be written. The lane can
        // be empty when the sampled batch was cleared.
        let Some(r) = q.replies.pop_front() else {
            return;
        };
        let seq = q.next_seq();
        let record = WriteRecord {
            seq,
            origin: WriteOrigin::VtReply { kind: r.kind },
            submitted_at: r.at,
            started_at: Some(Instant::now()),
            finished_at: None,
            byte_start: q.input_pos,
            bytes_written: 0,
            split_writes: 0,
            outcome: WriteOutcome::InProgress,
        };
        q.last_started[OriginClass::Reply.index()] = seq;
        q.slot = Some(Slot {
            id: REPLY,
            record,
            done: None,
        });
        (seq, r.bytes)
    };
    sh.responder.dispatched.fetch_add(1, Ordering::Relaxed);
    write_parts(sh, input, seq, &[Part::Bytes(bytes)]);
}

fn write_next_unit(sh: &Shared, input: &mut dyn Write) {
    let mut fired = Fired::default();
    let mut q = lock(&sh.q);
    if q.closed {
        return;
    }
    let Some(item) = q.user.pop_front() else {
        return;
    };
    q.user_bytes -= item.bytes;
    let seq = q.next_seq();
    let Queued {
        id,
        unit,
        guard,
        done,
        submitted_at,
        started,
        ..
    } = item;
    let class = unit.origin.class();
    q.slot = Some(Slot {
        id,
        record: WriteRecord {
            seq,
            origin: unit.origin,
            submitted_at,
            started_at: None,
            finished_at: None,
            byte_start: q.input_pos,
            bytes_written: 0,
            split_writes: 0,
            outcome: WriteOutcome::InProgress,
        },
        done: Some(done),
    });
    let before = facts(sh);
    if let Some(r) = pre_check(&q, before, &guard, seq) {
        q.settle_slot(WriteOutcome::NotStarted(r), &mut fired);
        drop(q);
        fired.fire(sh);
        return;
    }
    let Guard {
        not_after,
        bracketed_paste,
        recheck,
        fence,
        ..
    } = guard;
    if let Some(recheck) = recheck {
        let ctx = WriteContext {
            seq,
            modes: ModeWord::view(before.word),
            last_started: q.last_started,
        };
        drop(q);
        crate::probe!(sh, RecheckEntered);
        let res = catch_unwind(AssertUnwindSafe(move || recheck(&ctx)));
        q = lock(&sh.q);
        if !q.slot_is(seq) {
            // Cancelled or closed during the recheck: already settled.
            return;
        }
        let refusal = match res {
            Err(_) => Some(Refusal::RecheckPanicked),
            Ok(Err(why)) => Some(Refusal::Recheck(why)),
            Ok(Ok(())) => boundary_check(before, facts(sh), not_after, bracketed_paste),
        };
        if let Some(r) = refusal {
            q.settle_slot(WriteOutcome::NotStarted(r), &mut fired);
            drop(q);
            fired.fire(sh);
            return;
        }
    }
    // Dispatch boundary: the first write call's permission. The deadline and the fence are
    // part of it, decided here under the queue lock.
    crate::probe!(sh, Dispatching);
    let last = if Instant::now() >= not_after {
        Some(Refusal::Expired)
    } else if fence.as_ref().is_some_and(Fence::moved) {
        Some(Refusal::FenceMoved)
    } else {
        None
    };
    if let Some(r) = last {
        q.settle_slot(WriteOutcome::NotStarted(r), &mut fired);
        drop(q);
        fired.fire(sh);
        return;
    }
    let pos = q.input_pos;
    let at = Instant::now();
    if let Some(s) = q.slot.as_mut() {
        s.record.started_at = Some(at);
        s.record.byte_start = pos;
    }
    let _ = started.set(at);
    q.last_started[class.index()] = seq;
    drop(q);
    write_parts(sh, input, seq, &unit.parts);
}

/// Writes a dispatched unit's parts. The first call's permission was granted at dispatch;
/// every later call needs its own, granted only while the queue is open and the child has
/// not exited.
fn write_parts(sh: &Shared, input: &mut dyn Write, seq: u64, parts: &[Part]) {
    let mut fired = Fired::default();
    let mut permitted = true;
    for part in parts {
        match part {
            Part::Pause(d) => {
                crate::probe!(sh, PauseEntered);
                let end = Instant::now() + *d;
                let mut q = lock(&sh.q);
                while !q.closed && !sh.exited() && q.slot_is(seq) {
                    let left = end.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    q = sh
                        .cv
                        .wait_timeout(q, left)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0;
                }
            }
            Part::Bytes(b) => {
                let mut off = 0;
                while off < b.len() {
                    if !permitted {
                        let mut q = lock(&sh.q);
                        if !q.slot_is(seq) {
                            return;
                        }
                        let stop = if q.closed {
                            Some("closed")
                        } else if sh.exited() {
                            Some("child_exited")
                        } else {
                            None
                        };
                        if let Some(e) = stop {
                            q.settle_slot(WriteOutcome::Incomplete { error: e.into() }, &mut fired);
                            drop(q);
                            fired.fire(sh);
                            return;
                        }
                    }
                    permitted = false;
                    crate::probe!(sh, Permitted);
                    let rest = &b[off..];
                    let res = input.write(rest);
                    let mut q = lock(&sh.q);
                    let ours = q.slot_is(seq);
                    match res {
                        Ok(n) if n > 0 => {
                            q.input_pos += n as u64;
                            if let Some(s) = q.slot.as_mut().filter(|_| ours) {
                                s.record.bytes_written += n as u64;
                                if n < rest.len() {
                                    s.record.split_writes += 1;
                                }
                            }
                            off += n;
                        }
                        other => {
                            let error = match other {
                                Err(e) => e.to_string(),
                                Ok(_) => "write_zero".to_owned(),
                            };
                            fail_io(sh, q, seq, &error, fired);
                            return;
                        }
                    }
                }
                if let Err(e) = input.flush() {
                    fail_io(sh, lock(&sh.q), seq, &e.to_string(), fired);
                    return;
                }
            }
        }
    }
    let mut q = lock(&sh.q);
    if q.slot_is(seq) {
        q.settle_slot(WriteOutcome::Written, &mut fired);
    }
    drop(q);
    fired.fire(sh);
}

/// An I/O error or `Ok(0)`: the unit (if still in the slot) ends `Incomplete` with the
/// bytes written so far, and the input is marked broken.
fn fail_io(sh: &Shared, mut q: MutexGuard<'_, Queue>, seq: u64, error: &str, mut fired: Fired) {
    if q.slot_is(seq) {
        q.settle_slot(
            WriteOutcome::Incomplete {
                error: error.to_owned(),
            },
            &mut fired,
        );
    }
    sh.break_input(&mut q, error, &mut fired);
    drop(q);
    fired.fire(sh);
}
