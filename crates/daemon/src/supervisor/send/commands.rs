//! The five send-side commands (send §2.2): `send`, `ready`, `key`, `input_resolve`,
//! `input_cancel`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chaldea_core::adapter::capabilities;
use chaldea_core::auth::{Action, ThreadGraph, grant, may_act_on_thread};
use chaldea_core::command::{
    InputCancelArgs, InputCommandResult, InputResolveArgs, KeyArgs, KeyOutcome, KeyResult,
    ReadyArgs, ReadyResult, SendArgs, SendResult,
};
use chaldea_core::event::TypedEvent;
use chaldea_core::http::ErrorCode;
use chaldea_core::ids::{
    Actor, ApprovalId, CommandId, IdempotencyKey, InputId, LaunchId, ThreadId,
};
use chaldea_core::input::encode::{Purpose, check};
use chaldea_core::input::key::{KeyToken, MAX_KEYS, encode_keys};
use chaldea_core::input::{InputInput, InputReason, InputState, MAX_PROMPT_BYTES, step};
use chaldea_core::payload::{
    InputCreated, InputInitialState, InputOrigin, InputStateChanged, ReadinessEvidence, SourceKind,
};
use chaldea_core::state::{AttentionCause, ProcessState, StateInput};
use chaldea_pty_runtime::{Basis, ClassSet, Fence, Guard, OriginClass, WriteOrigin, WriteUnit};
use futures_util::future::BoxFuture;
use serde_json::value::RawValue;
use uuid::Uuid;

use super::super::commands::{caller, parse, run_op, target};
use super::super::events::{ThreadJob, command_key, refusal};
use super::super::ops::{code, ok, refused_result};
use super::super::record::{JobOf, Recorded, record};
use super::super::{COMMAND_DEADLINE, Supervisor};
use super::driver::readiness_in;
use super::{KEY_WAIT, KeyReport, READ_WAIT, fence_quiet};
use crate::deadline::settle_before;
use crate::http::{
    ApiError, Authorized, CommandEffect, CommandHandler, CommandRegistry, EffectOutcome, EffectRun,
    Intent, Prepared, deny_for, refused,
};
use crate::store::{JobError, ThreadRow, TokenKind, WriteTx, prepare_content};

/// Registers the five handlers.
pub(crate) fn register(commands: &mut CommandRegistry, sup: &Arc<Supervisor>) {
    commands.register(Box::new(SendHandler(sup.clone())));
    commands.register(Box::new(ReadyHandler(sup.clone())));
    commands.register(Box::new(KeyHandler(sup.clone())));
    commands.register(Box::new(InputResolveHandler));
    commands.register(Box::new(InputCancelHandler));
}

fn refuse(c: ErrorCode) -> JobError {
    refused(code(c))
}

fn done(c: ErrorCode) -> EffectOutcome {
    EffectOutcome::Done(refused_result(&code(c)))
}

/// An input's move as a command event, with `by` the actor.
fn input_moved(
    job: &mut ThreadJob,
    key: IdempotencyKey,
    input: InputId,
    from: InputState,
    by_input: &InputInput,
    by: Actor,
    launch: Option<LaunchId>,
) -> Result<InputState, JobError> {
    let to = step(from, by_input).map_err(refusal)?;
    let gate_key = key.clone();
    job.push(
        key,
        TypedEvent::InputStateChanged(InputStateChanged {
            input_id: input,
            from,
            to,
            reason: InputReason::from(by_input),
            sent_after_seq: None,
            by: Some(by),
        }),
        launch,
    );
    if from == InputState::Queued {
        job.left_queued(&gate_key, input, launch)?;
    }
    Ok(to)
}

// ---- send ---------------------------------------------------------------------------------------

struct SendHandler(Arc<Supervisor>);

impl CommandHandler for SendHandler {
    fn name(&self) -> &'static str {
        "send"
    }

    fn deadline(&self) -> Option<Duration> {
        Some(COMMAND_DEADLINE)
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized) = parse::<SendArgs>(args)?;
        if a.text.len() > MAX_PROMPT_BYTES {
            return Err(code(ErrorCode::PromptTooLarge));
        }
        let prepared = prepare_content(a.text.clone().into_bytes(), "text/plain; charset=utf-8")
            .map_err(|_| ApiError::bad_args())?;
        let input_id = InputId(Uuid::new_v4());
        let host_os = self.0.host_os;
        Ok(Prepared {
            normalized,
            intent: Box::new(move |tx, auth, ctx| {
                let t = target(tx, auth, Action::Send, a.thread_id)?;
                if t.closed {
                    return Err(refuse(ErrorCode::ThreadClosed));
                }
                let live = t.current_launch_id.and_then(|l| {
                    matches!(
                        t.state.process,
                        ProcessState::Starting | ProcessState::Running
                    )
                    .then_some(l)
                });
                let Some(launch) = live else {
                    return Err(refuse(ErrorCode::NoLiveLaunch));
                };
                if tx
                    .read()
                    .launch(launch)?
                    .is_none_or(|l| l.exit_reason.is_some())
                {
                    return Err(refuse(ErrorCode::NoLiveLaunch));
                }
                let encoding = capabilities(t.agent, host_os).pty_encoding();
                let by = caller(auth);
                let cid = ctx.command_id();
                let mut job = ThreadJob::new(
                    t.thread_id,
                    t.agent,
                    t.state.clone(),
                    SourceKind::Command,
                    Some(cid),
                );
                job.push_blobs(
                    command_key(cid, 0),
                    TypedEvent::InputCreated(InputCreated {
                        input_id,
                        launch_id: Some(launch),
                        initial_state: InputInitialState::Queued,
                        text: prepared.content.clone(),
                        encoding,
                        origin: a.origin.unwrap_or(InputOrigin::Cli),
                        created_by: by,
                    }),
                    Some(launch),
                    prepared.blob.clone().into_iter().collect(),
                );
                let reject = check(t.agent, encoding, Purpose::Send, &a.text).err();
                let state = match reject {
                    Some(r) => input_moved(
                        &mut job,
                        command_key(cid, 1),
                        input_id,
                        InputState::Queued,
                        &InputInput::Rejected(r.reason),
                        by,
                        Some(launch),
                    )?,
                    None => InputState::Queued,
                };
                job.flush(tx)?;
                Ok(Intent::Completed(ok(&SendResult {
                    input_id,
                    state,
                    reject_reason: reject.map(|r| r.reason),
                })))
            }),
        })
    }
}

// ---- ready --------------------------------------------------------------------------------------

struct ReadyHandler(Arc<Supervisor>);

struct ReadyReq {
    thread: ThreadId,
    launch: LaunchId,
    command_id: CommandId,
}

impl CommandHandler for ReadyHandler {
    fn name(&self) -> &'static str {
        "ready"
    }

    fn deadline(&self) -> Option<Duration> {
        Some(COMMAND_DEADLINE)
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized) = parse::<ReadyArgs>(args)?;
        Ok(Prepared {
            normalized,
            intent: Box::new(move |tx, auth, ctx| {
                let t = target(tx, auth, Action::Ready, a.thread_id)?;
                let launch = ready_checks(tx, &t, a.expected_launch_id).map_err(refuse)?;
                Ok(Intent::Pending(EffectRun::new(ReadyReq {
                    thread: a.thread_id,
                    launch,
                    command_id: ctx.command_id(),
                })))
            }),
        })
    }

    fn effect(&self) -> Option<&dyn CommandEffect> {
        Some(self)
    }
}

/// `ready`'s target checks on a fresh read: open thread, the expected launch current,
/// `running`, bound.
fn ready_checks(
    tx: &WriteTx<'_>,
    t: &ThreadRow,
    expected: Option<LaunchId>,
) -> Result<LaunchId, ErrorCode> {
    if t.closed {
        return Err(ErrorCode::ThreadClosed);
    }
    let Some(l) = t.current_launch_id else {
        return Err(ErrorCode::NoLiveLaunch);
    };
    if expected.is_some_and(|e| e != l) {
        return Err(ErrorCode::LaunchChanged);
    }
    let row = tx.read().launch(l).map_err(|_| ErrorCode::Internal)?;
    let Some(row) = row else {
        return Err(ErrorCode::NoLiveLaunch);
    };
    if row.exit_reason.is_some() || t.state.process != ProcessState::Running {
        return Err(ErrorCode::NoLiveLaunch);
    }
    if row.session_id.is_none() {
        return Err(ErrorCode::NotBound);
    }
    Ok(l)
}

impl CommandEffect for ReadyHandler {
    fn run(&self, run: EffectRun) -> BoxFuture<'static, EffectOutcome> {
        match run.downcast::<ReadyReq>() {
            Ok(req) => run_op(&self.0, ready_op(self.0.clone(), req)),
            Err(_) => Box::pin(async { EffectOutcome::Unrecorded }),
        }
    }
}

/// `ready`'s effect (send §2.4 "Owner assertion").
async fn ready_op(sup: Arc<Supervisor>, req: ReadyReq) -> EffectOutcome {
    let Some(sl) = sup.send.launch(req.launch) else {
        return done(ErrorCode::NoLiveLaunch);
    };
    if sl.running_flag.load(Ordering::SeqCst) {
        return done(ErrorCode::UnitRunning);
    }
    let Some(entry) = sup.live(req.thread, req.launch) else {
        return done(ErrorCode::NoLiveLaunch);
    };
    let basis = match entry
        .with_session(&sup, sup.now() + READ_WAIT, |s| {
            s.last_started(ClassSet::ALL)
        })
        .await
    {
        Ok(b) => b,
        Err(super::super::live::SessionError::Busy) => return done(ErrorCode::LaunchBusy),
        Err(super::super::live::SessionError::Gone) => return done(ErrorCode::NoLiveLaunch),
    };
    let (thread, launch, cid) = (req.thread, req.launch, req.command_id);
    let me = sl.clone();
    let job: JobOf<Result<(), ErrorCode>> = Arc::new(move |tx| {
        if me.running_flag.load(Ordering::SeqCst) {
            return Ok(Err(ErrorCode::UnitRunning));
        }
        let Some(t) = tx.read().thread(thread)? else {
            return Ok(Err(ErrorCode::NoLiveLaunch));
        };
        if let Err(c) = ready_checks(tx, &t, Some(launch)) {
            return Ok(Err(c));
        }
        readiness_in(
            tx,
            &t,
            launch,
            &command_key(cid, 0),
            ReadinessEvidence::OwnerAsserted,
            basis,
            Some(cid),
        )?;
        Ok(Ok(()))
    });
    match record(&sup, job, false).await {
        Recorded::Done(Ok(())) => EffectOutcome::Done(ok(&ReadyResult { launch_id: launch })),
        Recorded::Done(Err(c)) => done(c),
        Recorded::Refused(e) => {
            tracing::error!(error = ?e, "the ready job refused");
            EffectOutcome::Unrecorded
        }
        Recorded::Unrecorded => EffectOutcome::Unrecorded,
        Recorded::ShuttingDown => EffectOutcome::ShuttingDown,
    }
}

// ---- key ----------------------------------------------------------------------------------------

struct KeyHandler(Arc<Supervisor>);

struct KeyReq {
    thread: ThreadId,
    launch: LaunchId,
    keys: Vec<KeyToken>,
    card: Option<ApprovalId>,
}

impl CommandHandler for KeyHandler {
    fn name(&self) -> &'static str {
        "key"
    }

    fn deadline(&self) -> Option<Duration> {
        Some(COMMAND_DEADLINE)
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized) = parse::<KeyArgs>(args)?;
        if a.keys.is_empty() || a.keys.len() > MAX_KEYS || encode_keys(&a.keys, false).is_err() {
            return Err(ApiError::bad_args());
        }
        let host_os = self.0.host_os;
        Ok(Prepared {
            normalized,
            intent: Box::new(move |tx, auth, _ctx| {
                let t = target(tx, auth, Action::Key, a.thread_id)?;
                if a.approval_id.is_some() {
                    // One deciding key, as the last token: every earlier one navigates.
                    let nav = capabilities(t.agent, host_os).card_nav_keys();
                    let (_, before) = a.keys.split_last().unwrap_or((&KeyToken::Enter, &[]));
                    if before.iter().any(|k| !nav.contains(k)) {
                        return Err(refused(ApiError::bad_args()));
                    }
                }
                if t.closed {
                    return Err(refuse(ErrorCode::ThreadClosed));
                }
                if t.current_launch_id != Some(a.launch_id) {
                    return Err(refuse(ErrorCode::LaunchChanged));
                }
                let row = tx.read().launch(a.launch_id)?;
                if row.is_none_or(|l| l.exit_reason.is_some())
                    || t.state.process != ProcessState::Running
                {
                    return Err(refuse(ErrorCode::NoLiveLaunch));
                }
                if let Some(c) = a.approval_id {
                    let open = tx
                        .read()
                        .approval(c)?
                        .is_some_and(|r| r.launch_id == a.launch_id && r.outcome.is_none());
                    if !open {
                        return Err(refuse(ErrorCode::CardClosed));
                    }
                }
                Ok(Intent::Pending(EffectRun::new(KeyReq {
                    thread: a.thread_id,
                    launch: a.launch_id,
                    keys: a.keys.clone(),
                    card: a.approval_id,
                })))
            }),
        })
    }

    fn effect(&self) -> Option<&dyn CommandEffect> {
        Some(self)
    }
}

impl CommandEffect for KeyHandler {
    fn run(&self, run: EffectRun) -> BoxFuture<'static, EffectOutcome> {
        match run.downcast::<KeyReq>() {
            Ok(req) => run_op(&self.0, key_op(self.0.clone(), req)),
            Err(_) => Box::pin(async { EffectOutcome::Unrecorded }),
        }
    }
}

/// `key`'s effect (send §2.6 steps 1–4).
#[allow(
    clippy::too_many_lines,
    reason = "the four steps of send §2.6 in order"
)]
async fn key_op(sup: Arc<Supervisor>, req: KeyReq) -> EffectOutcome {
    // 1. The live launch and its fence word.
    let (Some(entry), Some(sl)) = (
        sup.live(req.thread, req.launch),
        sup.send.launch(req.launch),
    ) else {
        return done(ErrorCode::NoLiveLaunch);
    };
    let word = sl.fence.load(Ordering::Acquire);
    if !fence_quiet(word) {
        return done(ErrorCode::KeyNotWritten);
    }
    // 2. The launch and the card, read afresh.
    let (thread, launch, card) = (req.thread, req.launch, req.card);
    let deadline = sup.now() + READ_WAIT;
    let store = sup.store.clone();
    let real = sup.real(deadline);
    let read = sup.rt.spawn_blocking(move || {
        store.read(real, |r| {
            let t = r.thread(thread)?;
            let l = r.launch(launch)?;
            let c = card.map(|c| r.approval(c)).transpose()?.flatten();
            Ok((t, l, c))
        })
    });
    let Some(Ok(Ok((t, l, c)))) = settle_before(|| sup.now(), deadline, sup.timer(deadline), read)
        .await
        .in_time()
    else {
        return done(ErrorCode::KeyNotWritten);
    };
    let running = t.is_some_and(|t| {
        t.state.process == ProcessState::Running && t.current_launch_id == Some(launch)
    }) && l.is_some_and(|l| l.exit_reason.is_none());
    if !running {
        return done(ErrorCode::NoLiveLaunch);
    }
    let human_at = match card {
        None => None,
        Some(id) => {
            if !c.is_some_and(|c| c.outcome.is_none() && c.launch_id == launch) {
                return done(ErrorCode::CardClosed);
            }
            match sl.state().cards.get(&id).copied().flatten() {
                Some(h) => Some(h),
                None => return done(ErrorCode::KeyNotWritten),
            }
        }
    };
    // 3. The unit, inside `with_session`.
    let now = sup.now();
    let not_after = now + KEY_WAIT;
    let (tx_report, rx_report) = tokio::sync::oneshot::channel::<KeyReport>();
    let me = sl.clone();
    let keys = req.keys.clone();
    let real_not_after = sup.real(not_after);
    let submitted = entry
        .with_session(&sup, now + READ_WAIT, move |s| {
            let app_cursor = s.modes().modes.app_cursor;
            let bytes = encode_keys(&keys, app_cursor).ok()?;
            let origin = match card {
                Some(approval_id) => WriteOrigin::CardKey { approval_id },
                None => WriteOrigin::Key,
            };
            let guard = Guard {
                not_after: real_not_after,
                basis: human_at.map(|seq| Basis {
                    seq,
                    counted: ClassSet::of(&[OriginClass::Human]),
                }),
                bracketed_paste: None,
                recheck: Some(Box::new(move |ctx| {
                    if ctx.modes.modes.app_cursor == app_cursor {
                        Ok(())
                    } else {
                        Err("modes_changed".into())
                    }
                })),
                fence: Some(Fence {
                    word: me.fence.clone(),
                    expect: word,
                }),
            };
            me.submit_human(s, WriteUnit::bytes(origin, bytes), guard, Some(tx_report))
                .ok()
        })
        .await;
    let ticket = match submitted {
        Ok(Some(t)) => t,
        Ok(None) | Err(super::super::live::SessionError::Busy) => {
            return done(ErrorCode::KeyNotWritten);
        }
        Err(super::super::live::SessionError::Gone) => return done(ErrorCode::NoLiveLaunch),
    };
    // 4. The worker's report.
    let by = not_after + READ_WAIT;
    let report = settle_before(|| sup.now(), by, sup.timer(by), rx_report)
        .await
        .in_time();
    match report {
        Some(Ok(KeyReport::Recorded { complete, bytes })) => EffectOutcome::Done(ok(&KeyResult {
            outcome: if complete {
                KeyOutcome::Written
            } else {
                KeyOutcome::Incomplete
            },
            bytes_written: Some(bytes),
        })),
        Some(Ok(KeyReport::NotStarted)) => done(ErrorCode::KeyNotWritten),
        Some(Err(_)) | None => {
            // Never waits for the queue lock: a writer holding it at the unit's dispatch
            // boundary may still permit it, so that is `pending` too.
            if ticket.try_cancel() == Some(true) {
                done(ErrorCode::KeyNotWritten)
            } else {
                EffectOutcome::Done(ok(&KeyResult {
                    outcome: KeyOutcome::Pending,
                    bytes_written: None,
                }))
            }
        }
    }
}

// ---- input_resolve and input_cancel -------------------------------------------------------------

struct InputResolveHandler;

impl CommandHandler for InputResolveHandler {
    fn name(&self) -> &'static str {
        "input_resolve"
    }

    fn deadline(&self) -> Option<Duration> {
        Some(COMMAND_DEADLINE)
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized) = parse::<InputResolveArgs>(args)?;
        Ok(Prepared {
            normalized,
            intent: Box::new(move |tx, auth, ctx| {
                let Some(row) = tx.read().input(a.input_id)? else {
                    return Err(refused(ApiError::not_found()));
                };
                let t = target(tx, auth, Action::InputResolve, row.thread_id)?;
                if row.state != InputState::Unconfirmed {
                    return Err(refuse(ErrorCode::InputState));
                }
                let cid = ctx.command_id();
                let mut job = ThreadJob::new(
                    t.thread_id,
                    t.agent,
                    t.state.clone(),
                    SourceKind::Command,
                    Some(cid),
                );
                let to = input_moved(
                    &mut job,
                    command_key(cid, 0),
                    a.input_id,
                    row.state,
                    &InputInput::Resolved {
                        delivered: a.delivered,
                    },
                    caller(auth),
                    row.launch_id,
                )?;
                let cause = AttentionCause::InputUnconfirmed {
                    input_id: a.input_id,
                };
                if job.raised(cause) {
                    job.apply(
                        &command_key(cid, 1),
                        "cleared",
                        &StateInput::AttentionCleared(cause),
                        row.launch_id,
                    )?;
                }
                job.flush(tx)?;
                Ok(Intent::Completed(ok(&InputCommandResult {
                    input_id: a.input_id,
                    state: to,
                })))
            }),
        })
    }
}

struct InputCancelHandler;

impl CommandHandler for InputCancelHandler {
    fn name(&self) -> &'static str {
        "input_cancel"
    }

    fn deadline(&self) -> Option<Duration> {
        Some(COMMAND_DEADLINE)
    }

    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
        let (a, normalized) = parse::<InputCancelArgs>(args)?;
        Ok(Prepared {
            normalized,
            intent: Box::new(move |tx, auth, ctx| {
                let Some(row) = tx.read().input(a.input_id)? else {
                    return Err(refused(ApiError::not_found()));
                };
                cancel_scope(tx, auth, row.thread_id, row.created_by)?;
                let Some(t) = tx.read().thread(row.thread_id)? else {
                    return Err(refused(ApiError::not_found()));
                };
                if row.state != InputState::Queued {
                    return Err(refuse(ErrorCode::InputState));
                }
                let cid = ctx.command_id();
                let mut job = ThreadJob::new(
                    t.thread_id,
                    t.agent,
                    t.state.clone(),
                    SourceKind::Command,
                    Some(cid),
                );
                let to = input_moved(
                    &mut job,
                    command_key(cid, 0),
                    a.input_id,
                    row.state,
                    &InputInput::Cancelled,
                    caller(auth),
                    row.launch_id,
                )?;
                job.flush(tx)?;
                Ok(Intent::Completed(ok(&InputCommandResult {
                    input_id: a.input_id,
                    state: to,
                })))
            }),
        })
    }
}

/// `input_cancel`'s scope (send §2.2): a thread the token cannot read is 404; an agent may
/// cancel only an input it created (403 on a readable thread).
fn cancel_scope(
    tx: &WriteTx<'_>,
    auth: &Authorized,
    thread: ThreadId,
    created_by: Actor,
) -> Result<(), JobError> {
    let r = tx.read();
    let g = ThreadGraph::new(
        r.threads()?
            .into_iter()
            .map(|row| (row.thread_id, row.parent_thread_id)),
    );
    let readable = g.contains(thread)
        && grant(auth.kind, Action::ReadStream)
            .is_some_and(|gr| may_act_on_thread(&g, auth.thread, gr, thread));
    if !readable {
        return Err(refused(ApiError::not_found()));
    }
    match auth.kind {
        TokenKind::Owner => Ok(()),
        TokenKind::Agent
            if auth
                .thread
                .is_some_and(|me| created_by == Actor::Thread(me)) =>
        {
            Ok(())
        }
        _ => Err(refused(deny_for(&g, auth, thread))),
    }
}
