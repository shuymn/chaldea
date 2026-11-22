//! The event store: one SQLite database with append-only events, projections, command
//! receipts, hook deliveries, tokens, and blobs (`docs/design/event-store.md`).
//!
//! One writer thread runs every write as one `BEGIN IMMEDIATE` transaction per job; a small
//! pool of read-only connections serves reads, each in one read transaction. See the crate
//! documentation for the rules every operation follows.

mod annex;
mod append;
mod clock;
mod content;
mod error;
mod open;
mod project;
mod readers;
mod rows;
mod schema;
mod write_tx;
mod writer;

#[cfg(test)]
mod tests;

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chaldea_core::ids::{EventId, Timestamp};
use chaldea_core::sync::StreamHeadEntry;
use tokio::sync::{broadcast, oneshot};

pub use annex::{AnnexError, Continuity, CountedClass, CountedClasses, EventAnnex, WriteBasis};
pub use clock::{RandomIds, SystemClock};
pub use content::{ContentError, MAX_BLOB_BYTES, Prepared, prepare_content};
pub use error::{OpenError, ProjectionError, ReadError, StoreError};
pub use readers::{Page, ReadTx};
pub use rows::{
    ApprovalRow, Binding, BlobBody, BufferedDelivery, BufferedHook, HookDelivery,
    HookDeliveryLookup, HookReply, InputRow, LaunchRow, ProjectionDiff, RebuildReport, Routing,
    SchemaInfo, SurfaceRow, ThreadRow, TokenKind, TokenRow, TranscriptCursor, WorktreeRow,
};
pub use write_tx::{
    Appended, BlobInput, CursorPos, NewEvent, NewGeneration, NewHookDelivery, NewRouting, NewToken,
    Promotion, ReadPos, TokenSelector, TranscriptBatch, TranscriptLine, WriteTx,
};

use readers::Pool;
use writer::{ErasedJob, Faults, TypedJob, WriterExit};

/// Where and how to open the store.
pub struct StoreConfig {
    /// `<state_dir>/chaldea.db`. The directory must exist; `open` never creates one.
    pub path: PathBuf,
    /// Read-only connections in the pool (4 in the daemon), 1 to [`MAX_READERS`].
    pub readers: usize,
    /// Jobs the writer queue holds (256 in the daemon), 1 to [`MAX_WRITER_QUEUE`]; `submit`
    /// fails at once beyond it.
    pub writer_queue: usize,
    /// SQLite's busy-handler budget for the write lock (5 s in the daemon), at most
    /// `i32::MAX` milliseconds (SQLite's range). Not a wall-clock bound the store promises.
    pub busy_timeout: Duration,
    /// Source of `recorded_at` and of the deadline checks; tests pass a fixed clock.
    pub clock: Arc<dyn Clock>,
    /// Source of event ids and of the epoch; tests pass a counter.
    pub ids: Arc<dyn IdSource>,
}

/// The largest reader pool `open` accepts.
pub const MAX_READERS: usize = 64;
/// The largest writer queue `open` accepts (the queue's buffer is allocated up front).
pub const MAX_WRITER_QUEUE: usize = 65_536;

/// The time source: a monotonic instant for deadlines and the RFC 3339 UTC timestamp
/// (millisecond precision) recorded with events.
pub trait Clock: Send + Sync {
    fn now(&self) -> (Instant, Timestamp);
}

/// The id source for event ids and the database epoch.
pub trait IdSource: Send + Sync {
    fn event_id(&self) -> EventId;
    fn epoch(&self) -> chaldea_core::ids::Epoch;
}

/// The three files of the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorePaths {
    pub db: PathBuf,
    pub wal: PathBuf,
    pub shm: PathBuf,
}

/// Why `submit` refused a job without queueing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    QueueFull,
    /// `shutdown` began, or the writer thread ended.
    Closed,
    /// The writer is in the failed state (§2.2).
    Failed,
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SubmitError::QueueFull => "the writer queue is full",
            SubmitError::Closed => "the store is closed",
            SubmitError::Failed => "the store writer failed",
        })
    }
}

impl std::error::Error for SubmitError {}

/// What happened to a job. Each variant says whether the job's effects are in the database.
#[derive(Debug)]
pub enum WriteError {
    /// `not_after` passed before the job ran: nothing written.
    Expired,
    /// The write lock was not obtained (another process holds it): nothing written.
    Busy,
    /// The job returned `Err`: rolled back.
    Rejected(JobError),
    /// A statement failed (constraint, trigger, disk full, ...): rolled back.
    RolledBack(StoreError),
    /// The job panicked: rolled back.
    Panicked,
    /// `COMMIT` failed after the transaction ended: may or may not be durable; the writer
    /// is retired.
    CommitUnknown(StoreError),
    /// The connection failed (corrupt, I/O, failed rollback, projection version): the job
    /// did not run or was rolled back, and the store accepts no more writes.
    Failed(StoreError),
    /// The writer thread ended without replying: outcome unknown.
    WriterGone,
    /// `blocking_wait` was called on a store writer thread (inside a job): the ticket's job
    /// cannot run until the calling job returns, so the wait is refused. The ticket's job
    /// stays queued and runs later; its result is dropped.
    WouldDeadlock,
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Expired => f.write_str("the job's deadline passed before it ran"),
            WriteError::Busy => f.write_str("the write lock was not obtained"),
            WriteError::Rejected(e) => write!(f, "the job refused: {e}"),
            WriteError::RolledBack(e) => write!(f, "the job rolled back: {e}"),
            WriteError::Panicked => f.write_str("the job panicked and rolled back"),
            WriteError::CommitUnknown(e) => write!(f, "the commit outcome is unknown: {e}"),
            WriteError::Failed(e) => write!(f, "the store writer failed: {e}"),
            WriteError::WriterGone => f.write_str("the writer ended without replying"),
            WriteError::WouldDeadlock => {
                f.write_str("a job cannot wait on a ticket: the writer runs one job at a time")
            }
        }
    }
}

impl std::error::Error for WriteError {}

/// The error a job closure returns.
#[derive(Debug)]
pub enum JobError {
    /// A store error the job passed on.
    Store(StoreError),
    /// The caller's own refusal (a failed precondition).
    Refused(Box<dyn std::error::Error + Send + Sync>),
}

impl From<StoreError> for JobError {
    fn from(e: StoreError) -> Self {
        JobError::Store(e)
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JobError::Store(e) => e.fmt(f),
            JobError::Refused(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for JobError {}

/// Published after a job committed and its ticket resolved: the streams it advanced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    pub heads: Vec<StreamHeadEntry>,
}

/// The reply to one submitted job.
pub struct WriteTicket<R>(oneshot::Receiver<Result<R, WriteError>>);

impl<R> Future for WriteTicket<R> {
    type Output = Result<R, WriteError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(|r| r.unwrap_or(Err(WriteError::WriterGone)))
    }
}

impl<R> WriteTicket<R> {
    /// Waits for the reply. No deadline of its own: the writer replies to every accepted
    /// job, and a dropped reply is `WriterGone`. Must not be called inside an async runtime;
    /// inside a job (on a writer thread) it returns `WouldDeadlock` at once.
    ///
    /// # Errors
    ///
    /// The job's [`WriteError`].
    pub fn blocking_wait(self) -> Result<R, WriteError> {
        if writer::on_writer_thread() {
            return Err(WriteError::WouldDeadlock);
        }
        self.0
            .blocking_recv()
            .unwrap_or(Err(WriteError::WriterGone))
    }
}

/// The result of the shutdown checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checkpoint {
    /// `wal_checkpoint(TRUNCATE)` completed.
    Complete,
    /// It could not complete (a reader still held a snapshot).
    Busy,
    Error(String),
    /// Its result had not arrived when `shutdown` returned, or the writer had no
    /// connection (failed state).
    NotRun,
}

/// What `shutdown` observed when it returned, as separate facts (§5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Every lent reader connection came back before the deadline.
    pub readers_returned: bool,
    /// Reader connections still lent at the deadline.
    pub readers_abandoned: usize,
    /// The writer thread had exited when `shutdown` returned. PR 8 may release the
    /// single-instance lock only when this is true.
    pub writer_exited: bool,
    /// Jobs the writer ran after `shutdown` began.
    pub jobs_run: u64,
    /// Jobs the writer replied `Expired` to after `shutdown` began.
    pub jobs_expired: u64,
    pub checkpoint: Checkpoint,
}

type JobBox = Box<dyn ErasedJob>;

/// State shared by every clone and the writer thread.
pub(crate) struct Shared {
    /// The queue sender; taken once, under this lock, by the intake transition (shutdown)
    /// or by the writer retiring (failed state).
    sender: Mutex<Option<SyncSender<JobBox>>>,
    failed: AtomicBool,
    shutting_down: AtomicBool,
    /// Shutdown's progress, which every `shutdown` caller and the writer observe.
    stop: Mutex<Stop>,
    stop_cv: Condvar,
}

#[derive(Default)]
struct Stop {
    /// Every reader connection was closed (or the first caller's deadline passed).
    readers_closed: bool,
    /// The writer's final report; published after it closed its connection.
    exit: Option<WriterExit>,
}

impl Shared {
    pub(crate) fn new(sender: SyncSender<JobBox>) -> Self {
        Shared {
            sender: Mutex::new(Some(sender)),
            failed: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            stop: Mutex::new(Stop::default()),
            stop_cv: Condvar::new(),
        }
    }

    /// Lets the writer checkpoint.
    fn close_readers(&self) {
        lock(&self.stop).readers_closed = true;
        self.stop_cv.notify_all();
    }

    pub(crate) fn wait_readers_closed(&self) {
        let mut s = lock(&self.stop);
        while !s.readers_closed {
            s = self.stop_cv.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Records the writer's report (the first one wins).
    pub(crate) fn publish_exit(&self, exit: WriterExit) {
        let mut s = lock(&self.stop);
        if s.exit.is_none() {
            s.exit = Some(exit);
        }
        drop(s);
        self.stop_cv.notify_all();
    }

    /// The writer's report, waiting for it until `deadline`.
    fn wait_exit(&self, deadline: Instant) -> Option<WriterExit> {
        let mut s = lock(&self.stop);
        loop {
            if let Some(e) = &s.exit {
                return Some(e.clone());
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            s = self
                .stop_cv
                .wait_timeout(s, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

struct Inner {
    shared: Arc<Shared>,
    pool: Arc<Pool>,
    epoch: chaldea_core::ids::Epoch,
    paths: StorePaths,
    /// Taken by shutdown, so subscribers see `Closed` once the writer's clone drops too.
    notifier: Mutex<Option<broadcast::Sender<Committed>>>,
    /// Joined only once the thread has finished.
    writer: Mutex<Option<JoinHandle<()>>>,
    faults: Arc<Faults>,
}

/// The store. Cheap to clone; every clone shares the writer, the readers, and shutdown.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("db", &self.inner.paths.db)
            .field("epoch", &self.inner.epoch)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Store {
    /// Opens (creating if missing) the database at `cfg.path` (§2.8).
    ///
    /// # Errors
    ///
    /// [`OpenError`]; every error from the checks of an existing file is returned before
    /// Chaldea changes it, and `open` never deletes a file.
    pub fn open(cfg: StoreConfig) -> Result<Store, OpenError> {
        open::open(cfg, Arc::new(Faults::default()))
    }

    /// The database epoch, created once with the database.
    #[must_use]
    pub fn epoch(&self) -> &chaldea_core::ids::Epoch {
        &self.inner.epoch
    }

    #[must_use]
    pub fn paths(&self) -> StorePaths {
        self.inner.paths.clone()
    }

    /// Queues a write job. Fails at once, without waiting, when the queue is full, closed,
    /// or failed. The job runs only if `not_after` had not passed once the writer held the
    /// write lock.
    ///
    /// # Errors
    ///
    /// [`SubmitError`].
    pub fn submit<R, F>(&self, not_after: Instant, job: F) -> Result<WriteTicket<R>, SubmitError>
    where
        F: FnOnce(&mut WriteTx<'_>) -> Result<R, JobError> + Send + 'static,
        R: Send + 'static,
    {
        let shared = &self.inner.shared;
        let (tx, rx) = oneshot::channel();
        let job: JobBox = Box::new(TypedJob::new(not_after, job, tx));
        // Admission is decided under the sender lock, which the writer holds while it
        // publishes the failed state.
        let guard = lock(&shared.sender);
        if shared.failed.load(Ordering::SeqCst) {
            return Err(SubmitError::Failed);
        }
        let Some(sender) = guard.as_ref() else {
            return Err(if shared.failed.load(Ordering::SeqCst) {
                SubmitError::Failed
            } else {
                SubmitError::Closed
            });
        };
        match sender.try_send(job) {
            Ok(()) => Ok(WriteTicket(rx)),
            Err(TrySendError::Full(_)) => Err(SubmitError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(if shared.failed.load(Ordering::SeqCst) {
                SubmitError::Failed
            } else {
                SubmitError::Closed
            }),
        }
    }

    /// Runs `f` in one read transaction on a pooled read-only connection, so everything it
    /// reads comes from one snapshot. Waits for a free connection until `wait_until`.
    ///
    /// # Errors
    ///
    /// `Busy` when no connection was free in time, `Closed` once `shutdown` began, or the
    /// store error `f` or the transaction returned.
    pub fn read<R>(
        &self,
        wait_until: Instant,
        f: impl FnOnce(&ReadTx<'_>) -> Result<R, StoreError>,
    ) -> Result<R, ReadError> {
        self.inner.pool.read(wait_until, f)
    }

    /// Commit notifications, published after each committed job's reply.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Committed> {
        if let Some(n) = lock(&self.inner.notifier).as_ref() {
            return n.subscribe();
        }
        // After shutdown: a receiver that is already closed.
        broadcast::channel(1).1
    }

    /// Stops intake for every clone, drains the queue, waits for lent readers, lets the
    /// writer checkpoint and exit, and reports what it observed by `deadline` (§5). Any
    /// clone may call it, concurrently too: each caller waits only until its own deadline.
    pub fn shutdown(&self, deadline: Instant) -> ShutdownReport {
        let inner = &self.inner;
        // (1) One intake transition: once `submit` refuses, `read` refuses too.
        {
            let mut sender = lock(&inner.shared.sender);
            inner.shared.shutting_down.store(true, Ordering::SeqCst);
            drop(sender.take());
            inner.pool.close();
        }
        drop(lock(&inner.notifier).take());
        inner.faults.mark("intake_closed");
        // (2) The writer runs the queued jobs meanwhile. (3) Close every reader connection:
        // idle ones now, lent ones as they come back; wait until all are closed.
        inner.pool.drain();
        let readers_returned = inner.pool.wait_closed(deadline);
        let readers_abandoned = inner.pool.open();
        if readers_returned {
            inner.faults.mark("readers_closed");
        }
        inner.shared.close_readers();
        // (4) The writer checkpoints and exits. (5) Wait for its report until the deadline.
        let exit = inner.shared.wait_exit(deadline);
        let writer_exited = exit.is_some();
        if writer_exited {
            let mut w = lock(&inner.writer);
            if w.as_ref().is_some_and(JoinHandle::is_finished)
                && let Some(h) = w.take()
            {
                let _ = h.join();
            }
        }
        let exit = exit.unwrap_or_else(WriterExit::not_reached);
        ShutdownReport {
            readers_returned,
            readers_abandoned,
            writer_exited,
            jobs_run: exit.jobs_run,
            jobs_expired: exit.jobs_expired,
            checkpoint: exit.checkpoint,
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The last clone went away without `shutdown`: stop intake, close the readers, and
        // let the writer finish on its own.
        {
            let mut sender = lock(&self.shared.sender);
            drop(sender.take());
            self.pool.close();
        }
        self.pool.drain();
        self.shared.close_readers();
    }
}
