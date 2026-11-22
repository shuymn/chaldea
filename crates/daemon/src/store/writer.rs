//! The writer thread: one read-write connection, a bounded job queue, one `BEGIN IMMEDIATE`
//! transaction per job, deadlines decided under the lock, poisoning, the failed state, and
//! the shutdown checkpoint (`docs/design/event-store.md` §2.2, §5).

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use rusqlite::{Connection, ffi};
use tokio::sync::{broadcast, oneshot};

use super::error::{OpenError, StoreError};
use super::write_tx::WriteTx;
use super::{
    Checkpoint, Clock, Committed, IdSource, JobBox, JobError, Shared, StoreConfig, WriteError, lock,
};

/// What a job's closure did.
pub(crate) enum Ran {
    Ok,
    Err(JobError),
    Panicked,
}

/// A job with its result type erased.
pub(crate) trait ErasedJob: Send {
    fn not_after(&self) -> Instant;
    /// Runs the closure (once) under `catch_unwind`, keeping its `Ok` value for `reply`.
    fn run(&mut self, tx: &mut WriteTx<'_>) -> Ran;
    /// Sends the kept value on `Ok(())`, else the error.
    fn reply(self: Box<Self>, outcome: Result<(), WriteError>);
}

pub(crate) struct TypedJob<R, F> {
    not_after: Instant,
    f: Option<F>,
    out: Option<R>,
    reply: oneshot::Sender<Result<R, WriteError>>,
}

impl<R, F> TypedJob<R, F> {
    pub(crate) fn new(
        not_after: Instant,
        f: F,
        reply: oneshot::Sender<Result<R, WriteError>>,
    ) -> Self {
        TypedJob {
            not_after,
            f: Some(f),
            out: None,
            reply,
        }
    }
}

impl<R, F> ErasedJob for TypedJob<R, F>
where
    F: FnOnce(&mut WriteTx<'_>) -> Result<R, JobError> + Send + 'static,
    R: Send + 'static,
{
    fn not_after(&self) -> Instant {
        self.not_after
    }

    fn run(&mut self, tx: &mut WriteTx<'_>) -> Ran {
        let Some(f) = self.f.take() else {
            return Ran::Panicked;
        };
        match catch_unwind(AssertUnwindSafe(|| f(tx))) {
            Ok(Ok(r)) => {
                self.out = Some(r);
                Ran::Ok
            }
            Ok(Err(e)) => Ran::Err(e),
            Err(_) => Ran::Panicked,
        }
    }

    fn reply(self: Box<Self>, outcome: Result<(), WriteError>) {
        let this = *self;
        let result = match (outcome, this.out) {
            (Ok(()), Some(r)) => Ok(r),
            (Ok(()), None) => Err(WriteError::Panicked),
            (Err(e), _) => Err(e),
        };
        let _ = this.reply.send(result);
    }
}

/// Test-only fault points; every field is inert by default.
#[derive(Default)]
pub(crate) struct Faults {
    /// Makes the next commit fail as named.
    pub(crate) commit: Mutex<Option<CommitFault>>,
    /// Panics the writer thread outside any job, before it takes the next one.
    pub(crate) panic_outside_job: AtomicBool,
    /// Fails `open` after step 5.
    pub(crate) fail_open_after_init: AtomicBool,
    /// Runs inside `open` right after step 1 (create).
    pub(crate) after_create: Option<Box<dyn Fn() + Send + Sync>>,
    /// The projection version this store uses instead of the code's.
    pub(crate) projection_version: Option<u32>,
    /// Ordered marks of shutdown's call boundaries (unit tests only).
    #[cfg(test)]
    pub(crate) marks: Mutex<Vec<&'static str>>,
    #[cfg(test)]
    /// Called at each mark, after recording it; a test can block there.
    pub(crate) on_point: Option<Box<dyn Fn(&'static str) + Send + Sync>>,
}

impl Faults {
    /// Records a call boundary for unit tests; a no-op in other builds.
    pub(crate) fn mark(&self, what: &'static str) {
        #[cfg(test)]
        {
            lock(&self.marks).push(what);
            if let Some(f) = &self.on_point {
                f(what);
            }
        }
        #[cfg(not(test))]
        let _ = what;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code, reason = "constructed by unit tests only"))]
pub(crate) enum CommitFault {
    /// `COMMIT` returns `SQLITE_BUSY` without committing; the transaction stays open.
    BusyBeforeCommit,
    /// The real `COMMIT` runs, then an `SQLITE_IOERR` is reported.
    IoErrAfterCommit,
}

/// The writer's final report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WriterExit {
    pub(crate) checkpoint: Checkpoint,
    pub(crate) jobs_run: u64,
    pub(crate) jobs_expired: u64,
}

impl WriterExit {
    /// The writer ended without a report (a panic outside a job).
    pub(crate) fn gone() -> Self {
        WriterExit {
            checkpoint: Checkpoint::NotRun,
            jobs_run: 0,
            jobs_expired: 0,
        }
    }

    /// The report had not arrived by the deadline.
    pub(crate) fn not_reached() -> Self {
        Self::gone()
    }
}

pub(crate) struct WriterCtx {
    pub(crate) conn: Connection,
    pub(crate) rx: Receiver<JobBox>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) ids: Arc<dyn IdSource>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) notifier: broadcast::Sender<Committed>,
    pub(crate) faults: Arc<super::writer::Faults>,
    pub(crate) projection_version: u32,
    pub(crate) jobs_run: u64,
    pub(crate) jobs_expired: u64,
}

thread_local! {
    /// True on a store writer thread, where waiting on a ticket would wait for itself.
    static ON_WRITER: Cell<bool> = const { Cell::new(false) };
}

/// Whether the calling thread is a store writer thread (inside a job).
pub(crate) fn on_writer_thread() -> bool {
    ON_WRITER.with(Cell::get)
}

/// The writer thread's last statement: publishes its report (or `WriterExit::gone()` after
/// a panic outside a job) once everything else of the thread is dropped (the connection,
/// the queue receiver, the notification sender), so a published report means the thread
/// has finished its work and every subscriber already sees `Closed`.
struct ExitGuard {
    shared: Arc<Shared>,
    faults: Arc<Faults>,
    exit: Option<WriterExit>,
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.faults.mark("writer_exiting");
        self.shared
            .publish_exit(self.exit.take().unwrap_or_else(WriterExit::gone));
    }
}

pub(crate) fn spawn(ctx: WriterCtx) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("store-writer".into())
        .spawn(move || {
            ON_WRITER.with(|w| w.set(true));
            let mut guard = ExitGuard {
                shared: ctx.shared.clone(),
                faults: ctx.faults.clone(),
                exit: None,
            };
            // `run` consumes the context, so it is dropped before the guard publishes.
            guard.exit = Some(ctx.run());
        })
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.extended_code & 0xff == ffi::SQLITE_BUSY)
}

/// Reads `meta.projection_version` and refuses when it is not `code`.
pub(crate) fn check_projection_version(conn: &Connection, code: u32) -> Result<(), StoreError> {
    let stored: String = conn.query_row(
        "SELECT value FROM meta WHERE key = 'projection_version'",
        [],
        |r| r.get(0),
    )?;
    if stored == code.to_string() {
        Ok(())
    } else {
        Err(StoreError::ProjectionVersion { stored, code })
    }
}

/// Rebuilds the projections in one transaction at open (§2.8 step 6).
pub(crate) fn rebuild_at_open(
    conn: &Connection,
    cfg: &StoreConfig,
    projection_version: u32,
) -> Result<(), OpenError> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let (_, now) = cfg.clock.now();
    let mut tx = WriteTx::new(conn, now, cfg.ids.as_ref(), projection_version);
    let result = tx.rebuild_projections();
    drop(tx);
    match result {
        Ok(_) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            if !conn.is_autocommit() {
                let _ = conn.execute_batch("ROLLBACK");
            }
            Err(e.into())
        }
    }
}

enum Next {
    Continue,
    Fail,
}

impl WriterCtx {
    fn run(mut self) -> WriterExit {
        loop {
            if self.faults.panic_outside_job.swap(false, Ordering::SeqCst) {
                panic!("injected writer panic outside a job");
            }
            let Ok(job) = self.rx.recv() else {
                break;
            };
            match self.run_job(job) {
                Next::Continue => {}
                Next::Fail => return self.fail(),
            }
        }
        // The queue closed (shutdown): wait until the owning shutdown has finished with the
        // readers (closed, or abandoned at its deadline), then checkpoint.
        self.shared.wait_readers_released();
        self.faults.mark("checkpoint_begin");
        let checkpoint = match self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                r.get::<_, i64>(0)
            }) {
            Ok(0) => Checkpoint::Complete,
            Ok(_) => Checkpoint::Busy,
            Err(e) if is_busy(&e) => Checkpoint::Busy,
            Err(e) => Checkpoint::Error(StoreError::from(e).to_string()),
        };
        self.faults.mark("checkpoint_end");
        WriterExit {
            checkpoint,
            jobs_run: self.jobs_run,
            jobs_expired: self.jobs_expired,
        }
    }

    /// Publishes the failed state before any reply that ends the writer: from then on no
    /// clone's `submit` is admitted.
    fn retire(&self) {
        let mut sender = lock(&self.shared.sender);
        self.shared.failed.store(true, Ordering::SeqCst);
        drop(sender.take());
    }

    /// Retires the writer, then replies.
    fn reply_and_fail(&self, job: JobBox, e: WriteError) -> Next {
        self.retire();
        job.reply(Err(e));
        Next::Fail
    }

    /// The failed state, entered after `reply_and_fail` retired the writer: the connection
    /// is closed and every queued job gets `Failed`; the thread then ends.
    fn fail(self) -> WriterExit {
        self.faults.mark("fail_begin");
        drop(self.conn);
        while let Ok(job) = self.rx.recv() {
            job.reply(Err(WriteError::Failed(StoreError::Sqlite {
                extended_code: -1,
                message: "the store writer is in the failed state".into(),
            })));
        }
        WriterExit {
            checkpoint: Checkpoint::NotRun,
            jobs_run: self.jobs_run,
            jobs_expired: self.jobs_expired,
        }
    }

    fn run_job(&mut self, mut job: JobBox) -> Next {
        let conn = &self.conn;
        // 1. Expired in the queue.
        if self.clock.now().0 > job.not_after() {
            job.reply(Err(WriteError::Expired));
            count(&self.shared, &mut self.jobs_expired);
            return Next::Continue;
        }
        // 2. The write lock.
        if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
            if is_busy(&e) {
                job.reply(Err(WriteError::Busy));
                count(&self.shared, &mut self.jobs_run);
                return Next::Continue;
            }
            let e = StoreError::from(e);
            if e.is_fatal() {
                return self.reply_and_fail(job, WriteError::Failed(e));
            }
            job.reply(Err(WriteError::Failed(e)));
            return Next::Continue;
        }
        // 3. The deadline again, under the lock; recorded_at comes from this read.
        let (now, recorded_at) = self.clock.now();
        if now > job.not_after() {
            return match rollback(conn) {
                Ok(()) => {
                    job.reply(Err(WriteError::Expired));
                    count(&self.shared, &mut self.jobs_expired);
                    Next::Continue
                }
                Err(e) => self.reply_and_fail(job, WriteError::Failed(e)),
            };
        }
        // 4. Another process may have rebuilt the projections with another `apply`.
        if let Err(e) = check_projection_version(conn, self.projection_version) {
            let _ = rollback(conn);
            return self.reply_and_fail(job, WriteError::Failed(e));
        }
        // 5. The closure.
        let mut tx = WriteTx::new(
            conn,
            recorded_at,
            self.ids.as_ref(),
            self.projection_version,
        );
        let ran = job.run(&mut tx);
        let poison = tx.take_poison();
        let heads = tx.take_heads();
        drop(tx);
        count(&self.shared, &mut self.jobs_run);
        // 7. Roll back after an error, a panic, or a poisoned WriteTx.
        if poison.is_some() || !matches!(ran, Ran::Ok) {
            if let Err(e) = rollback(conn) {
                return self.reply_and_fail(job, WriteError::Failed(e));
            }
            if let Some(e) = poison {
                if e.is_fatal() {
                    return self.reply_and_fail(job, WriteError::Failed(e));
                }
                job.reply(Err(WriteError::RolledBack(e)));
            } else {
                job.reply(Err(match ran {
                    Ran::Err(e) => WriteError::Rejected(e),
                    Ran::Panicked | Ran::Ok => WriteError::Panicked,
                }));
            }
            return Next::Continue;
        }
        // 6. Commit.
        if conn.is_autocommit() {
            job.reply(Err(WriteError::RolledBack(StoreError::Sqlite {
                extended_code: -1,
                message: "the transaction ended without an error".into(),
            })));
            return Next::Continue;
        }
        let fault = lock(&self.faults.commit).take();
        let committed = match fault {
            Some(CommitFault::BusyBeforeCommit) => Err(StoreError::Busy),
            Some(CommitFault::IoErrAfterCommit) => conn
                .execute_batch("COMMIT")
                .map_err(StoreError::from)
                .and(Err(StoreError::Sqlite {
                    extended_code: ffi::SQLITE_IOERR,
                    message: "injected I/O error after COMMIT".into(),
                })),
            None => conn.execute_batch("COMMIT").map_err(StoreError::from),
        };
        match committed {
            Ok(()) => {
                job.reply(Ok(()));
                if !heads.is_empty() {
                    let _ = self.notifier.send(Committed { heads });
                }
                Next::Continue
            }
            Err(e) if !conn.is_autocommit() => match rollback(conn) {
                // The transaction is still open, so it did not commit.
                Ok(()) => {
                    job.reply(Err(WriteError::RolledBack(e)));
                    Next::Continue
                }
                Err(e) => self.reply_and_fail(job, WriteError::Failed(e)),
            },
            Err(e) => self.reply_and_fail(job, WriteError::CommitUnknown(e)),
        }
    }
}

/// Counts a job the writer handled after `shutdown` began.
fn count(shared: &Shared, counter: &mut u64) {
    if shared.shutting_down.load(Ordering::SeqCst) {
        *counter += 1;
    }
}

/// Rolls back unless SQLite already ended the transaction.
fn rollback(conn: &Connection) -> Result<(), StoreError> {
    if conn.is_autocommit() {
        return Ok(());
    }
    conn.execute_batch("ROLLBACK").map_err(StoreError::from)
}
