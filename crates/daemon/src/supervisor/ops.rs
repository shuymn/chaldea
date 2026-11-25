//! Launch operations (design §2.3): supervisor-owned tasks that outlive their requests and
//! own every `Session`, spawn, kill, and close result. Spawn and resume, stop, natural exit,
//! and the kill task they share.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chaldea_core::adapter::capabilities;
use chaldea_core::command::{
    CommandResult, LaunchOutcome, ResumeResult, SpawnResult, StopOutcome, StopResult,
};
use chaldea_core::event::TypedEvent;
use chaldea_core::http::ErrorCode;
use chaldea_core::ids::{Actor, Agent, CommandId, IdempotencyKey, InputId, LaunchId, ThreadId};
use chaldea_core::input::{InputInput, InputState, RejectReason};
use chaldea_core::launch::{LaunchInput, plan};
use chaldea_core::payload::{
    ExitReason, FailedStart, InputCreated, InputEncoding, InputInitialState, InputOrigin,
    LaunchReserved, LaunchStarted, SourceKind, ThreadRegistered, ThreadRole, WorktreeState,
};
use chaldea_core::state::{AttentionCause, ProcessState, StateInput, ThreadState};
use chaldea_pty_runtime::{
    ChildExit, Cleanup, CloseBounds, CloseOutcome, KillAttempt, KillReport, ProcessIdentity,
    Session, SessionConfig, SessionEvent, SpawnError, SpawnSpec,
};
use serde::Serialize;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::deadline::{Settled, settle_before};
use crate::http::{
    ApiError, COMPLETION_ATTEMPT, EffectOutcome, MintedToken, StoreFatal, TokenService, write_with,
};
use crate::store::{JobError, LaunchRow, TokenKind, WriteTx};

use super::events::{ThreadJob, command_key, launch_key, refusal, seq_of, thread_row};
use super::exit::{ExitFacts, ExitPlan, exit_in};
use super::live::{CommittedStart, LiveLaunch, OpGuard, Published};
use super::record::{JobOf, Recorded, record};
use super::tracker::TrackedSession;
use super::{
    EXIT_ROUND, EXIT_TOTAL, KILL_LOCK_WAIT, PROOF_CUTOFF, Pause, STOP_LOCK, STOP_REPORT, Supervisor,
};

// ---- results --------------------------------------------------------------------------------

/// A success body.
pub(crate) fn ok<T: Serialize>(v: &T) -> CommandResult {
    CommandResult {
        http_status: 200,
        body: serde_json::to_string(v).unwrap_or_default(),
    }
}

/// A refusal as a stored result.
pub(crate) fn refused_result(e: &ApiError) -> CommandResult {
    CommandResult {
        http_status: e.status().as_u16(),
        body: serde_json::to_string(&e.body()).unwrap_or_default(),
    }
}

/// The launch commands' codes at their statuses (design §2.2 "Codes").
pub(crate) fn code(c: ErrorCode) -> ApiError {
    use axum::http::StatusCode;
    let status = match c {
        ErrorCode::BadArgs | ErrorCode::PromptRejected | ErrorCode::PromptTooLarge => {
            StatusCode::BAD_REQUEST
        }
        ErrorCode::LaunchBusy => StatusCode::GATEWAY_TIMEOUT,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::CONFLICT,
    };
    ApiError::new(status, c)
}

fn done_code(c: ErrorCode) -> EffectOutcome {
    EffectOutcome::Done(refused_result(&code(c)))
}

fn outcome_of<R>(r: Recorded<R>) -> Result<R, EffectOutcome> {
    match r {
        Recorded::Done(v) => Ok(v),
        Recorded::Refused(e) => {
            tracing::error!(error = ?e, "a supervisor job refused");
            Err(EffectOutcome::Unrecorded)
        }
        Recorded::Unrecorded => Err(EffectOutcome::Unrecorded),
        Recorded::ShuttingDown => Err(EffectOutcome::ShuttingDown),
    }
}

// ---- spawn and resume -------------------------------------------------------------------------

/// What a spawn's or resume's effect hands to its op.
pub(crate) struct LaunchReq {
    pub command_id: CommandId,
    pub thread: ThreadId,
    pub launch: LaunchId,
    pub kind: LaunchKind,
    pub prompt: Option<String>,
    pub input_id: Option<InputId>,
    pub hook_token: MintedToken,
    pub caller: Actor,
}

#[derive(Clone)]
pub(crate) enum LaunchKind {
    Spawn {
        agent: Agent,
        cwd: String,
        task: Option<String>,
        parent: Option<ThreadId>,
    },
    Resume {
        expected: LaunchId,
    },
}

/// What the reservation job recorded.
struct Reserved {
    agent: Agent,
    cwd: PathBuf,
    upgraded: bool,
    args: Vec<String>,
    env: Vec<(OsString, OsString)>,
}

/// The spawn or resume op (design §2.3 "Spawn and resume").
#[allow(
    clippy::result_large_err,
    reason = "the spawn error carries its cleanup evidence by value, as Session::spawn returns it"
)]
pub(crate) async fn launch_op(sup: Arc<Supervisor>, req: LaunchReq) -> EffectOutcome {
    // Step 1: inputs.
    let mut workspace = None;
    if let LaunchKind::Spawn { cwd, .. } = &req.kind {
        let path = PathBuf::from(cwd);
        // Absolute: the PTY backends start the child in exactly this path.
        if !path.is_absolute() || !std::fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
            return done_code(ErrorCode::BadArgs);
        }
        workspace = Some(super::workspace::workspace_of(&sup, &path).await);
    }
    // Step 2: the slot, its op lock taken by this op.
    // Step 3: the reservation job (it builds the plan from the thread it reads).
    let job = reservation_job(&sup, &req, workspace);
    let reserved = match record(&sup, job, false).await {
        Recorded::Done(Ok((r, entry_guard))) => (r, entry_guard),
        Recorded::Done(Err(c)) => {
            return done_code(c);
        }
        other => {
            sup.remove(req.launch);
            return outcome_of(other).err().unwrap_or(EffectOutcome::Unrecorded);
        }
    };
    let (r, (entry, mut guard)) = reserved;
    sup.pauses().at(Pause::AfterReserve).await;
    // Step 4: spawn.
    let program = sup.program(r.agent);
    let token = program.as_ref().and_then(|_| sup.tracker.admit());
    let (Some(program), Some(token)) = (program, token) else {
        return spawn_failed(&sup, &req, &entry, guard, SpawnFailure::NeverStarted).await;
    };
    entry.set_creation_started();
    sup.pauses().at(Pause::BeforeSpawn).await;
    let mut argv = program;
    argv.extend(r.args.iter().map(OsString::from));
    let spec = SpawnSpec {
        argv,
        cwd: r.cwd.clone(),
        env: r.env.clone(),
        size: sup.cfg.pty_size,
    };
    let config = SessionConfig::new(exit_forwarder(&sup, req.thread, req.launch));
    let runtime = sup.cfg.runtime.clone();
    let spawned = sup
        .rt
        .spawn_blocking(
            move || match Session::spawn(runtime.as_ref(), &spec, config) {
                Ok(s) => Ok(TrackedSession::new(s, token)),
                Err(e) => {
                    if matches!(e.cleanup(), Cleanup::Confirmed) {
                        token.release();
                    }
                    Err(e)
                }
            },
        )
        .await;
    let tracked = match spawned {
        Ok(Ok(t)) => t,
        Ok(Err(SpawnError::NeverStarted { .. })) => {
            return spawn_failed(&sup, &req, &entry, guard, SpawnFailure::NeverStarted).await;
        }
        Ok(Err(SpawnError::StartedThenFailed {
            identity, cleanup, ..
        })) => {
            let confirmed = matches!(cleanup, Cleanup::Confirmed);
            if let Cleanup::Unresolved { kill, close, .. } = &cleanup {
                tracing::warn!(launch = %req.launch, ?kill, ?close, "spawn cleanup unresolved");
            }
            let failed_start = (!confirmed)
                .then_some(identity)
                .flatten()
                .map(|id| FailedStart {
                    pid: id.pid,
                    start_time: (!id.start_time.is_empty()).then_some(id.start_time),
                    pgid_or_job: (!id.group.is_empty()).then_some(id.group),
                });
            return spawn_failed(
                &sup,
                &req,
                &entry,
                guard,
                SpawnFailure::StartedThenFailed {
                    confirmed,
                    failed_start,
                },
            )
            .await;
        }
        Err(join) => {
            tracing::error!(launch = %req.launch, error = %join, "Session::spawn panicked");
            return spawn_failed(
                &sup,
                &req,
                &entry,
                guard,
                SpawnFailure::StartedThenFailed {
                    confirmed: false,
                    failed_start: None,
                },
            )
            .await;
        }
    };
    sup.pauses().at(Pause::AfterSpawn).await;
    // Step 5: publication.
    let observer = tracked.observer();
    let reference = observer.last_started(chaldea_pty_runtime::ClassSet::ALL);
    let identity = tracked.identity().clone();
    sup.pauses().at(Pause::BeforePublish).await;
    let mut held = Some(tracked);
    let published = sup.tracker.while_accepting(|| {
        sup.pauses().blocking(Pause::InPublish);
        *guard = held.take();
        entry.set_published(Published {
            observer,
            reference,
        });
    });
    if published.is_none() {
        let Some(tracked) = held else {
            return EffectOutcome::ShuttingDown;
        };
        late_creation(&sup, &entry, tracked).await;
        sup.remove(req.launch);
        return EffectOutcome::ShuttingDown;
    }
    sup.pauses().at(Pause::BeforeStartJob).await;
    // Step 6: the start job.
    let started = start_job(&sup, &req, &identity, r.upgraded);
    let seq = match outcome_of(record(&sup, started, false).await) {
        Ok(seq) => seq,
        Err(o) => return o,
    };
    entry.set_committed(CommittedStart {
        seq,
        identity,
        at: sup.now(),
    });
    drop(guard);
    launch_result(&req, LaunchOutcome::Started)
}

fn launch_result(req: &LaunchReq, outcome: LaunchOutcome) -> EffectOutcome {
    EffectOutcome::Done(match req.kind {
        LaunchKind::Spawn { .. } => ok(&SpawnResult {
            thread_id: req.thread,
            launch_id: req.launch,
            input_id: req.input_id,
            outcome,
        }),
        LaunchKind::Resume { .. } => ok(&ResumeResult {
            launch_id: req.launch,
            input_id: req.input_id,
            outcome,
        }),
    })
}

/// The reservation job (§2.3 step 3). Inserts the slot with its op lock taken before the
/// job's commit can be seen; a refusal or rollback removes it.
#[allow(clippy::type_complexity, reason = "the job's result")]
fn reservation_job(
    sup: &Arc<Supervisor>,
    req: &LaunchReq,
    workspace: Option<chaldea_core::ids::WorkspaceId>,
) -> JobOf<Result<(Reserved, (Arc<LiveLaunch>, OpGuard)), ErrorCode>> {
    let sup2 = sup.clone();
    let thread = req.thread;
    let launch = req.launch;
    let cid = req.command_id;
    let prompt = req.prompt.clone();
    let input_id = req.input_id;
    let caller = req.caller;
    let token =
        TokenService::new_token(&req.hook_token, TokenKind::Hook, Some(thread), Some(launch));
    let secret = req.hook_token.secret.expose().to_owned();
    let kind = req.kind.clone();
    Arc::new(move |tx: &mut WriteTx<'_>| {
        let (agent, cwd, prev, resume_session, mut job) = match &kind {
            LaunchKind::Spawn {
                agent,
                cwd,
                task,
                parent,
            } => {
                let ws = workspace
                    .clone()
                    .ok_or_else(|| refusal("no workspace id"))?;
                let mut job = ThreadJob::new(
                    thread,
                    *agent,
                    ThreadState::registered(task.is_some()),
                    SourceKind::Command,
                    Some(cid),
                );
                job.push_host(
                    command_key(cid, 0),
                    TypedEvent::ThreadRegistered(ThreadRegistered {
                        thread_id: thread,
                        agent: *agent,
                        cwd: cwd.clone(),
                        workspace_id: ws,
                        parent_thread_id: *parent,
                        role: ThreadRole::Standard,
                        task: task.clone(),
                    }),
                );
                (*agent, cwd.clone(), None, None, job)
            }
            LaunchKind::Resume { expected } => {
                let expected = *expected;
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
                let Some(prev) = r.launch(expected)? else {
                    return Ok(Err(ErrorCode::LaunchChanged));
                };
                if prev.exit_reason.is_none() {
                    return Ok(Err(ErrorCode::LaunchLive));
                }
                let mut job = ThreadJob::new(
                    thread,
                    t.agent,
                    t.state.clone(),
                    SourceKind::Command,
                    Some(cid),
                );
                if !prev.death_confirmed {
                    if !job.raised(AttentionCause::ResumeBlocked) {
                        let key = command_key(cid, 0);
                        job.apply(
                            &key,
                            "resume_blocked",
                            &StateInput::AttentionRaised(AttentionCause::ResumeBlocked),
                            Some(expected),
                        )?;
                        job.flush(tx)?;
                    }
                    return Ok(Err(ErrorCode::DeathUnconfirmed));
                }
                if let Some(w) = &prev.worktree_id {
                    let state = tx.read().worktree(w)?.map(|row| row.record.state);
                    if !matches!(state, Some(WorktreeState::Active | WorktreeState::Released)) {
                        return Ok(Err(ErrorCode::WorktreeUnavailable));
                    }
                }
                let Some(session) = t.session_id.clone() else {
                    return Ok(Err(ErrorCode::NoSession));
                };
                (t.agent, t.cwd.clone(), Some(prev), Some(session), job)
            }
        };
        let lp = plan(&LaunchInput {
            agent,
            host_os: sup2.host_os,
            thread_id: thread,
            launch_id: launch,
            resume_session,
            prompt: prompt.clone(),
            worktree_id: prev
                .as_ref()
                .and_then(|p: &LaunchRow| p.worktree_id.clone()),
            daemon_env: sup2.daemon_env(),
            key_eq: chaldea_pty_runtime::env_key_eq,
            key_has_prefix: chaldea_pty_runtime::env_key_has_prefix,
            port: sup2.port,
            hook_token: secret.clone(),
        });
        let rkey = launch_key(launch, "reserved");
        let lid = Some(launch);
        job.push(
            rkey.clone(),
            TypedEvent::LaunchReserved(LaunchReserved {
                launch_id: launch,
                expected_previous: prev.as_ref().map(|p| p.launch_id),
                agent,
                argv_digest: lp.argv_digest.clone(),
                manual: lp.manual,
                worktree_id: lp.worktree_id.clone(),
            }),
            lid,
        );
        job.apply(
            &rkey,
            "reserved",
            &StateInput::Reserved {
                relaunch: prev.is_some(),
            },
            lid,
        )?;
        if let (Some(text), Some(id)) = (&prompt, input_id) {
            let prepared = crate::store::prepare_content(
                text.as_bytes().to_vec(),
                "text/plain; charset=utf-8",
            )
            .map_err(refusal)?;
            job.push_blobs(
                command_key(cid, 1),
                TypedEvent::InputCreated(InputCreated {
                    input_id: id,
                    launch_id: lid,
                    initial_state: InputInitialState::Sending,
                    text: prepared.content,
                    encoding: InputEncoding::Positional,
                    origin: InputOrigin::Cli,
                    created_by: caller,
                }),
                lid,
                prepared.blob.into_iter().collect(),
            );
        }
        job.flush(tx)?;
        tx.token_insert(token.clone())?;
        // The slot, its op lock taken, exists before this job's commit is visible.
        let entry = Arc::new(LiveLaunch::new(
            thread,
            launch,
            agent,
            capabilities(agent, sup2.host_os),
            lp.manual,
            input_id,
        ));
        let guard = entry
            .ops
            .clone()
            .try_lock_owned()
            .map_err(|_| refusal("a new slot's lock is free"))?;
        sup2.live.lock().insert(launch, entry.clone());
        Ok(Ok((
            Reserved {
                agent,
                cwd: PathBuf::from(cwd),
                upgraded: lp.upgraded,
                args: lp.args,
                env: lp.env,
            },
            (entry, guard),
        )))
    })
}

/// The start job (§2.3 step 6, `Ok`): `launch_started`, `Spawned`, the positional input
/// `sending → sent` after that seq, and an upgraded Codex launch's `startup_stall`.
fn start_job(
    sup: &Arc<Supervisor>,
    req: &LaunchReq,
    id: &ProcessIdentity,
    upgraded: bool,
) -> JobOf<u64> {
    let sup2 = sup.clone();
    let thread = req.thread;
    let launch = req.launch;
    let input = req.input_id;
    let id = id.clone();
    Arc::new(move |tx: &mut WriteTx<'_>| {
        sup2.pauses().blocking(Pause::InStartJob);
        let t = thread_row(tx, thread)?;
        let mut job = ThreadJob::new(thread, t.agent, t.state, SourceKind::Process, None);
        let (key, seq) = started_in(tx, &mut job, launch, &id, input)?;
        let lid = Some(launch);
        if upgraded && t.agent == Agent::Codex && !job.raised(AttentionCause::StartupStall) {
            job.apply(
                &key,
                "startup_stall",
                &StateInput::AttentionRaised(AttentionCause::StartupStall),
                lid,
            )?;
        }
        job.flush(tx)?;
        Ok(seq)
    })
}

/// `launch_started` from `id`, `Spawned`, then the positional input `sending → sent` with
/// `sent_after_seq` = that event's seq, in `job`'s transaction: the start job's records, and
/// the exit job's when it records a start the start job did not commit. Returns the
/// event's key and seq.
pub(crate) fn started_in(
    tx: &mut WriteTx<'_>,
    job: &mut ThreadJob,
    launch: LaunchId,
    id: &ProcessIdentity,
    input: Option<InputId>,
) -> Result<(IdempotencyKey, u64), JobError> {
    let key = launch_key(launch, "started");
    let lid = Some(launch);
    job.push(
        key.clone(),
        TypedEvent::LaunchStarted(LaunchStarted {
            launch_id: launch,
            pid: id.pid,
            start_time: id.start_time.clone(),
            pgid_or_job: id.group.clone(),
            cli_version: None,
        }),
        lid,
    );
    job.apply(&key, "spawned", &StateInput::Spawned, lid)?;
    let appended = job.flush(tx)?;
    let seq = seq_of(&appended, &key)?;
    if let Some(i) = input
        && let Some(row) = tx.read().input(i)?
        && row.state == InputState::Sending
    {
        job.step_input(&key, &row, &InputInput::WriteCompleted, Some(seq), lid)?;
    }
    Ok((key, seq))
}

enum SpawnFailure {
    NeverStarted,
    StartedThenFailed {
        confirmed: bool,
        failed_start: Option<FailedStart>,
    },
}

/// §2.3 step 6, the failure branches: `launch_exited { spawn_failed }` and the exit steps.
async fn spawn_failed(
    sup: &Arc<Supervisor>,
    req: &LaunchReq,
    entry: &Arc<LiveLaunch>,
    guard: OpGuard,
    f: SpawnFailure,
) -> EffectOutcome {
    let mut plan = ExitPlan::new(req.thread, req.launch, ExitReason::SpawnFailed);
    match f {
        SpawnFailure::NeverStarted => {
            plan.death_confirmed = true;
            plan.positional = req
                .input_id
                .map(|i| (i, InputInput::Rejected(RejectReason::SpawnFailed)));
        }
        SpawnFailure::StartedThenFailed {
            confirmed,
            failed_start,
        } => {
            plan.death_confirmed = confirmed;
            plan.failed_start = failed_start;
        }
    }
    plan.sealed = entry.settled();
    let r = record_exit(sup, plan).await;
    drop(guard);
    sup.remove(req.launch);
    match r {
        Ok(_) => launch_result(req, LaunchOutcome::SpawnFailed),
        Err(o) => o,
    }
}

/// Records an exit job (with the nudge) and runs the steps' `after_commit`.
async fn record_exit(sup: &Arc<Supervisor>, plan: ExitPlan) -> Result<(), EffectOutcome> {
    let facts = commit_exit(sup, plan).await?;
    after_commit(sup, facts.as_ref());
    Ok(())
}

/// [`record_exit`]'s job, without the steps' `after_commit`.
async fn commit_exit(
    sup: &Arc<Supervisor>,
    plan: ExitPlan,
) -> Result<Option<ExitFacts>, EffectOutcome> {
    let steps = sup.cfg.exit_steps.clone();
    let job: JobOf<Option<ExitFacts>> = Arc::new(move |tx| exit_in(tx, &steps, &plan));
    outcome_of(record(sup, job, true).await)
}

/// The exit steps' `after_commit`, when the exit job wrote an exit.
fn after_commit(sup: &Supervisor, facts: Option<&ExitFacts>) {
    if let Some(f) = facts {
        for s in &sup.cfg.exit_steps {
            s.after_commit(f);
        }
    }
}

/// `config.on_event`: forwards `Exited` and `WaitFailed` to the launch's exit op without
/// blocking.
fn exit_forwarder(
    sup: &Arc<Supervisor>,
    thread: ThreadId,
    launch: LaunchId,
) -> impl Fn(SessionEvent) + Send + Sync + 'static {
    let weak = Arc::downgrade(sup);
    let rt = sup.rt.clone();
    let fired = AtomicBool::new(false);
    move |e| {
        let exit = match e {
            SessionEvent::Exited(x) => Some(x),
            SessionEvent::WaitFailed(_) => None,
            _ => return,
        };
        if fired.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(sup) = weak.upgrade() {
            rt.spawn(exit_op(sup, thread, launch, exit));
        }
    }
}

// ---- the kill task --------------------------------------------------------------------------

/// A report that proves the tree dead (Commitment #1).
pub(crate) fn proving(r: &KillReport) -> bool {
    r.tree_dead && matches!(r.attempt, KillAttempt::Signalled | KillAttempt::AlreadyDead)
}

/// A submitted kill task: the instant of its submission, its join handle, and the one-shot
/// of its first proving report.
pub(crate) struct KillTask {
    submitted: Instant,
    join: JoinHandle<CloseOutcome>,
    first: oneshot::Receiver<KillReport>,
}

#[cfg(test)]
impl KillTask {
    /// A kill task's parts, for tests of [`await_kill`].
    pub(crate) fn of(
        submitted: Instant,
        join: JoinHandle<CloseOutcome>,
        first: oneshot::Receiver<KillReport>,
    ) -> KillTask {
        KillTask {
            submitted,
            join,
            first,
        }
    }
}

/// Submits the kill task (§2.3 Stop step 4) and reads the clock for its submission.
pub(crate) fn submit_kill(
    sup: &Arc<Supervisor>,
    entry: &LiveLaunch,
    tracked: TrackedSession,
) -> KillTask {
    let (tx, first) = oneshot::channel();
    let s = sup.clone();
    let extra = entry.extra();
    let launch = entry.launch_id;
    // The cutoff's origin, read before the task exists, so no task start precedes it.
    let submitted = sup.now();
    sup.pauses().blocking(Pause::KillSubmitted);
    let join = sup
        .rt
        .spawn_blocking(move || kill_task(&s, launch, tracked, &extra, tx));
    KillTask {
        submitted,
        join,
        first,
    }
}

fn kill_task(
    sup: &Supervisor,
    launch: LaunchId,
    tracked: TrackedSession,
    extra: &[ProcessIdentity],
    tx: oneshot::Sender<KillReport>,
) -> CloseOutcome {
    let start = sup.now();
    let kill_end = start + sup.cfg.kill_budget;
    tracing::info!(%launch, ?kill_end, "kill task started");
    sup.pauses().blocking(Pause::KillTaskStart);
    loop {
        let now = sup.now();
        let lock_by = (now + KILL_LOCK_WAIT).min(kill_end);
        let r = tracked.kill_by(extra, sup.real(lock_by), sup.real(kill_end));
        tracing::info!(%launch, ?lock_by, attempt = ?r.attempt, method = %r.method, proved_at = ?r.proved_at, "kill call");
        if proving(&r) {
            let _ = tx.send(r);
            break;
        }
        if !r.tree_dead && !r.survivors.is_empty() {
            tracing::warn!(%launch, survivors = ?r.survivors, "kill left survivors");
        }
        if !(r.is_busy() && sup.now() < kill_end) {
            break;
        }
    }
    sup.pauses().blocking(Pause::BeforeCloseBy);
    let o = tracked.close_by(extra, sup.real(kill_end), sup.cfg.close_bounds);
    if !o.kill.tree_dead {
        tracing::warn!(%launch, survivors = ?o.kill.survivors, "close left survivors");
    }
    sup.pauses().blocking(Pause::KillTaskEnd);
    o
}

/// What a kill task gave the op by its cutoff.
#[derive(Debug, Default)]
pub(crate) struct Proof {
    pub report: Option<KillReport>,
    pub exit: Option<ChildExit>,
}

/// §2.3 Stop step 5: the one-shot and the join, each through `settle_before` against the
/// kill's submission + 12 s; a value counts only when received before it.
pub(crate) async fn await_kill(sup: &Supervisor, k: KillTask) -> Proof {
    let cutoff = k.submitted + PROOF_CUTOFF;
    let now = || sup.now();
    let (first, joined) = tokio::join!(
        settle_before(now, cutoff, sup.timer(cutoff), k.first),
        settle_before(now, cutoff, sup.timer(cutoff), k.join),
    );
    let first = first.in_time().and_then(Result::ok);
    let joined = match joined {
        Settled::InTime(Ok(o)) => Some(o),
        Settled::InTime(Err(e)) => {
            tracing::error!(error = %e, "kill task panicked");
            None
        }
        Settled::Late(_) | Settled::Elapsed => None,
    };
    let report = first.or_else(|| joined.as_ref().map(|o| o.kill.clone()).filter(proving));
    Proof {
        report,
        exit: joined.and_then(|o| o.exit),
    }
}

// ---- stop -----------------------------------------------------------------------------------

/// What a stop's effect hands to its op.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StopReq {
    pub command_id: CommandId,
    pub thread: ThreadId,
    pub launch: LaunchId,
    pub request_start: Instant,
}

/// The stop op (design §2.3 "Stop"): `Ok` the stop's result, `Err` an effect outcome
/// (a refusal stored as `Done`, or not recorded).
pub(crate) async fn stop_launch(
    sup: &Arc<Supervisor>,
    req: StopReq,
) -> Result<StopResult, EffectOutcome> {
    let t0 = sup.now();
    let bound = t0 + STOP_LOCK;
    sup.pauses().at(Pause::StopStart).await;
    let entry = sup.entry(req.launch);
    // Step 1: the lock.
    let guard = match &entry {
        Some(e) => match e.lock_before(sup, bound).await {
            Some(g) => Some(g),
            None => return Err(done_code(ErrorCode::LaunchBusy)),
        },
        None => None,
    };
    // Step 2: the re-read.
    let store = sup.store.clone();
    let (thread, launch) = (req.thread, req.launch);
    let wait = sup.real(bound);
    let read = sup
        .rt
        .spawn_blocking(move || store.read(wait, |r| Ok((r.thread(thread)?, r.launch(launch)?))));
    let Some(Ok(Ok((Some(t), Some(l))))) =
        settle_before(|| sup.now(), bound, sup.timer(bound), read)
            .await
            .in_time()
    else {
        return Err(done_code(ErrorCode::LaunchBusy));
    };
    let read_at = sup.now();
    if t.current_launch_id != Some(req.launch) {
        return Err(done_code(ErrorCode::LaunchChanged));
    }
    if l.exit_reason.is_some() {
        if !l.death_confirmed {
            return Err(done_code(ErrorCode::DeathUnconfirmed));
        }
        return Ok(already_exited(
            req.launch,
            read_at <= req.request_start + STOP_REPORT,
        ));
    }
    let (Some(entry), Some(mut guard)) = (entry, guard) else {
        // An open launch with no slot: its op ended without recording; the next start
        // records it.
        return Err(sup.unrecorded_outcome());
    };
    // Step 3: seal, then submit `StopRequested` with the kill in its `on_submit`.
    let Some(tracked) = entry.seal_for_stop(&mut guard) else {
        return Err(sup.unrecorded_outcome());
    };
    sup.pauses().at(Pause::StopAfterSeal).await;
    let (ktx, krx) = oneshot::channel::<KillTask>();
    let s3 = sup.clone();
    let cid = req.command_id;
    let job: crate::http::Job<()> = Box::new(move |tx| {
        s3.pauses().blocking(Pause::InStopRequested);
        stop_requested_in(tx, thread, launch, cid)
    });
    let s4 = sup.clone();
    let e4 = entry.clone();
    let write = write_with(
        &sup.store,
        sup.sink.as_ref(),
        sup.clock(),
        sup.now() + COMPLETION_ATTEMPT,
        job,
        None,
        move || {
            let _ = ktx.send(submit_kill(&s4, &e4, tracked));
        },
    );
    let waits = async {
        match krx.await {
            Ok(k) => await_kill(sup, k).await,
            Err(_) => Proof::default(),
        }
    };
    let (written, proof) = tokio::join!(write, waits);
    if let Err(f) = written {
        tracing::warn!(%launch, failure = ?f, "StopRequested was not committed");
    }
    sup.pauses().at(Pause::StopWritten).await;
    // Step 5: the exit job.
    let mut plan = ExitPlan::new(thread, launch, ExitReason::Stopped);
    plan.stop_first = true;
    plan.death_confirmed = proof.report.is_some();
    plan.method = proof.report.as_ref().map(|r| r.method.clone());
    plan.code = proof.exit.and_then(|x| x.code);
    plan.signal = proof.exit.and_then(|x| x.signal);
    plan.sealed = entry.settled();
    let r = record_exit(sup, plan).await;
    drop(guard);
    sup.remove(launch);
    r?;
    let within = proof
        .report
        .as_ref()
        .and_then(|r| r.proved_at)
        .is_some_and(|p| p <= sup.real(req.request_start + STOP_REPORT));
    Ok(StopResult {
        launch_id: launch,
        outcome: StopOutcome::Stopped,
        death_confirmed: proof.report.is_some(),
        confirmed_within_deadline: within,
        exit_code: proof.exit.and_then(|x| x.code),
        signal: proof.exit.and_then(|x| x.signal),
    })
}

impl Supervisor {
    fn unrecorded_outcome(&self) -> EffectOutcome {
        if self.tracker.accepting() {
            EffectOutcome::Unrecorded
        } else {
            EffectOutcome::ShuttingDown
        }
    }
}

pub(crate) fn already_exited(launch: LaunchId, within: bool) -> StopResult {
    StopResult {
        launch_id: launch,
        outcome: StopOutcome::AlreadyExited,
        death_confirmed: true,
        confirmed_within_deadline: within,
        exit_code: None,
        signal: None,
    }
}

/// The stop's own `StopRequested` job: applied only when the launch is current and
/// `running`.
fn stop_requested_in(
    tx: &mut WriteTx<'_>,
    thread: ThreadId,
    launch: LaunchId,
    cid: CommandId,
) -> Result<(), JobError> {
    let t = thread_row(tx, thread)?;
    if t.current_launch_id != Some(launch) || t.state.process != ProcessState::Running {
        return Ok(());
    }
    let mut job = ThreadJob::new(thread, t.agent, t.state, SourceKind::Command, Some(cid));
    let key = launch_key(launch, "stop_requested");
    job.apply(&key, "stop", &StateInput::StopRequested, Some(launch))?;
    job.flush(tx)?;
    Ok(())
}

pub(crate) async fn stop_op(sup: Arc<Supervisor>, req: StopReq) -> EffectOutcome {
    match stop_launch(&sup, req).await {
        Ok(r) => EffectOutcome::Done(ok(&r)),
        Err(o) => o,
    }
}

// ---- natural exit ---------------------------------------------------------------------------

/// The exit op (design §2.3 "Natural exit").
pub(crate) async fn exit_op(
    sup: Arc<Supervisor>,
    thread: ThreadId,
    launch: LaunchId,
    exit: Option<ChildExit>,
) {
    sup.pauses().at(Pause::ExitStart).await;
    let Some(entry) = sup.entry(launch) else {
        return;
    };
    if entry.stop_requested() {
        return;
    }
    sup.pauses().at(Pause::ExitBeforeLock).await;
    let start = sup.now();
    let mut guard = loop {
        if entry.stop_requested() {
            return;
        }
        sup.pauses().at(Pause::ExitRound).await;
        // No round runs past the total: a lock taken after it does not count.
        let round = (sup.now() + EXIT_ROUND).min(start + EXIT_TOTAL);
        if let Some(g) = entry.lock_before(&sup, round).await {
            break g;
        }
        if entry.stop_requested() || sup.entry(launch).is_none() {
            return;
        }
        if sup.now() >= start + EXIT_TOTAL {
            if sup.tracker.accepting() {
                sup.sink
                    .store_fatal(StoreFatal::LaunchUnrecorded { launch_id: launch });
            }
            return;
        }
    };
    if entry.stop_requested() || guard.is_none() {
        return;
    }
    entry.seal(&guard);
    let Some(tracked) = guard.take() else {
        return;
    };
    let unwound = UnrecordedOnUnwind { sup: &sup, launch };
    sup.pauses().at(Pause::ExitTookSession).await;
    let k = submit_kill(&sup, &entry, tracked);
    let proof = await_kill(&sup, k).await;
    let mut plan = ExitPlan::new(thread, launch, ExitReason::Exit);
    plan.death_confirmed = proof.report.is_some();
    plan.method = proof.report.as_ref().map(|r| r.method.clone());
    plan.code = exit.and_then(|x| x.code);
    plan.signal = exit.and_then(|x| x.signal);
    plan.sealed = entry.settled();
    let r = commit_exit(&sup, plan).await;
    // The exit job answered: an unwind from here on (an exit step's `after_commit`) leaves
    // nothing unrecorded, and an unrecorded answer is reported below.
    drop(unwound);
    if let Ok(f) = &r {
        after_commit(&sup, f.as_ref());
    }
    drop(guard);
    sup.remove(launch);
    if let Err(EffectOutcome::Unrecorded) = r {
        sup.sink
            .store_fatal(StoreFatal::LaunchUnrecorded { launch_id: launch });
    }
}

/// Reports `LaunchUnrecorded` for `launch` when dropped during an unwind while admission is
/// open: an exit op that panics after it took the session and before its exit job answered
/// (when the op drops it) records no exit for it. Once shutdown started, the op ends
/// unreported, as every op shutdown ends does (PR10a1-CL-22).
struct UnrecordedOnUnwind<'a> {
    sup: &'a Supervisor,
    launch: LaunchId,
}

impl Drop for UnrecordedOnUnwind<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let fatal = StoreFatal::LaunchUnrecorded {
                launch_id: self.launch,
            };
            // A panic leaving a drop during an unwind aborts the process.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if self.sup.tracker.accepting() {
                    self.sup.sink.store_fatal(fatal);
                }
            }));
        }
    }
}

// ---- late creations and the participant's closes ----------------------------------------------

/// `CloseBounds::default()` scaled to fit `avail` (design §2.9 step 4).
pub(crate) fn scaled(avail: Duration) -> CloseBounds {
    let d = CloseBounds::default();
    let total = d.total().as_secs_f64();
    let k = if total > 0.0 {
        (avail.as_secs_f64() / total).clamp(0.0, 1.0)
    } else {
        0.0
    };
    CloseBounds {
        kill: d.kill.mul_f64(k),
        input: d.input.mul_f64(k),
        close: d.close.mul_f64(k),
        drain: d.drain.mul_f64(k),
        join: d.join.mul_f64(k),
    }
}

/// Seals and closes `tracked` on a blocking task joined until `close_end`, then records its
/// exit by `end` (design §2.9 steps 4–5). `pause` holds the close task before it returns.
pub(crate) async fn close_and_record(
    sup: &Arc<Supervisor>,
    entry: &LiveLaunch,
    tracked: TrackedSession,
    pause: Pause,
) {
    let (close_end, end) = sup.window_or_now();
    entry.add_settled(tracked.seal());
    let identity = tracked.identity().clone();
    let bounds = scaled(close_end.saturating_duration_since(sup.now()));
    let extra = entry.extra();
    let s = sup.clone();
    let join = sup.rt.spawn_blocking(move || {
        let o = tracked.close(&extra, bounds);
        s.pauses().blocking(pause);
        o
    });
    let Some(Ok(o)) = settle_before(|| sup.now(), close_end, sup.timer(close_end), join)
        .await
        .in_time()
    else {
        return;
    };
    if !o.kill.tree_dead {
        tracing::warn!(launch = %entry.launch_id, survivors = ?o.kill.survivors, "shutdown close left survivors");
    }
    let mut plan = ExitPlan::new(entry.thread_id, entry.launch_id, ExitReason::Stopped);
    plan.start = Some((identity, entry.positional));
    plan.stop_first = true;
    plan.death_confirmed = proving(&o.kill);
    plan.method = plan.death_confirmed.then(|| o.kill.method.clone());
    plan.code = o.exit.and_then(|x| x.code);
    plan.signal = o.exit.and_then(|x| x.signal);
    plan.sealed = entry.settled();
    let steps = sup.cfg.exit_steps.clone();
    let job: crate::http::Job<Option<ExitFacts>> = Box::new(move |tx| exit_in(tx, &steps, &plan));
    let write = write_with(
        &sup.store,
        sup.sink.as_ref(),
        sup.clock(),
        end,
        job,
        None,
        || {},
    );
    if let Some(Ok(f)) = settle_before(|| sup.now(), end, sup.timer(end), write)
        .await
        .in_time()
    {
        after_commit(sup, f.as_ref());
    }
}

/// A creation that returned after admission closed (design §2.9 "Late creations").
async fn late_creation(sup: &Arc<Supervisor>, entry: &LiveLaunch, tracked: TrackedSession) {
    close_and_record(sup, entry, tracked, Pause::LateCloseEnd).await;
}
