//! Store access for handlers (design §2.3): `ReadCtx` for reads with one cutoff, `WriteCtx`
//! for writes that are never dropped by a timeout, and `run_write`, the one
//! submit-and-observe path every write takes from a registered task.
//!
//! The store handle is private to this module and the guard's lookup, so a handler reaches
//! the store only through these contexts.

use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, Bytes};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use tokio::sync::oneshot;

use super::auth::{Authorized, Principal, verify_in_read, verify_opt_in_job};
use super::error::{ApiError, WriteFailure, in_source_chain, read_error, write_failure};
use super::registry::{Closing, TaskKind};
use super::{LifecycleSink, MonotonicClock, Shared, StoreFatal};
use crate::deadline::{Settled, settle_before};
use crate::store::{
    JobError, ReadError, ReadTx, Store, StoreError, SubmitError, WriteError, WriteTx,
};

/// The instant of the connection's first head byte, on the server's clock: the request's
/// origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestStart(pub Instant);

/// The request's deadline on the server's clock: `RequestStart` plus the route's budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline(pub Instant);

/// One write job, built from data only (store §5).
pub(crate) type Job<R> = Box<dyn FnOnce(&mut WriteTx<'_>) -> Result<R, JobError> + Send>;
/// Runs after a job committed, from the task that observed it.
pub(crate) type AfterOk = Box<dyn FnOnce() + Send>;
/// Dropped by the observer as soon as `Store::submit` for its job returns, whatever it
/// returned, or with the observer when the registry refuses or aborts it before it submits
/// (hook-ingest §2.3 `write_then`).
pub(crate) type Release = Box<dyn std::any::Any + Send>;

/// One `Store::read` in `spawn_blocking`, with the pool's wait ending at `deadline` and the
/// result awaited until `deadline` and no longer (design §2.3 "one cutoff"). A store outcome
/// taken before the cutoff maps through §2.9; a read not returned by it is 408, and so is any
/// result taken at or after the deadline (by the server's clock), except reader-pool
/// exhaustion, whose wait ends at the cutoff itself (503 `busy`). A read cut off this way
/// finishes on its own and has no effect.
pub(crate) async fn read_until<R: Send + 'static>(
    shared: &Arc<Shared>,
    deadline: Instant,
    f: impl FnOnce(&ReadTx<'_>) -> Result<R, StoreError> + Send + 'static,
) -> Result<R, ApiError> {
    read_cut(shared, deadline, f, false).await
}

/// [`read_until`]; with `strict`, reader-pool exhaustion taken at or after `deadline` is 408
/// too, like every other result. A ready result is classified by the one clock sample taken
/// when the wait returned it.
async fn read_cut<R: Send + 'static>(
    shared: &Arc<Shared>,
    deadline: Instant,
    f: impl FnOnce(&ReadTx<'_>) -> Result<R, StoreError> + Send + 'static,
    strict: bool,
) -> Result<R, ApiError> {
    // The cutoff is set before the read starts.
    let cutoff = shared.timer(deadline);
    let handle = spawn_read(shared, deadline, f);
    #[cfg(test)]
    shared
        .faults
        .before_cutoff_wait(|| handle.is_finished())
        .await;
    let settled = settle_before(|| shared.cfg.clock.now(), deadline, cutoff, handle).await;
    #[cfg(test)]
    if !matches!(settled, Settled::Elapsed) {
        shared.faults.after_cutoff_sample().await;
    }
    read_outcome(settled, strict)
}

/// A read's [`Settled`] result as its reply: a fired cutoff is 408; a ready result goes
/// through [`classify`], late when its one clock sample was at or after the deadline.
fn read_outcome<R>(settled: Settled<Joined<R>>, strict: bool) -> Result<R, ApiError> {
    match settled {
        Settled::Elapsed => Err(ApiError::request_timeout()),
        Settled::InTime(joined) => classify(false, strict, joined),
        Settled::Late(joined) => classify(true, strict, joined),
    }
}

/// The result of a read spawned by [`spawn_read`].
pub(crate) type Joined<R> = Result<Result<R, ReadError>, tokio::task::JoinError>;

/// Starts one `Store::read` in `spawn_blocking`, the pool's wait ending at `deadline` (on
/// the server's clock). The caller holds the handle and bounds its wait at the same
/// deadline; a socket holds it across loop iterations (design §2.6).
pub(crate) fn spawn_read<R: Send + 'static>(
    shared: &Arc<Shared>,
    deadline: Instant,
    f: impl FnOnce(&ReadTx<'_>) -> Result<R, StoreError> + Send + 'static,
) -> tokio::task::JoinHandle<Result<R, ReadError>> {
    let s = shared.clone();
    tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        s.faults.before_read();
        let r = s.store.read(s.real(deadline), f);
        #[cfg(test)]
        s.faults.after_read();
        r
    })
}

/// A read's result as taken now: any result taken at or after `deadline` (by the server's
/// clock), a failed join included, is 408, except reader-pool exhaustion, whose wait ends at
/// the cutoff itself (503 `busy`); otherwise the store outcome maps through §2.9.
pub(crate) fn take_read<R>(
    shared: &Shared,
    deadline: Instant,
    joined: Joined<R>,
) -> Result<R, ApiError> {
    classify(shared.passed(deadline), false, joined)
}

/// A read's result against one clock sample: `late` (taken at or after the deadline) is
/// 408, a failed join included, except reader-pool exhaustion unless `strict` (its wait ends
/// at the cutoff itself: 503 `busy`); otherwise the store outcome maps through §2.9.
fn classify<R>(late: bool, strict: bool, joined: Joined<R>) -> Result<R, ApiError> {
    match joined {
        Ok(Err(ReadError::Busy)) if !strict => Err(ApiError::busy()),
        _ if late => Err(ApiError::request_timeout()),
        Err(_) => Err(ApiError::internal()),
        Ok(Err(e)) => Err(read_error(&e)),
        Ok(Ok(v)) => Ok(v),
    }
}

/// A token check by hash in one read with the request's cutoff.
pub(crate) async fn verify_until(
    shared: &Arc<Shared>,
    deadline: Instant,
    sha256: [u8; 32],
) -> Result<Authorized, ApiError> {
    read_until(shared, deadline, move |tx| verify_in_read(tx, &sha256)).await?
}

/// Awaits the future `start` makes until `deadline`: not done by then, or done at or after
/// it (by the server's clock, even when ready at once), is 408. The timer is set before
/// `start` runs.
pub(crate) async fn until_deadline<F: Future>(
    shared: &Shared,
    deadline: Instant,
    start: impl FnOnce() -> F,
) -> Result<F::Output, ApiError> {
    let cutoff = shared.timer(deadline);
    settle_before(|| shared.cfg.clock.now(), deadline, cutoff, start())
        .await
        .in_time()
        .ok_or_else(ApiError::request_timeout)
}

/// Collects `body` until `deadline` (408, [`until_deadline`]), at most `max` bytes (413).
pub(crate) async fn body_within(
    shared: &Shared,
    body: Body,
    deadline: Instant,
    max: u64,
) -> Result<Bytes, ApiError> {
    let max = usize::try_from(max).unwrap_or(usize::MAX);
    match until_deadline(shared, deadline, || Limited::new(body, max).collect()).await? {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(e) if in_source_chain(e.as_ref(), <dyn std::error::Error>::is::<LengthLimitError>) => {
            Err(ApiError::payload_too_large())
        }
        Err(_) => Err(ApiError::bad_request()),
    }
}

/// Submits `job` with `not_after` (on the server's clock), awaits its ticket (the writer
/// answers every accepted job, store §5), reports a fatal outcome to the lifecycle sink, and
/// runs `after_ok` on success. Called by an already registered task: an observer of
/// [`observe_write`] or a command runner.
pub(crate) async fn run_write<R: Send + 'static>(
    shared: &Shared,
    not_after: Instant,
    job: Job<R>,
    after_ok: Option<AfterOk>,
) -> Result<R, WriteFailure> {
    run_write_releasing(shared, not_after, job, after_ok, None).await
}

/// [`run_write`] that drops `release` as soon as `Store::submit` returns.
async fn run_write_releasing<R: Send + 'static>(
    shared: &Shared,
    not_after: Instant,
    job: Job<R>,
    after_ok: Option<AfterOk>,
    release: Option<Release>,
) -> Result<R, WriteFailure> {
    #[cfg(test)]
    shared.faults.write_submitted(not_after);
    #[cfg(test)]
    shared.faults.before_submit().await;
    let ticket = submit_with(
        &shared.store,
        shared.sink.as_ref(),
        shared.cfg.clock.as_ref(),
        not_after,
        job,
        // The job is in the writer's queue now, or never will be.
        move || drop(release),
    )
    .map_err(WriteFailure::Submit)?;
    #[cfg(test)]
    let ticket = shared.faults.signal_ticket_park(ticket);
    let result = await_ticket(ticket, shared.sink.as_ref(), after_ok).await;
    #[cfg(test)]
    if matches!(result, Err(WriteFailure::Write(WriteError::Expired))) {
        shared.faults.write_expired();
    }
    result
}

/// The one submit-and-observe path of a write, for the server and for components that
/// hold the store, the sink, and a clock but no server: submits `job` with `not_after` on
/// `clock`, calls `on_submit` synchronously right after `Store::submit` returns, whatever it
/// returned and before that result is examined, then awaits the writer's answer. A
/// `SubmitError::Failed`, `CommitUnknown`, `WriterGone`, or `Failed` is reported to `sink`;
/// `after_ok` runs once the job committed.
///
/// # Errors
///
/// The submission's or the write's failure.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by the supervisor in PR 10a1 part 2")
)]
pub(crate) async fn write_with<R: Send + 'static>(
    store: &Store,
    sink: &dyn LifecycleSink,
    clock: &dyn MonotonicClock,
    not_after: Instant,
    job: Job<R>,
    after_ok: Option<AfterOk>,
    on_submit: impl FnOnce(),
) -> Result<R, WriteFailure> {
    let ticket =
        submit_with(store, sink, clock, not_after, job, on_submit).map_err(WriteFailure::Submit)?;
    await_ticket(ticket, sink, after_ok).await
}

/// [`write_with`]'s submission: `Store::submit`, then `on_submit`, then the submission's
/// result examined (`SubmitError::Failed` reported to `sink`).
fn submit_with<R: Send + 'static>(
    store: &Store,
    sink: &dyn LifecycleSink,
    clock: &dyn MonotonicClock,
    not_after: Instant,
    job: Job<R>,
    on_submit: impl FnOnce(),
) -> Result<crate::store::WriteTicket<R>, SubmitError> {
    let submitted = store.submit(clock.real(not_after), job);
    on_submit();
    submitted.inspect_err(|e| {
        if *e == SubmitError::Failed {
            sink.store_fatal(StoreFatal::Failed);
        }
    })
}

/// [`write_with`]'s wait: the writer's answer to an admitted job, a fatal outcome reported to
/// `sink`, `after_ok` run once it committed.
async fn await_ticket<R>(
    ticket: impl Future<Output = Result<R, WriteError>>,
    sink: &dyn LifecycleSink,
    after_ok: Option<AfterOk>,
) -> Result<R, WriteFailure> {
    match ticket.await {
        Ok(r) => {
            if let Some(f) = after_ok {
                f();
            }
            Ok(r)
        }
        Err(e) => {
            let fatal = match &e {
                WriteError::CommitUnknown(_) => Some(StoreFatal::CommitUnknown),
                WriteError::WriterGone => Some(StoreFatal::WriterGone),
                WriteError::Failed(_) => Some(StoreFatal::Failed),
                _ => None,
            };
            if let Some(f) = fatal {
                sink.store_fatal(f);
            }
            Err(WriteFailure::Write(e))
        }
    }
}

/// For plain writes: registers one observer task running [`run_write`] and returns its
/// result on a oneshot. Refused once the registry is closing.
#[cfg(test)]
pub(crate) fn observe_write<R: Send + 'static>(
    shared: &Arc<Shared>,
    not_after: Instant,
    job: Job<R>,
    after_ok: Option<AfterOk>,
) -> Result<oneshot::Receiver<Result<R, WriteFailure>>, Closing> {
    observe_write_releasing(shared, not_after, job, after_ok, None)
}

/// [`observe_write`] whose observer owns `release` from its registration: a refused or
/// aborted observer drops it with its future, a running one at submission.
fn observe_write_releasing<R: Send + 'static>(
    shared: &Arc<Shared>,
    not_after: Instant,
    job: Job<R>,
    after_ok: Option<AfterOk>,
    release: Option<Release>,
) -> Result<oneshot::Receiver<Result<R, WriteFailure>>, Closing> {
    let (tx, rx) = oneshot::channel();
    let s = shared.clone();
    shared.registry.spawn(TaskKind::Write, async move {
        let r = run_write_releasing(&s, not_after, job, after_ok, release).await;
        let _ = tx.send(r);
    })?;
    Ok(rx)
}

/// A plain write's reply: past the deadline, 408 with nothing submitted; otherwise the job
/// submitted with `not_after = deadline` through an observer, its result awaited until
/// `deadline + write_reply_grace`. No reply by then, or a reply taken at or after it (by the
/// server's clock), is 503 `outcome_unknown` (never `in_progress`: no receipt exists); the
/// observer still sees the job through.
pub(crate) async fn await_write<R: Send + 'static>(
    shared: &Arc<Shared>,
    deadline: Instant,
    job: Job<R>,
    after_ok: Option<AfterOk>,
) -> Result<R, ApiError> {
    await_write_releasing(shared, deadline, job, after_ok, None).await
}

/// [`await_write`] whose observer drops `release` once `Store::submit` returned (or with
/// the observer, refused or aborted before it submitted); past the deadline it is dropped
/// here, with nothing submitted.
async fn await_write_releasing<R: Send + 'static>(
    shared: &Arc<Shared>,
    deadline: Instant,
    job: Job<R>,
    after_ok: Option<AfterOk>,
    release: Option<Release>,
) -> Result<R, ApiError> {
    if shared.passed(deadline) {
        return Err(ApiError::request_timeout());
    }
    // The reply cutoff is set before the job is submitted.
    let reply_by = deadline + shared.cfg.write_reply_grace;
    let cutoff = shared.timer(reply_by);
    let rx = observe_write_releasing(shared, deadline, job, after_ok, release)
        .map_err(|_| ApiError::shutting_down())?;
    #[cfg(test)]
    shared.faults.before_cutoff_wait(|| !rx.is_empty()).await;
    match reply_cutoff(cutoff, rx).await {
        Err(_) | Ok(Err(_)) => Err(ApiError::outcome_unknown()),
        Ok(Ok(_)) if shared.passed(reply_by) => Err(ApiError::outcome_unknown()),
        Ok(Ok(r)) => r.map_err(write_failure),
    }
}

/// The plain write's reply wait. Not [`settle_before`]: a closed reply channel is 503
/// before any clock reading, and only a reply is classified by one sample (design §2.3).
#[allow(
    clippy::disallowed_methods,
    reason = "the plain write's reply wait: a closed channel answers before any clock reading (http-surface §2.3)"
)]
fn reply_cutoff<T>(
    cutoff: tokio::time::Instant,
    rx: oneshot::Receiver<T>,
) -> tokio::time::Timeout<oneshot::Receiver<T>> {
    tokio::time::timeout_at(cutoff, rx)
}

/// What a read mount's handler gets (design §2.3).
pub struct ReadCtx {
    principal: Option<Principal>,
    deadline: Instant,
    shared: Arc<Shared>,
}

impl ReadCtx {
    pub(crate) fn new(
        principal: Option<Principal>,
        deadline: Instant,
        shared: Arc<Shared>,
    ) -> Self {
        ReadCtx {
            principal,
            deadline,
            shared,
        }
    }

    #[must_use]
    pub fn principal(&self) -> Option<&Principal> {
        self.principal.as_ref()
    }

    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// The store's epoch.
    #[must_use]
    pub fn epoch(&self) -> &chaldea_core::ids::Epoch {
        self.shared.store.epoch()
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// The blob slots (design §2.7), for a read closure's non-blocking admission.
    pub(crate) fn blob_slots(&self) -> Arc<tokio::sync::Semaphore> {
        self.shared.blob_slots.clone()
    }

    /// One read with the request's cutoff; the token is verified first in the same snapshot
    /// as everything `f` reads. A route without a credential has no principal and cannot
    /// read (500).
    ///
    /// # Errors
    ///
    /// 401 for a token no longer valid, `f`'s own refusal, or the read mapping (§2.9).
    pub async fn read<R: Send + 'static>(
        &self,
        f: impl FnOnce(&ReadTx<'_>, &Authorized) -> Result<R, ApiError> + Send + 'static,
    ) -> Result<R, ApiError> {
        let Some(p) = &self.principal else {
            return Err(ApiError::internal());
        };
        let sha = p.token_sha256;
        read_until(&self.shared, self.deadline, move |tx| {
            Ok(verify_in_read(tx, &sha)?.and_then(|auth| f(tx, &auth)))
        })
        .await?
    }

    /// The request body, by the deadline (408) and at most `max` bytes (413).
    ///
    /// # Errors
    ///
    /// 408, 413, or 400 for a body that ended abnormally.
    pub async fn body_within(&self, body: Body, max: u64) -> Result<Bytes, ApiError> {
        body_within(&self.shared, body, self.deadline, max).await
    }
}

/// What a write mount's handler gets (design §2.3). Writes are never wrapped in a dropping
/// timeout: a submitted write is observed by a registered task.
pub struct WriteCtx {
    principal: Option<Principal>,
    deadline: Instant,
    shared: Arc<Shared>,
}

impl WriteCtx {
    pub(crate) fn new(
        principal: Option<Principal>,
        deadline: Instant,
        shared: Arc<Shared>,
    ) -> Self {
        WriteCtx {
            principal,
            deadline,
            shared,
        }
    }

    #[must_use]
    pub fn principal(&self) -> Option<&Principal> {
        self.principal.as_ref()
    }

    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// The request body, by the deadline (408, nothing submitted) and at most `max` bytes.
    ///
    /// # Errors
    ///
    /// 408, 413, or 400 for a body that ended abnormally.
    pub async fn body_within(&self, body: Body, max: u64) -> Result<Bytes, ApiError> {
        body_within(&self.shared, body, self.deadline, max).await
    }

    /// One write without a receipt: the token (if any) is verified first in the job's
    /// transaction, then `job` runs; submitted with `not_after = deadline`, its result
    /// awaited until `deadline + write_reply_grace`.
    ///
    /// # Errors
    ///
    /// 401 for a token no longer valid, the job's own refusal (`JobError::Refused` with an
    /// [`ApiError`]), 408 only when the writer replied `Expired`, 503 `outcome_unknown`
    /// when it had not replied by then, else the write mapping (§2.8, §2.9).
    pub async fn write<R: Send + 'static>(
        &self,
        job: impl FnOnce(&mut WriteTx<'_>, Option<&Authorized>) -> Result<R, JobError> + Send + 'static,
    ) -> Result<R, ApiError> {
        self.write_releasing(job, None).await
    }

    /// [`WriteCtx::write`] with `release`, which the observer drops as soon as
    /// `Store::submit` for the job returns, success or failure; a refused registration, or
    /// an observer aborted before its first poll, drops it too. Ownership passes to the
    /// observer at registration, so a client that disconnects afterwards no longer holds it
    /// (hook-ingest §2.3).
    ///
    /// # Errors
    ///
    /// As [`WriteCtx::write`].
    pub(crate) async fn write_then<R: Send + 'static>(
        &self,
        job: impl FnOnce(&mut WriteTx<'_>, Option<&Authorized>) -> Result<R, JobError> + Send + 'static,
        release: Release,
    ) -> Result<R, ApiError> {
        self.write_releasing(job, Some(release)).await
    }

    async fn write_releasing<R: Send + 'static>(
        &self,
        job: impl FnOnce(&mut WriteTx<'_>, Option<&Authorized>) -> Result<R, JobError> + Send + 'static,
        release: Option<Release>,
    ) -> Result<R, ApiError> {
        let sha = self.principal.as_ref().map(|p| p.token_sha256);
        await_write_releasing(
            &self.shared,
            self.deadline,
            Box::new(move |tx| {
                let auth = verify_opt_in_job(tx, sha)?;
                job(tx, auth.as_ref())
            }),
            None,
            release,
        )
        .await
    }

    /// One read bounded by `by` (on the server's clock) instead of the request deadline:
    /// the reader pool's wait ends at `by`, and a result not returned by it, or taken at or
    /// after it, is 408, reader-pool exhaustion included (a strict cutoff, unlike
    /// [`read_until`]'s 503 `busy` at the cutoff).
    ///
    /// # Errors
    ///
    /// 408, 503 `busy` (exhaustion before `by`) / `shutting_down`, 500, or `f`'s store error
    /// mapped (§2.9).
    pub(crate) async fn read_by<R: Send + 'static>(
        &self,
        by: Instant,
        f: impl FnOnce(&ReadTx<'_>) -> Result<R, StoreError> + Send + 'static,
    ) -> Result<R, ApiError> {
        read_cut(&self.shared, by, f, true).await
    }

    /// The store's commit notifications, from now on (closed once the store shuts down).
    pub(crate) fn commits(&self) -> tokio::sync::broadcast::Receiver<crate::store::Committed> {
        self.shared.store.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::{Joined, read_outcome};
    use crate::deadline::Settled;
    use crate::http::ApiError;
    use crate::store::{ReadError, StoreError};

    /// A failed join: a task that panicked.
    async fn failed_join() -> tokio::task::JoinError {
        tokio::spawn(async { panic!("the read panics") })
            .await
            .unwrap_err()
    }

    /// `read_cut`'s mapping (docs/design/deadlines.md §3 D4): a late result is 408, except
    /// reader-pool exhaustion on a non-strict read, which stays 503 `busy`.
    #[tokio::test]
    async fn read_outcome_table() {
        type R = Result<u8, ApiError>;
        let ok = |v: u8| -> Joined<u8> { Ok(Ok(v)) };
        let err = |e: ReadError| -> Joined<u8> { Ok(Err(e)) };
        for strict in [false, true] {
            assert_eq!(read_outcome(Settled::InTime(ok(7)), strict), R::Ok(7));
            assert_eq!(
                read_outcome(Settled::Late(ok(7)), strict),
                R::Err(ApiError::request_timeout())
            );
            assert_eq!(
                read_outcome(
                    Settled::Late(err(ReadError::Store(StoreError::DiskFull))),
                    strict
                ),
                R::Err(ApiError::request_timeout())
            );
            assert_eq!(
                read_outcome(Settled::InTime(err(ReadError::Busy)), strict),
                R::Err(ApiError::busy())
            );
            assert_eq!(
                read_outcome(Settled::Late(Err(failed_join().await)), strict),
                R::Err(ApiError::request_timeout())
            );
            assert_eq!(
                read_outcome(Settled::InTime(Err(failed_join().await)), strict),
                R::Err(ApiError::internal())
            );
            assert_eq!(
                read_outcome(Settled::<Joined<u8>>::Elapsed, strict),
                R::Err(ApiError::request_timeout())
            );
        }
        assert_eq!(
            read_outcome(Settled::Late(err(ReadError::Busy)), false),
            R::Err(ApiError::busy())
        );
        assert_eq!(
            read_outcome(Settled::Late(err(ReadError::Busy)), true),
            R::Err(ApiError::request_timeout())
        );
    }
}
