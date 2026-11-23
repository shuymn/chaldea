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
use tokio::time::timeout_at;

use super::auth::{Authorized, Principal, verify_in_read, verify_opt_in_job};
use super::error::{ApiError, WriteFailure, read_error, write_failure};
use super::registry::{Closing, TaskKind};
use super::{Shared, StoreFatal};
use crate::store::{JobError, ReadError, ReadTx, StoreError, SubmitError, WriteError, WriteTx};

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
    // The cutoff is set before the read starts.
    let cutoff = shared.timer(deadline);
    let handle = spawn_read(shared, deadline, f);
    #[cfg(test)]
    shared
        .faults
        .before_cutoff_wait(|| handle.is_finished())
        .await;
    match timeout_at(cutoff, handle).await {
        Err(_) => Err(ApiError::request_timeout()),
        Ok(joined) => take_read(shared, deadline, joined),
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
/// clock) is 408, except reader-pool exhaustion, whose wait ends at the cutoff itself (503
/// `busy`); otherwise the store outcome maps through §2.9.
pub(crate) fn take_read<R>(
    shared: &Shared,
    deadline: Instant,
    joined: Joined<R>,
) -> Result<R, ApiError> {
    match joined {
        Err(_) => Err(ApiError::internal()),
        Ok(Err(ReadError::Busy)) => Err(ApiError::busy()),
        Ok(_) if shared.passed(deadline) => Err(ApiError::request_timeout()),
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
    match timeout_at(cutoff, start()).await {
        Ok(v) if !shared.passed(deadline) => Ok(v),
        _ => Err(ApiError::request_timeout()),
    }
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
        Err(e) => {
            let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e.as_ref());
            while let Some(err) = cur {
                if err.is::<LengthLimitError>() {
                    return Err(ApiError::payload_too_large());
                }
                cur = err.source();
            }
            Err(ApiError::bad_request())
        }
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
    #[cfg(test)]
    shared.faults.write_submitted();
    let ticket = match shared.store.submit(shared.real(not_after), job) {
        Ok(t) => t,
        Err(e) => {
            if e == SubmitError::Failed {
                shared.sink.store_fatal(StoreFatal::Failed);
            }
            return Err(WriteFailure::Submit(e));
        }
    };
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
                shared.sink.store_fatal(f);
            }
            Err(WriteFailure::Write(e))
        }
    }
}

/// For plain writes: registers one observer task running [`run_write`] and returns its
/// result on a oneshot. Refused once the registry is closing.
pub(crate) fn observe_write<R: Send + 'static>(
    shared: &Arc<Shared>,
    not_after: Instant,
    job: Job<R>,
    after_ok: Option<AfterOk>,
) -> Result<oneshot::Receiver<Result<R, WriteFailure>>, Closing> {
    let (tx, rx) = oneshot::channel();
    let s = shared.clone();
    shared.registry.spawn(TaskKind::Write, async move {
        let r = run_write(&s, not_after, job, after_ok).await;
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
    if shared.passed(deadline) {
        return Err(ApiError::request_timeout());
    }
    // The reply cutoff is set before the job is submitted.
    let reply_by = deadline + shared.cfg.write_reply_grace;
    let cutoff = shared.timer(reply_by);
    let rx =
        observe_write(shared, deadline, job, after_ok).map_err(|_| ApiError::shutting_down())?;
    #[cfg(test)]
    shared.faults.before_cutoff_wait(|| !rx.is_empty()).await;
    match timeout_at(cutoff, rx).await {
        Err(_) | Ok(Err(_)) => Err(ApiError::outcome_unknown()),
        Ok(Ok(_)) if shared.passed(reply_by) => Err(ApiError::outcome_unknown()),
        Ok(Ok(r)) => r.map_err(write_failure),
    }
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
        let sha = self.principal.as_ref().map(|p| p.token_sha256);
        await_write(
            &self.shared,
            self.deadline,
            Box::new(move |tx| {
                let auth = verify_opt_in_job(tx, sha)?;
                job(tx, auth.as_ref())
            }),
            None,
        )
        .await
    }
}
