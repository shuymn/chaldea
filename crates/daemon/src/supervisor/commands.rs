//! The four launch commands (design §2.2): `spawn`, `resume`, `stop`, `close`. `prepare`
//! parses the wire args, admits the prompt, and mints ids and the hook token; the intent
//! checks the target scope before any other read of the thread; the effect hands its work to
//! a [`super::ops`] task that outlives the runner.

use std::sync::Arc;
use std::time::Duration;

use chaldea_core::auth::{Action, Grant, ThreadGraph, grant, may_act_on_thread};
use chaldea_core::canonical::CanonicalJson;
use chaldea_core::command::{CloseArgs, CloseResult, ResumeArgs, SpawnArgs, StopArgs};
use chaldea_core::event::TypedEvent;
use chaldea_core::http::ErrorCode;
use chaldea_core::ids::{Actor, Agent, CommandId, InputId, LaunchId, ThreadId};
use chaldea_core::input::MAX_PROMPT_BYTES;
use chaldea_core::input::encode::{Purpose, check};
use chaldea_core::payload::{InputEncoding, SourceKind, ThreadClosed};
use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::value::RawValue;
use uuid::Uuid;

use crate::http::{
    ApiError, Authorized, CommandEffect, CommandHandler, EffectOutcome, EffectRun, Intent,
    Prepared, deny_for, mint_token, refused,
};
use crate::store::{JobError, ThreadRow, TokenKind, TokenSelector, WriteTx};

use super::events::{ThreadJob, command_key, refusal};
use super::ops::{
    LaunchKind, LaunchReq, StopReq, already_exited, code, launch_op, ok, refused_result,
    stop_launch, stop_op,
};
use super::record::{JobOf, Recorded, record};
use super::{COMMAND_DEADLINE, STOP_REPORT, Supervisor};

fn parse<T: serde::de::DeserializeOwned + Serialize>(
    args: &RawValue,
) -> Result<(T, CanonicalJson), ApiError> {
    let a: T = serde_json::from_str(args.get()).map_err(|_| ApiError::bad_args())?;
    let n = CanonicalJson::of(&a).map_err(|_| ApiError::bad_args())?;
    Ok((a, n))
}

/// Prompt admission (design §2.2, C1): the size first, then `check` for Codex's positional
/// prompt. A Claude prompt is refused by the caller, which knows the agent.
fn admit_prompt(prompt: Option<&str>) -> Result<(), ApiError> {
    let Some(text) = prompt else {
        return Ok(());
    };
    if text.len() > MAX_PROMPT_BYTES {
        return Err(code(ErrorCode::PromptTooLarge));
    }
    check(Agent::Codex, InputEncoding::Positional, Purpose::Send, text)
        .map_err(|_| code(ErrorCode::PromptRejected))
}

fn caller(auth: &Authorized) -> Actor {
    match (auth.kind, auth.thread) {
        (TokenKind::Agent, Some(t)) => Actor::Thread(t),
        _ => Actor::Human,
    }
}

/// The target-scope check (design §2.2 "Target scope first"): before any other read of the
/// thread; a refusal is `deny_for`.
fn target(
    tx: &WriteTx<'_>,
    auth: &Authorized,
    action: Action,
    t: ThreadId,
) -> Result<ThreadRow, JobError> {
    let r = tx.read();
    let g = ThreadGraph::new(
        r.threads()?
            .into_iter()
            .map(|row| (row.thread_id, row.parent_thread_id)),
    );
    let allowed = g.contains(t)
        && grant(auth.kind, action).is_some_and(|gr| may_act_on_thread(&g, auth.thread, gr, t));
    if !allowed {
        return Err(refused(deny_for(&g, auth, t)));
    }
    r.thread(t)?.ok_or_else(|| refused(ApiError::not_found()))
}

fn run_op(
    sup: &Arc<Supervisor>,
    fut: impl std::future::Future<Output = EffectOutcome> + Send + 'static,
) -> BoxFuture<'static, EffectOutcome> {
    let h = sup.rt.spawn(fut);
    Box::pin(async move {
        h.await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "a launch op panicked");
            EffectOutcome::Unrecorded
        })
    })
}

// ---- spawn ------------------------------------------------------------------------------------

pub(crate) struct SpawnHandler(pub(crate) Arc<Supervisor>);

impl CommandHandler for SpawnHandler {
    fn name(&self) -> &'static str {
        "spawn"
    }

    fn deadline(&self) -> Option<Duration> {
        Some(COMMAND_DEADLINE)
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized) = parse::<SpawnArgs>(args)?;
        if a.prompt.is_some() && a.agent == Agent::Claude {
            return Err(ApiError::bad_args());
        }
        admit_prompt(a.prompt.as_deref())?;
        let thread = ThreadId(Uuid::new_v4());
        let launch = LaunchId(Uuid::new_v4());
        let input_id = a.prompt.as_ref().map(|_| InputId(Uuid::new_v4()));
        let token = mint_token();
        Ok(Prepared {
            normalized,
            intent: Box::new(move |_tx, auth, ctx| {
                let parent = match grant(auth.kind, Action::Spawn) {
                    Some(Grant::All) => None,
                    Some(Grant::ChildrenOfSelf) => {
                        Some(auth.thread.ok_or_else(|| refused(ApiError::forbidden()))?)
                    }
                    _ => return Err(refused(ApiError::forbidden())),
                };
                Ok(Intent::Pending(EffectRun::new(LaunchReq {
                    command_id: ctx.command_id(),
                    thread,
                    launch,
                    kind: LaunchKind::Spawn {
                        agent: a.agent,
                        cwd: a.cwd,
                        task: a.task,
                        parent,
                    },
                    prompt: a.prompt,
                    input_id,
                    hook_token: token,
                    caller: caller(auth),
                })))
            }),
        })
    }

    fn effect(&self) -> Option<&dyn CommandEffect> {
        Some(self)
    }
}

impl CommandEffect for SpawnHandler {
    fn run(&self, run: EffectRun) -> BoxFuture<'static, EffectOutcome> {
        launch_effect(&self.0, run)
    }
}

fn launch_effect(sup: &Arc<Supervisor>, run: EffectRun) -> BoxFuture<'static, EffectOutcome> {
    match run.downcast::<LaunchReq>() {
        Ok(req) => run_op(sup, launch_op(sup.clone(), req)),
        Err(_) => Box::pin(async { EffectOutcome::Unrecorded }),
    }
}

// ---- resume -----------------------------------------------------------------------------------

pub(crate) struct ResumeHandler(pub(crate) Arc<Supervisor>);

impl CommandHandler for ResumeHandler {
    fn name(&self) -> &'static str {
        "resume"
    }

    fn deadline(&self) -> Option<Duration> {
        Some(COMMAND_DEADLINE)
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized) = parse::<ResumeArgs>(args)?;
        admit_prompt(a.prompt.as_deref())?;
        let launch = LaunchId(Uuid::new_v4());
        let input_id = a.prompt.as_ref().map(|_| InputId(Uuid::new_v4()));
        let token = mint_token();
        Ok(Prepared {
            normalized,
            intent: Box::new(move |tx, auth, ctx| {
                let t = target(tx, auth, Action::Resume, a.thread_id)?;
                if a.prompt.is_some() && t.agent == Agent::Claude {
                    return Err(refused(ApiError::bad_args()));
                }
                Ok(Intent::Pending(EffectRun::new(LaunchReq {
                    command_id: ctx.command_id(),
                    thread: a.thread_id,
                    launch,
                    kind: LaunchKind::Resume {
                        expected: a.expected_launch_id,
                    },
                    prompt: a.prompt,
                    input_id,
                    hook_token: token,
                    caller: caller(auth),
                })))
            }),
        })
    }

    fn effect(&self) -> Option<&dyn CommandEffect> {
        Some(self)
    }
}

impl CommandEffect for ResumeHandler {
    fn run(&self, run: EffectRun) -> BoxFuture<'static, EffectOutcome> {
        launch_effect(&self.0, run)
    }
}

// ---- stop -------------------------------------------------------------------------------------

pub(crate) struct StopHandler(pub(crate) Arc<Supervisor>);

impl CommandHandler for StopHandler {
    fn name(&self) -> &'static str {
        "stop"
    }

    fn deadline(&self) -> Option<Duration> {
        Some(COMMAND_DEADLINE)
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized) = parse::<StopArgs>(args)?;
        Ok(Prepared {
            normalized,
            intent: Box::new(move |tx, auth, ctx| {
                let t = target(tx, auth, Action::Stop, a.thread_id)?;
                let Some(l) = t.current_launch_id else {
                    return Err(refused(ApiError::not_found()));
                };
                if a.expected_launch_id.is_some_and(|e| e != l) {
                    return Err(refused(code(ErrorCode::LaunchChanged)));
                }
                let row = tx
                    .read()
                    .launch(l)?
                    .ok_or_else(|| refusal("the current launch has no row"))?;
                if row.exit_reason.is_some() {
                    if !row.death_confirmed {
                        return Err(refused(code(ErrorCode::DeathUnconfirmed)));
                    }
                    let within = ctx.job_start() <= ctx.request_start() + STOP_REPORT;
                    return Ok(Intent::Completed(ok(&already_exited(l, within))));
                }
                Ok(Intent::Pending(EffectRun::new(StopReq {
                    command_id: ctx.command_id(),
                    thread: a.thread_id,
                    launch: l,
                    request_start: ctx.request_start(),
                })))
            }),
        })
    }

    fn effect(&self) -> Option<&dyn CommandEffect> {
        Some(self)
    }
}

impl CommandEffect for StopHandler {
    fn run(&self, run: EffectRun) -> BoxFuture<'static, EffectOutcome> {
        match run.downcast::<StopReq>() {
            Ok(req) => run_op(&self.0, stop_op(self.0.clone(), req)),
            Err(_) => Box::pin(async { EffectOutcome::Unrecorded }),
        }
    }
}

// ---- close ------------------------------------------------------------------------------------

pub(crate) struct CloseHandler(pub(crate) Arc<Supervisor>);

/// The close op's data: the stop of the expected launch, then the closing job.
struct CloseReq {
    stop: StopReq,
    by: Actor,
}

/// The closing job's checks and writes, shared by the intent and the op. `Ok(Err(code))`
/// is a refusal that wrote nothing.
fn closing_in(
    tx: &mut WriteTx<'_>,
    thread: ThreadId,
    expected: LaunchId,
    cid: CommandId,
    by: Actor,
) -> Result<Result<(), ErrorCode>, JobError> {
    let r = tx.read();
    let Some(t) = r.thread(thread)? else {
        return Ok(Err(ErrorCode::NotFound));
    };
    if t.closed {
        return Ok(Err(ErrorCode::ThreadClosed));
    }
    if t.current_launch_id != Some(expected) {
        return Ok(Err(ErrorCode::LaunchChanged));
    }
    let Some(l) = r.launch(expected)? else {
        return Ok(Err(ErrorCode::LaunchChanged));
    };
    if l.exit_reason.is_none() {
        return Ok(Err(ErrorCode::LaunchLive));
    }
    if !l.death_confirmed {
        return Ok(Err(ErrorCode::DeathUnconfirmed));
    }
    let mut job = ThreadJob::new(thread, t.agent, t.state, SourceKind::Command, Some(cid));
    job.push_host(
        command_key(cid, 0),
        TypedEvent::ThreadClosed(ThreadClosed {
            thread_id: thread,
            by,
        }),
    );
    job.flush(tx)?;
    tx.token_revoke(TokenSelector::Thread(thread))?;
    Ok(Ok(()))
}

impl CommandHandler for CloseHandler {
    fn name(&self) -> &'static str {
        "close"
    }

    fn deadline(&self) -> Option<Duration> {
        Some(COMMAND_DEADLINE)
    }

    fn revokes(&self) -> bool {
        true
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized) = parse::<CloseArgs>(args)?;
        Ok(Prepared {
            normalized,
            intent: Box::new(move |tx, auth, ctx| {
                let t = target(tx, auth, Action::Close, a.thread_id)?;
                if t.closed {
                    return Err(refused(code(ErrorCode::ThreadClosed)));
                }
                if t.current_launch_id != Some(a.expected_launch_id) {
                    return Err(refused(code(ErrorCode::LaunchChanged)));
                }
                let live = tx
                    .read()
                    .launch(a.expected_launch_id)?
                    .is_some_and(|l| l.exit_reason.is_none());
                if a.stop && live {
                    return Ok(Intent::Pending(EffectRun::new(CloseReq {
                        stop: StopReq {
                            command_id: ctx.command_id(),
                            thread: a.thread_id,
                            launch: a.expected_launch_id,
                            request_start: ctx.request_start(),
                        },
                        by: caller(auth),
                    })));
                }
                match closing_in(
                    tx,
                    a.thread_id,
                    a.expected_launch_id,
                    ctx.command_id(),
                    caller(auth),
                )? {
                    Ok(()) => Ok(Intent::Completed(ok(&CloseResult {
                        thread_id: a.thread_id,
                        stop: None,
                    }))),
                    Err(c) => Err(refused(code(c))),
                }
            }),
        })
    }

    fn effect(&self) -> Option<&dyn CommandEffect> {
        Some(self)
    }
}

impl CommandEffect for CloseHandler {
    fn run(&self, run: EffectRun) -> BoxFuture<'static, EffectOutcome> {
        match run.downcast::<CloseReq>() {
            Ok(req) => run_op(&self.0, close_op(self.0.clone(), req)),
            Err(_) => Box::pin(async { EffectOutcome::Unrecorded }),
        }
    }
}

/// `close --stop` of a live launch: Stop for it, reported against the close's
/// `RequestStart`, then the closing job with the same checks and the nudge.
async fn close_op(sup: Arc<Supervisor>, req: CloseReq) -> EffectOutcome {
    let stop = match stop_launch(&sup, req.stop).await {
        Ok(s) => s,
        Err(o) => return o,
    };
    let (thread, launch, cid, by) = (
        req.stop.thread,
        req.stop.launch,
        req.stop.command_id,
        req.by,
    );
    let job: JobOf<Result<(), ErrorCode>> =
        Arc::new(move |tx| closing_in(tx, thread, launch, cid, by));
    match record(&sup, job, true).await {
        Recorded::Done(Ok(())) => EffectOutcome::Done(ok(&CloseResult {
            thread_id: thread,
            stop: Some(stop),
        })),
        Recorded::Done(Err(c)) => EffectOutcome::Done(refused_result(&code(c))),
        Recorded::Refused(e) => {
            tracing::error!(error = ?e, "the closing job refused");
            EffectOutcome::Unrecorded
        }
        Recorded::Unrecorded => EffectOutcome::Unrecorded,
        Recorded::ShuttingDown => EffectOutcome::ShuttingDown,
    }
}
