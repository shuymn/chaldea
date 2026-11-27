//! The exit job (design §2.11 "Exit job order"): `launch_exited` with the reducer's
//! `Exited` (and `launch_death_confirmed` with its method, or the `launch_evidence` of a death
//! left unconfirmed, `docs/design/supervisor-recovery.md` §2.4), the registered [`ExitStep`]s in
//! order, 10a1's input sweep, then `buffer::resolve_unbindable`. The restart reconciler runs
//! the same steps with `restart: true`.

use std::sync::Arc;

use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{InputId, LaunchId, ThreadId};
use chaldea_core::input::{InputInput, InputState};
use chaldea_core::payload::{
    EvidenceSource, ExitReason, FailedStart, LaunchDeathConfirmed, LaunchEvidence, LaunchExited,
    SourceKind,
};
use chaldea_core::state::{ProcessState, StateInput};
use chaldea_pty_runtime::{ProcessIdentity, WriteRecord};

use crate::store::{JobError, StoreError, WriteTx};

use super::events::{ThreadJob, launch_key, refusal, thread_row};

/// A step 10b (and later PRs) add to every exit job, before 10a1's sweep.
pub(crate) trait ExitStep: Send + Sync {
    /// Runs in the exit job, after `launch_exited`.
    ///
    /// # Errors
    ///
    /// The store's refusal: the exit job rolls back.
    fn in_exit_job(&self, tx: &mut WriteTx<'_>, exit: &ExitFacts) -> Result<(), StoreError>;
    /// Runs after the exit job committed.
    fn after_commit(&self, _exit: &ExitFacts) {}
}

/// What the exit job recorded, as the steps see it.
#[derive(Debug, Clone)]
pub(crate) struct ExitFacts {
    pub thread_id: ThreadId,
    pub launch_id: LaunchId,
    pub restart: bool,
    /// The launch's `settled`; empty at restart.
    pub sealed: Vec<WriteRecord>,
}

/// What one exit job records.
#[derive(Debug, Clone)]
pub(crate) struct ExitPlan {
    pub thread: ThreadId,
    pub launch: LaunchId,
    pub reason: ExitReason,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub death_confirmed: bool,
    /// The proving report's method, recorded as `launch_death_confirmed`.
    pub method: Option<String>,
    pub failed_start: Option<FailedStart>,
    pub restart: bool,
    pub sealed: Vec<WriteRecord>,
    /// Applies `StopRequested` first when the recorded state is still `running` (a stop
    /// whose own write did not commit; the participant).
    pub stop_first: bool,
    /// Records the start first, as the start job would, when the launch has none (the
    /// participant's or a late creation's published session whose start job did not
    /// commit): the identity, the launch's positional input, and its CLI version.
    pub start: Option<(ProcessIdentity, Option<InputId>, Option<String>)>,
    /// The positional input's own move, before the steps (a spawn failure).
    pub positional: Option<(InputId, InputInput)>,
    /// What an owned close or a failed start could not prove, appended right after
    /// `launch_exited` (`spawn_failed` or `close` evidence).
    pub evidence: Option<LaunchEvidence>,
}

impl ExitPlan {
    pub(crate) fn new(thread: ThreadId, launch: LaunchId, reason: ExitReason) -> ExitPlan {
        ExitPlan {
            thread,
            launch,
            reason,
            code: None,
            signal: None,
            death_confirmed: false,
            method: None,
            failed_start: None,
            restart: false,
            sealed: Vec::new(),
            stop_first: false,
            start: None,
            positional: None,
            evidence: None,
        }
    }
}

/// The exit job's body. `Ok(None)`: the launch already has an exit (nothing written).
///
/// # Errors
///
/// A store error or a rule refusal: the job rolls back.
pub(crate) fn exit_in(
    tx: &mut WriteTx<'_>,
    steps: &[Arc<dyn ExitStep>],
    plan: &ExitPlan,
) -> Result<Option<ExitFacts>, JobError> {
    let r = tx.read();
    let Some(l) = r.launch(plan.launch)? else {
        return Err(refusal("the launch has no row"));
    };
    if l.exit_reason.is_some() {
        return Ok(None);
    }
    let t = thread_row(tx, plan.thread)?;
    let kind = if plan.restart {
        SourceKind::Daemon
    } else {
        SourceKind::Process
    };
    let mut job = ThreadJob::new(t.thread_id, t.agent, t.state.clone(), kind, None);
    let lid = Some(plan.launch);
    if let Some((id, input, version)) = &plan.start
        && l.pid.is_none()
        && job.state.process == ProcessState::Starting
    {
        super::ops::started_in(tx, &mut job, plan.launch, id, *input, version.clone())?;
    }
    if plan.stop_first && job.state.process == ProcessState::Running {
        let key = launch_key(plan.launch, "stop_requested");
        job.apply(&key, "stop", &StateInput::StopRequested, lid)?;
    }
    let key = launch_key(plan.launch, "exited");
    job.push(
        key.clone(),
        TypedEvent::LaunchExited(LaunchExited {
            launch_id: plan.launch,
            code: plan.code,
            signal: plan.signal,
            reason: plan.reason,
            death_confirmed: plan.death_confirmed,
            failed_start: plan.failed_start.clone(),
        }),
        lid,
    );
    if let Some(ev) = &plan.evidence {
        job.push(
            evidence_key(plan.launch, ev.source),
            TypedEvent::LaunchEvidence(ev.clone()),
            lid,
        );
    }
    job.apply(
        &key,
        "exited",
        &StateInput::Exited {
            reason: plan.reason,
        },
        lid,
    )?;
    if let (true, Some(method)) = (plan.death_confirmed, &plan.method) {
        let dkey = launch_key(plan.launch, "death_confirmed");
        job.push(
            dkey.clone(),
            TypedEvent::LaunchDeathConfirmed(LaunchDeathConfirmed {
                launch_id: plan.launch,
                method: method.clone(),
            }),
            lid,
        );
        job.apply(&dkey, "death", &StateInput::DeathConfirmed, lid)?;
    }
    if let Some((input, by)) = &plan.positional
        && let Some(row) = tx.read().input(*input)?
        && row.state == InputState::Sending
    {
        job.step_input(&key, &row, by, None, lid)?;
    }
    job.flush(tx)?;
    let facts = ExitFacts {
        thread_id: plan.thread,
        launch_id: plan.launch,
        restart: plan.restart,
        sealed: plan.sealed.clone(),
    };
    for s in steps {
        s.in_exit_job(tx, &facts)?;
    }
    sweep(tx, plan.thread, Some(plan.launch), plan.restart, &key)?;
    crate::hook::resolve_unbindable(tx, plan.launch)?;
    Ok(Some(facts))
}

/// `launch:<L>:evidence:spawn_failed` and `launch:<L>:evidence:close`: written at most once
/// per launch, as `launch:<L>:exited` is. An exit plan carries no probe's evidence: the
/// prober writes its own, keyed with its attempt id (`super::prober`).
fn evidence_key(launch: LaunchId, source: EvidenceSource) -> chaldea_core::ids::IdempotencyKey {
    let name = match source {
        EvidenceSource::SpawnFailed => "spawn_failed",
        EvidenceSource::Close => "close",
        EvidenceSource::Probe => unreachable!("an exit plan carries no probe's evidence"),
    };
    launch_key(launch, &format!("evidence:{name}"))
}

/// 10a1's input sweep: the thread's inputs still `queued`, `sending`, or `sent`, of
/// `launch` or of no launch (all of the thread's when `launch` is `None`), move
/// `queued → cancelled` and `sending`/`sent → unconfirmed` (raising `input_unconfirmed`)
/// through `LaunchExited`, or `DaemonRestarted` when `restart`. `unconfirmed` inputs keep
/// their state and attention.
pub(crate) fn sweep(
    tx: &mut WriteTx<'_>,
    thread: ThreadId,
    launch: Option<LaunchId>,
    restart: bool,
    cause: &chaldea_core::ids::IdempotencyKey,
) -> Result<(), JobError> {
    let r = tx.read();
    let inputs = r.inputs(
        thread,
        &[InputState::Queued, InputState::Sending, InputState::Sent],
    )?;
    if inputs.is_empty() {
        return Ok(());
    }
    let t = thread_row(tx, thread)?;
    let kind = if restart {
        SourceKind::Daemon
    } else {
        SourceKind::Process
    };
    let by = if restart {
        InputInput::DaemonRestarted
    } else {
        InputInput::LaunchExited
    };
    let mut job = ThreadJob::new(t.thread_id, t.agent, t.state, kind, None);
    for i in inputs {
        if launch.is_some() && i.launch_id.is_some() && i.launch_id != launch {
            continue;
        }
        job.step_input(cause, &i, &by, None, i.launch_id.or(launch))?;
    }
    job.flush(tx)?;
    Ok(())
}
