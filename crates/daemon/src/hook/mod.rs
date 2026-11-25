//! Hook ingest (`docs/design/hook-ingest.md`, cited as "design §N"): `POST /hook`, binding,
//! buffering, promotion, and `raw_unknown`.
//!
//! The rules (design §2.1):
//!
//! 1. **One job decides.** Deduplication, routing, binding, buffering, promotion, and buffer
//!    bounds are decided inside the delivery's store job, with every target derived from
//!    the token as re-verified there. Everything before the job gathers inputs and writes
//!    nothing.
//! 2. **Memory holds only intake order** ([`IntakeOrder`]).
//! 3. **Binding never overtakes an earlier hook** of the same launch.
//! 4. **Hook meaning is the adapter's** ([`HookAdapter`]).
//! 5. **Only the current, live, bound launch and its session change state.**
//! 6. **Every wait has a deadline.**

mod adapter;
mod ancestry;
mod buffer;
mod intake;
pub mod payload;
mod request;
mod route;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chaldea_core::ids::{LaunchId, StreamId, ThreadId};
use tokio::sync::Semaphore;
use tokio::sync::broadcast::error::RecvError;

pub use adapter::{
    AcceptedHook, BindingRule, HookAdapter, HookEvent, Interpretation, MvpHooks, OsProcesses,
    ProcessProbe, TurnMark,
};
pub use ancestry::{Chain, Link, eligible};
pub use buffer::{expire, resolve_unbindable};
pub use intake::{IntakeFacts, IntakeGuard, IntakeOrder, IntakeProbe, IntakeTicket, NoIntakeFacts};

use crate::http::{ApiError, Mount, WriteCtx, spec, until_deadline, verify_in_read};
use crate::store::{HookDelivery, HookReply, JobError, LaunchRow, ReadTx, StoreError};

/// The hook body cap (mvp §2.5); the guard answers 413 above it.
pub const MAX_HOOK_BODY: u64 = 4 * 1024 * 1024;
/// One body permit stands for this many bytes.
const PERMIT_BYTES: u64 = 1024;

/// Hook ingest's settings. [`HookConfig::default`] gives the design's values.
#[derive(Debug, Clone)]
pub struct HookConfig {
    /// `links[0]` must run the emitter binary: `chaldea`.
    pub emitter_name: String,
    /// Early hooks wait this long, inside the deadline, for `launch_started` (1 s).
    pub pid_wait: Duration,
    /// Links walked from the emitter (16).
    pub max_chain: usize,
    /// Buffered deliveries per launch (32).
    pub buffer_max_count: usize,
    /// Buffered body bytes per launch (8 MiB).
    pub buffer_max_bytes: u64,
    /// From the earliest `received_at` of a launch's buffered deliveries (30 s).
    pub buffer_ttl: Duration,
    /// Hook bodies held from their read to the end of their job (32 MiB).
    pub body_budget: u64,
}

impl Default for HookConfig {
    fn default() -> Self {
        HookConfig {
            emitter_name: "chaldea".to_owned(),
            pid_wait: Duration::from_secs(1),
            max_chain: 16,
            buffer_max_count: 32,
            buffer_max_bytes: 8 * 1024 * 1024,
            buffer_ttl: Duration::from_secs(30),
            body_budget: 32 * 1024 * 1024,
        }
    }
}

/// The seams PR 10, 13, and 14 fill.
#[derive(Clone)]
pub struct HookSeams {
    pub adapter: Arc<dyn HookAdapter>,
    pub intake: Arc<dyn IntakeProbe>,
    pub procs: Arc<dyn ProcessProbe>,
}

impl HookSeams {
    /// The defaults until PR 10 / 13 / 14: [`MvpHooks`], no intake facts, and the OS walk.
    #[must_use]
    pub fn mvp() -> HookSeams {
        HookSeams {
            adapter: Arc::new(MvpHooks),
            intake: Arc::new(NoIntakeFacts),
            procs: Arc::new(OsProcesses),
        }
    }
}

/// `POST /hook`'s handler state: settings, seams, the intake order, and the body budget.
pub struct HookIngest {
    cfg: HookConfig,
    seams: HookSeams,
    order: Arc<IntakeOrder>,
    budget: Arc<Semaphore>,
    #[cfg(test)]
    pub(crate) points: tests::points::Points,
}

/// The `/hook` mount with a new [`HookIngest`].
#[must_use]
pub fn mount(cfg: HookConfig, seams: HookSeams) -> Mount {
    HookIngest::new(cfg, seams).mount()
}

/// The job's refusal for a broken invariant or an adapter bug: 500, rolled back.
fn refused_internal() -> JobError {
    crate::http::refused(ApiError::internal())
}

/// What the pre-read found (design §2.3 step 4).
struct PreRead {
    thread: ThreadId,
    launch: LaunchRow,
    delivery: Option<HookDelivery>,
    stored_max: u64,
}

fn reply(r: &HookReply) -> Response {
    let status = StatusCode::from_u16(r.http_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, r.body.clone()).into_response()
}

impl HookIngest {
    /// # Panics
    ///
    /// When `cfg.body_budget` is below one permit's 1 KiB or above `u32::MAX` KiB.
    #[must_use]
    pub fn new(cfg: HookConfig, seams: HookSeams) -> Arc<HookIngest> {
        let permits = usize::try_from(cfg.body_budget / PERMIT_BYTES)
            .ok()
            .filter(|p| *p >= 1 && u32::try_from(*p).is_ok())
            .expect("the body budget is 1 KiB to u32::MAX KiB");
        Arc::new(HookIngest {
            cfg,
            seams,
            order: Arc::new(IntakeOrder::default()),
            budget: Arc::new(Semaphore::new(permits)),
            #[cfg(test)]
            points: tests::points::Points::default(),
        })
    }

    /// The `/hook` route's write mount over this ingest.
    ///
    /// # Panics
    ///
    /// Never: `POST /hook` is in `ROUTES`.
    #[must_use]
    pub fn mount(self: &Arc<Self>) -> Mount {
        let me = self.clone();
        Mount::write(
            spec(&Method::POST, "/hook").expect("POST /hook is in ROUTES"),
            move |ctx: WriteCtx, req: Request<Body>| {
                let me = me.clone();
                async move { me.handle(ctx, req).await }
            },
        )
    }

    /// Body permits free now (for tests of the budget).
    #[must_use]
    pub fn free_permits(&self) -> usize {
        self.budget.available_permits()
    }

    /// Design §2.3.
    async fn handle(
        self: Arc<Self>,
        ctx: WriteCtx,
        req: Request<Body>,
    ) -> Result<Response, ApiError> {
        let shared = ctx.shared().clone();
        let deadline = ctx.deadline();
        let (parts, body) = req.into_parts();
        // 1. Headers.
        let headers = request::parse(&parts.headers)?;
        // 2. Body permits, before any body byte is read.
        let len = parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(ApiError::internal)?;
        let permits = u32::try_from(len.div_ceil(PERMIT_BYTES)).map_err(|_| ApiError::busy())?;
        let permit = self
            .budget
            .clone()
            .try_acquire_many_owned(permits)
            .map_err(|_| ApiError::busy())?;
        // 3. Body and the discriminating fields.
        let body = ctx.body_within(body, MAX_HOOK_BODY).await?;
        let head = route::Head::from(payload::decode(&body));
        // 4. Pre-read.
        let sha = ctx
            .principal()
            .map(|p| p.token_sha256)
            .ok_or_else(ApiError::internal)?;
        let emit = headers.emit_id;
        #[cfg(test)]
        self.points.before_pre_read().await;
        let pre = ctx
            .read_by(deadline, move |tx| pre_read(tx, &sha, emit))
            .await??;
        if let Some(d) = &pre.delivery {
            return Ok(reply(&d.first_reply));
        }
        // 5. Intake: the ordinal, the facts, and the pid wait's origin together.
        let (ticket, facts, guard, intake_at) = self.order.intake(
            pre.launch.launch_id,
            pre.stored_max,
            &self.seams.intake,
            || shared.cfg.clock.now(),
        );
        #[cfg(test)]
        self.points.after_intake(ticket.ordinal()).await;
        // 6. Ancestry walk.
        let chain = match headers.emitter_pid {
            None => Chain::no_pid(),
            Some(_) if shared.passed(deadline) => return Err(ApiError::request_timeout()),
            Some(pid) => {
                let procs = self.seams.procs.clone();
                let max = self.cfg.max_chain;
                #[cfg(test)]
                self.points.walk_submitted();
                let walk = tokio::task::spawn_blocking(move || procs.chain(pid, max));
                match until_deadline(&shared, deadline, || walk).await? {
                    Ok(a) => Chain::from_ancestry(a),
                    Err(_) => return Err(ApiError::internal()),
                }
            }
        };
        // 7. Pid wait.
        let pid_wait_expired = if pre.launch.pid.is_none() && pre.launch.exit_reason.is_none() {
            self.pid_wait(&ctx, intake_at, pre.launch.launch_id, pre.thread)
                .await?
        } else {
            false
        };
        // 8. Binding order.
        let may_bind = !headers.truncated
            && matches!(&head, route::Head::Hook(h) if h.is_session_start())
            && pre.launch.session_id.is_none();
        if may_bind {
            let order = self.order.clone();
            until_deadline(&shared, deadline, || async {
                order.earlier_released(&ticket).await;
            })
            .await?;
        }
        #[cfg(test)]
        self.points.before_job(ticket.ordinal()).await;
        // 9. The delivery job.
        let delivery = route::Delivery {
            headers,
            body,
            _permit: permit,
            head,
            chain,
            facts,
            ordinal: ticket.ordinal(),
            pid_wait_expired,
            adapter: self.seams.adapter.clone(),
            cfg: self.cfg.clone(),
        };
        let r = ctx
            .write_then(
                move |tx, auth| route::run(&delivery, tx, auth),
                Box::new(ticket),
                Some(Box::new(guard)),
            )
            .await?;
        // 10. Reply.
        Ok(reply(&r.reply))
    }

    /// Design §2.3 "Pid wait": `Ok(true)` when the 1 s side ran out (the job records
    /// `unbound`), `Ok(false)` when it ended without a decision, 408 when the request
    /// deadline came first.
    async fn pid_wait(
        &self,
        ctx: &WriteCtx,
        intake_at: Instant,
        launch: LaunchId,
        thread: ThreadId,
    ) -> Result<bool, ApiError> {
        let shared = ctx.shared();
        let deadline = ctx.deadline();
        let pid_deadline = (intake_at + self.cfg.pid_wait).min(deadline);
        let request_side = pid_deadline >= deadline;
        #[cfg(test)]
        shared.faults.pid_wait_started(pid_deadline);
        let ran_out = || {
            if request_side {
                Err(ApiError::request_timeout())
            } else {
                Ok(true)
            }
        };
        // Subscribe first, then re-read, so a start committed in between is seen.
        let mut commits = ctx.commits();
        #[cfg(test)]
        self.points.after_subscribe().await;
        let timer = pid_timer(shared.timer(pid_deadline));
        tokio::pin!(timer);
        let stream = StreamId::Thread(thread);
        loop {
            match ctx
                .read_by(pid_deadline, move |tx| started_or_gone(tx, launch, thread))
                .await
            {
                Ok(true) => return Ok(false),
                Ok(false) => {}
                // Cut off at the pid-wait deadline, or still waiting for a reader then: it
                // saw no start.
                Err(e)
                    if e.status() == StatusCode::REQUEST_TIMEOUT
                        || e.status() == StatusCode::SERVICE_UNAVAILABLE
                            && e.code() == chaldea_core::http::ErrorCode::Busy =>
                {
                    return ran_out();
                }
                Err(e) => return Err(e),
            }
            // The next notification naming the thread's stream, a lag, or the bound; the
            // bound wins when both are ready.
            loop {
                tokio::select! {
                    biased;
                    () = &mut timer => return ran_out(),
                    r = commits.recv() => match r {
                        Ok(c) if c.heads.iter().any(|h| h.stream_id == stream) => break,
                        Ok(_) => {}
                        Err(RecvError::Lagged(_)) => break,
                        Err(RecvError::Closed) => return Ok(false),
                    },
                }
            }
            if shared.passed(pid_deadline) {
                return ran_out();
            }
        }
    }
}

impl crate::http::OnReply<route::Ran> for IntakeGuard {
    fn submitted(&mut self) {
        IntakeGuard::submitted(self);
    }

    fn replied(self: Box<Self>, r: &Result<route::Ran, crate::http::WriteFailure>) {
        use crate::store::WriteError;
        match r {
            Ok(ran) => (*self).replied(Some(&ran.marks), false),
            Err(crate::http::WriteFailure::Write(
                WriteError::CommitUnknown(_) | WriteError::WriterGone,
            )) => (*self).replied(None, true),
            Err(_) => (*self).replied(None, false),
        }
    }
}

/// The pid wait's timer: biased first in its `select!`; a wake is followed by one clock
/// sample and a strict re-read (hook-ingest §2.3 "Pid wait").
#[allow(
    clippy::disallowed_methods,
    reason = "the pid wait's biased timer; a wake is followed by one sample and a strict re-read (hook-ingest §2.3)"
)]
fn pid_timer(at: tokio::time::Instant) -> tokio::time::Sleep {
    tokio::time::sleep_until(at)
}

/// The pre-read's one snapshot: the token (with the hook checks), the launch and thread,
/// an existing delivery, and the stored maximum ordinal.
fn pre_read(
    tx: &ReadTx<'_>,
    sha: &[u8; 32],
    emit: uuid::Uuid,
) -> Result<Result<PreRead, ApiError>, StoreError> {
    let auth = match verify_in_read(tx, sha)? {
        Ok(a) => a,
        Err(e) => return Ok(Err(e)),
    };
    let (Some(thread), Some(launch)) = (auth.thread, auth.launch) else {
        return Ok(Err(ApiError::unauthorized()));
    };
    let Some(l) = tx.launch(launch)? else {
        return Ok(Err(ApiError::unauthorized()));
    };
    Ok(Ok(PreRead {
        thread,
        delivery: tx.hook_delivery(launch, emit)?,
        stored_max: tx.max_receipt_ordinal(launch)?,
        launch: l,
    }))
}

/// Whether the launch has a recorded pid, an exit, or is no longer the thread's current
/// one: the pid wait ends without a decision.
fn started_or_gone(
    tx: &ReadTx<'_>,
    launch: LaunchId,
    thread: ThreadId,
) -> Result<bool, StoreError> {
    let current = tx.thread(thread)?.and_then(|t| t.current_launch_id);
    if current != Some(launch) {
        return Ok(true);
    }
    Ok(tx
        .launch(launch)?
        .is_none_or(|l| l.pid.is_some() || l.exit_reason.is_some()))
}
