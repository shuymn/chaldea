//! The driver and the per-launch workers (send §2.3): every pass applies the launch's
//! records, then its running unit's dispatch wait and stall, its timers, its settle, and the
//! open of its oldest queued input, each decided in a job that re-reads the store.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{IdempotencyKey, InputId, StreamId};
use chaldea_core::input::encode::{Purpose, encode};
use chaldea_core::input::{
    GateClosedReason, GateDecision, GateView, InputInput, InputState, blocks_gate, gate,
};
use chaldea_core::payload::{InputEncoding, ReadinessEvidence, SourceKind};
use chaldea_core::state::{AttentionCause, ProcessState, StateInput, TurnState};
use chaldea_pty_runtime::{
    Basis, ClassSet, Fence, Guard, ModeView, OriginClass, Part, WriteOrigin, WriteOutcome,
    WriteRecord, WriteUnit,
};
use tokio::sync::broadcast::error::RecvError;

use super::super::Supervisor;
use super::super::events::{ThreadJob, derived, seq_of, thread_row};
use super::super::live::{LaunchObservation, LiveLaunch};
use super::super::record::{JobOf, Recorded, record};
use super::attribution::content_text;
use super::completion::{apply_records, move_input};
use super::{
    ACK_WINDOW, DISPATCH_WAIT, Dispatch, Fired, GATE_TIMEOUT, READ_WAIT, RunningUnit, SAMPLE_WAIT,
    SETTLE, SendLaunch, WRITE_STALL, fence_quiet, lock,
};
use crate::deadline::settle_before;
use crate::store::{CountedClass, CountedClasses, EventAnnex, InputRow, LaunchRow, ThreadRow};

/// Wakes the workers: a commit naming a thread wakes its launches, a tick or a lag wakes
/// every launch.
pub(crate) async fn driver(sup: Arc<Supervisor>) {
    let mut commits = sup.store.subscribe();
    let mut ticks = sup.send.ticks.subscribe();
    loop {
        tokio::select! {
            r = commits.recv() => match r {
                Ok(c) => {
                    for h in c.heads {
                        if let StreamId::Thread(t) = h.stream_id {
                            for sl in sup.send.all() {
                                if sl.thread_id == t {
                                    sl.wake.notify_one();
                                }
                            }
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => wake_all(&sup, None),
                Err(RecvError::Closed) => return,
            },
            r = ticks.changed() => {
                if r.is_err() {
                    return;
                }
                let now = *ticks.borrow_and_update();
                wake_all(&sup, now);
            }
        }
    }
}

fn wake_all(sup: &Supervisor, tick: Option<Instant>) {
    for sl in sup.send.all() {
        if let Some(t) = tick {
            let mut cur = lock(&sl.tick);
            *cur = Some(cur.map_or(t, |c| c.max(t)));
        }
        sl.wake.notify_one();
    }
}

/// One launch's worker: one pass at a time, until its launch left the live map and its
/// writer fired every completion, which were all recorded.
pub(crate) async fn worker(sup: Arc<Supervisor>, sl: Arc<SendLaunch>) {
    loop {
        pass(&sup, &sl).await;
        sl.passes.fetch_add(1, Ordering::SeqCst);
        sl.passed.notify_waiters();
        let done = sup.entry(sl.launch_id).is_none()
            && sl.outstanding.load(Ordering::Acquire) == 0
            && lock(&sl.fired).is_empty()
            && sl.state().held.is_empty();
        if done {
            lock(&sup.send.map).remove(&sl.launch_id);
            return;
        }
        sl.wake.notified().await;
    }
}

/// What one read gives a pass.
struct View {
    thread: ThreadRow,
    launch: LaunchRow,
    /// The launch's inputs not yet settled, in creation order.
    inputs: Vec<InputRow>,
}

/// A read on a blocking task, bounded by now + 1 s; `None` when not in time.
async fn read<R: Send + 'static>(
    sup: &Supervisor,
    f: impl FnOnce(&crate::store::ReadTx<'_>) -> Result<R, crate::store::StoreError> + Send + 'static,
) -> Option<R> {
    let deadline = sup.now() + READ_WAIT;
    let store = sup.store.clone();
    let real = sup.real(deadline);
    let task = sup.rt.spawn_blocking(move || store.read(real, f));
    match settle_before(|| sup.now(), deadline, sup.timer(deadline), task)
        .await
        .in_time()
    {
        Some(Ok(Ok(v))) => Some(v),
        _ => None,
    }
}

/// The instant a pass decides its timers at: the clock, or the latest tick when later.
fn pass_now(sup: &Supervisor, sl: &SendLaunch) -> Instant {
    let now = sup.now();
    lock(&sl.tick).map_or(now, |t| t.max(now))
}

pub(crate) async fn pass(sup: &Arc<Supervisor>, sl: &Arc<SendLaunch>) {
    apply_records(sup, sl).await;
    let Some(entry) = sup.live(sl.thread_id, sl.launch_id) else {
        return;
    };
    let (thread, launch) = (sl.thread_id, sl.launch_id);
    let Some(view) = read(sup, move |r| {
        let (Some(t), Some(l)) = (r.thread(thread)?, r.launch(launch)?) else {
            return Ok(None);
        };
        let inputs = r
            .inputs(
                thread,
                &[
                    InputState::Queued,
                    InputState::Sending,
                    InputState::Sent,
                    InputState::Unconfirmed,
                ],
            )?
            .into_iter()
            .filter(|i| i.launch_id == Some(launch))
            .collect();
        Ok(Some(View {
            thread: t,
            launch: l,
            inputs,
        }))
    })
    .await
    .flatten() else {
        return;
    };
    if view.thread.current_launch_id != Some(launch) || view.launch.exit_reason.is_some() {
        return;
    }
    let now = pass_now(sup, sl);
    running_unit(sup, sl, now).await;
    gate_timeouts(sup, sl, &entry, &view, now).await;
    ack_windows(sup, sl, &entry, &view, now).await;
    settle(sup, sl, &view, now).await;
    open(sup, sl, &entry, &view).await;
}

// ---- the running unit -----------------------------------------------------------------------

/// The dispatch wait and the write stall (send §2.3 step 5).
async fn running_unit(sup: &Arc<Supervisor>, sl: &Arc<SendLaunch>, now: Instant) {
    let stalled = {
        let st = sl.state();
        let Some(r) = st.running.as_ref() else {
            return;
        };
        let Some(ticket) = r.ticket.as_ref() else {
            return;
        };
        match ticket.started_at() {
            None => {
                if now >= r.submitted_at + DISPATCH_WAIT + READ_WAIT {
                    // Never waits for the queue lock: `None` (held) tries again next pass;
                    // `Some(true)` fires the unit's `NotStarted` completion.
                    let _ = ticket.try_cancel();
                }
                None
            }
            Some(started) if !r.stalled && sup.real(now) >= started + WRITE_STALL => {
                Some(r.input_id)
            }
            Some(_) => None,
        }
    };
    let Some(id) = stalled else {
        return;
    };
    let (thread, launch) = (sl.thread_id, sl.launch_id);
    let cause = IdempotencyKey(format!("stall:{launch}:{id}"));
    let job: JobOf<bool> = Arc::new(move |tx| {
        move_input(
            tx,
            thread,
            launch,
            id,
            InputState::Sending,
            &InputInput::WriteOutcomeUnknown,
            &cause,
        )
    });
    if let Recorded::Done(_) = record(sup, job, false).await {
        let mut st = sl.state();
        if let Some(r) = st.running.as_mut().filter(|r| r.input_id == id) {
            r.stalled = true;
        }
    }
}

// ---- timers ---------------------------------------------------------------------------------

/// `input_gate_timeout` once per queued input, `GATE_TIMEOUT` after the worker first read it
/// as queued, with a VT snapshot when one is taken in time (send §2.7).
async fn gate_timeouts(
    sup: &Arc<Supervisor>,
    sl: &Arc<SendLaunch>,
    entry: &LiveLaunch,
    view: &View,
    now: Instant,
) {
    let observed = sup.now();
    let due: Vec<InputId> = {
        let mut st = sl.state();
        let queued: Vec<InputId> = view
            .inputs
            .iter()
            .filter(|i| i.state == InputState::Queued)
            .map(|i| i.input_id)
            .collect();
        st.queued_at.retain(|id, _| queued.contains(id));
        queued
            .into_iter()
            .filter(|id| {
                let at = *st.queued_at.entry(*id).or_insert(observed);
                now >= at + GATE_TIMEOUT && !st.timed_out.contains(id)
            })
            .collect()
    };
    for id in due {
        let snap = entry
            .snapshot_before(sup, sup.now() + SAMPLE_WAIT)
            .await
            .ok()
            .map(|s| s.to_payload());
        let (thread, launch) = (sl.thread_id, sl.launch_id);
        let job: JobOf<()> = Arc::new(move |tx| {
            let r = tx.read();
            let Some(row) = r.input(id)? else {
                return Ok(());
            };
            let t = thread_row(tx, thread)?;
            let cause = AttentionCause::InputGateTimeout { input_id: id };
            if row.state != InputState::Queued || t.state.attention.contains(&cause) {
                return Ok(());
            }
            let key = IdempotencyKey(format!("gate_timeout:{launch}:{id}"));
            let mut job = ThreadJob::new(thread, t.agent, t.state, SourceKind::Daemon, None);
            if let Some(mut v) = snap.clone() {
                let grid = match &v.grid {
                    chaldea_core::payload::Content::Inline { inline, .. } => inline.clone(),
                    chaldea_core::payload::Content::Blob { preview, .. } => preview.clone(),
                };
                let prepared = crate::store::prepare_content(grid.into_bytes(), "application/json")
                    .map_err(super::super::events::refusal)?;
                v.grid = prepared.content;
                job.push_blobs(
                    derived(&key, "snapshot"),
                    TypedEvent::VtSnapshotTaken(v),
                    Some(launch),
                    prepared.blob.into_iter().collect(),
                );
            }
            job.apply(
                &key,
                "raise",
                &StateInput::AttentionRaised(cause),
                Some(launch),
            )?;
            job.flush(tx)?;
            Ok(())
        });
        if let Recorded::Done(()) = record(sup, job, false).await {
            sl.state().timed_out.insert(id);
        }
    }
}

/// `AckWindowExpired` for a `sent` input `ACK_WINDOW` after its completion's commit; for the
/// positional input, from the committed start and only while no startup cause is active.
async fn ack_windows(
    sup: &Arc<Supervisor>,
    sl: &Arc<SendLaunch>,
    entry: &LiveLaunch,
    view: &View,
    now: Instant,
) {
    let startup = view
        .thread
        .state
        .attention
        .contains(&AttentionCause::SessionStartTimeout)
        || view
            .thread
            .state
            .attention
            .contains(&AttentionCause::StartupStall);
    let observed = sup.now();
    let due: Vec<InputId> = {
        let mut st = sl.state();
        view.inputs
            .iter()
            .filter(|i| i.state == InputState::Sent)
            .filter_map(|i| {
                let id = i.input_id;
                let start = if sl.positional == Some(id) {
                    if startup {
                        return None;
                    }
                    entry.committed().map(|c| c.at)?
                } else {
                    *st.sent_at.entry(id).or_insert(observed)
                };
                (now >= start + ACK_WINDOW).then_some(id)
            })
            .collect()
    };
    for id in due {
        let (thread, launch) = (sl.thread_id, sl.launch_id);
        let cause = IdempotencyKey(format!("ack_window:{launch}:{id}"));
        let job: JobOf<bool> = Arc::new(move |tx| {
            move_input(
                tx,
                thread,
                launch,
                id,
                InputState::Sent,
                &InputInput::AckWindowExpired,
                &cause,
            )
        });
        let _ = record(sup, job, false).await;
    }
}

// ---- settle ---------------------------------------------------------------------------------

/// A settle completion for the log's candidate `SETTLE` after its commit (send §2.4).
async fn settle(sup: &Arc<Supervisor>, sl: &Arc<SendLaunch>, view: &View, now: Instant) {
    let word = sl.fence.load(Ordering::Acquire);
    if !fence_quiet(word) {
        return;
    }
    let latest = sl.latest_intake.load(Ordering::SeqCst);
    let candidate = {
        let st = sl.state();
        if st.running.is_some() {
            return;
        }
        st.log.candidate(latest)
    };
    let Some((ordinal, at)) = candidate else {
        return;
    };
    if now < at + SETTLE || view.thread.state.turn != TurnState::Unknown {
        return;
    }
    let mode = sup.send.mode(sl.caps);
    if !chaldea_core::adapter::evidence_allowed(sl.agent, mode, ReadinessEvidence::StopSettled) {
        return;
    }
    let position = match sup.observe(sl.launch_id) {
        LaunchObservation::Started { last_started, .. } => {
            last_started.iter().copied().max().unwrap_or(0)
        }
        LaunchObservation::Reserved | LaunchObservation::Unobservable => return,
    };
    let me = sl.clone();
    let (thread, launch) = (sl.thread_id, sl.launch_id);
    let job: JobOf<()> = Arc::new(move |tx| {
        #[cfg(test)]
        super::run_hook(&me.hooks.settle_job);
        if me.fence.load(Ordering::Acquire) != word || me.running_flag.load(Ordering::SeqCst) {
            return Ok(());
        }
        let r = tx.read();
        let (Some(t), Some(l)) = (r.thread(thread)?, r.launch(launch)?) else {
            return Ok(());
        };
        if t.current_launch_id != Some(launch)
            || t.closed
            || l.session_id.is_none()
            || l.exit_reason.is_some()
            || t.state.process != ProcessState::Running
            || t.state.turn != TurnState::Unknown
            || r.open_approvals(thread)?
                .iter()
                .any(|c| c.launch_id == launch)
        {
            return Ok(());
        }
        let Some(s_seq) = r.delivery_seq(launch, ordinal)? else {
            return Ok(());
        };
        if l.readiness.is_some_and(|m| m.seq > s_seq) {
            return Ok(());
        }
        let key = IdempotencyKey(format!("settle:{launch}:{ordinal}"));
        readiness_in(
            tx,
            &t,
            launch,
            &key,
            ReadinessEvidence::StopSettled,
            position,
            None,
        )
    });
    let _ = record(sup, job, false).await;
}

/// `readiness_observed` with its write basis and the `status_changed` the reducer returns;
/// `command` names the command that recorded it (`ready`, by the human), else the daemon did.
///
/// # Errors
///
/// A store error or a rule refusal: the job rolls back.
pub(crate) fn readiness_in(
    tx: &mut crate::store::WriteTx<'_>,
    t: &ThreadRow,
    launch: chaldea_core::ids::LaunchId,
    key: &IdempotencyKey,
    kind: ReadinessEvidence,
    position: u64,
    command: Option<chaldea_core::ids::CommandId>,
) -> Result<(), crate::store::JobError> {
    let (source, by) = if command.is_some() {
        (SourceKind::Command, chaldea_core::ids::Actor::Human)
    } else {
        (SourceKind::Daemon, chaldea_core::ids::Actor::Daemon)
    };
    let mut job = ThreadJob::new(t.thread_id, t.agent, t.state.clone(), source, command);
    job.push_annex(
        key.clone(),
        TypedEvent::ReadinessObserved(chaldea_core::payload::ReadinessObserved {
            launch_id: launch,
            evidence: kind,
            by,
        }),
        Some(launch),
        EventAnnex::Readiness(crate::store::WriteBasis {
            launch_id: launch,
            position,
            counted: CountedClasses::ALL,
        }),
    );
    job.apply(
        key,
        "readiness",
        &StateInput::ReadinessObserved,
        Some(launch),
    )?;
    job.flush(tx)?;
    Ok(())
}

// ---- open and submit ------------------------------------------------------------------------

/// Why the open job left the input `queued`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Closed {
    /// A hook intake is unfinished, or the fence word is closed.
    Fence,
    Gate(GateClosedReason),
    /// The input is not the launch's oldest queued one, or the launch is not current.
    Moved,
}

/// What the open job committed.
#[derive(Debug, Clone)]
pub(crate) struct Opened {
    pub basis: u64,
    pub epoch: u64,
    pub word: u64,
    pub competing: bool,
    pub needs_paste: bool,
    pub text: String,
}

/// The classes of a stored basis.
fn classes(c: CountedClasses) -> ClassSet {
    let all: Vec<OriginClass> = c
        .iter()
        .map(|k| match k {
            CountedClass::Input => OriginClass::Input,
            CountedClass::Human => OriginClass::Human,
            CountedClass::Reply => OriginClass::Reply,
        })
        .collect();
    ClassSet::of(&all)
}

/// The open job (send §2.3 step 1).
fn open_job(
    sl: &Arc<SendLaunch>,
    input: InputId,
    modes: ModeView,
    last_started: [u64; 3],
) -> JobOf<Result<Opened, Closed>> {
    let me = sl.clone();
    let (thread, launch) = (sl.thread_id, sl.launch_id);
    Arc::new(move |tx| {
        #[cfg(test)]
        super::run_hook(&me.hooks.open_job);
        let word = me.fence.load(Ordering::Acquire);
        if !fence_quiet(word) {
            return Ok(Err(Closed::Fence));
        }
        if me.running_flag.load(Ordering::SeqCst) {
            return Ok(Err(Closed::Moved));
        }
        let r = tx.read();
        let (Some(t), Some(l), Some(q)) = (r.thread(thread)?, r.launch(launch)?, r.input(input)?)
        else {
            return Ok(Err(Closed::Moved));
        };
        if t.closed
            || t.current_launch_id != Some(launch)
            || l.exit_reason.is_some()
            || q.state != InputState::Queued
            || q.launch_id != Some(launch)
        {
            return Ok(Err(Closed::Moved));
        }
        let live = r.inputs(
            thread,
            &[
                InputState::Queued,
                InputState::Sending,
                InputState::Sent,
                InputState::Unconfirmed,
            ],
        )?;
        let oldest = live
            .iter()
            .find(|i| i.state == InputState::Queued && i.launch_id == Some(launch))
            .map(|i| i.input_id);
        if oldest != Some(input) {
            return Ok(Err(Closed::Moved));
        }
        let needs_paste = q.encoding == InputEncoding::BracketedPaste;
        let view = GateView {
            bound: l.session_id.is_some(),
            process: t.state.process,
            turn: t.state.turn,
            readiness: l.readiness,
            last_write: l.last_write_seq,
            last_human_write: l.last_human_write_seq,
            last_turn_start: l.last_turn_start_seq,
            in_flight: live
                .iter()
                .any(|i| blocks_gate(i.launch_id, i.state, launch)),
            needs_bracketed_paste: needs_paste,
            bracketed_paste_on: modes.modes.bracketed_paste,
        };
        if let GateDecision::Closed(why) = gate(&view) {
            return Ok(Err(Closed::Gate(why)));
        }
        // SB3: a counted write after the evidence's basis with no event of its own.
        let Some(basis) = l.write_basis.as_ref() else {
            return Ok(Err(Closed::Gate(GateClosedReason::NoEvidence)));
        };
        let counted = classes(basis.counted);
        let last = counted
            .iter()
            .map(|c| last_started[c.index()])
            .max()
            .unwrap_or(0);
        if last > basis.position {
            return Ok(Err(Closed::Gate(GateClosedReason::WriteAfterEvidence)));
        }
        let text = content_text(&r, StreamId::Thread(thread), &q.text)?;
        let head = r.head(StreamId::Thread(thread), 0)?.head_seq;
        let competing = match (l.last_human_write_seq, l.last_turn_start_seq) {
            (Some(h), Some(s)) => h > s,
            (Some(_), None) => true,
            _ => false,
        };
        let cause = IdempotencyKey(format!("open:{launch}:{input}"));
        let mut job = ThreadJob::new(thread, t.agent, t.state, SourceKind::Daemon, None);
        job.step_input(
            &cause,
            &q,
            &InputInput::GateOpened {
                sent_after_seq: head,
            },
            Some(head),
            Some(launch),
        )?;
        let appended = job.flush(tx)?;
        let _attempt = seq_of(&appended, &derived(&cause, &format!("input:{input}")))?;
        Ok(Ok(Opened {
            basis: basis.position,
            epoch: modes.bracketed_paste_epoch,
            word,
            competing,
            needs_paste,
            text,
        }))
    })
}

/// The open and the submit of the launch's oldest queued input, when no unit runs.
async fn open(sup: &Arc<Supervisor>, sl: &Arc<SendLaunch>, entry: &LiveLaunch, view: &View) {
    {
        let st = sl.state();
        if st.running.is_some() || st.human_unsettled > 0 {
            return;
        }
    }
    if !fence_quiet(sl.fence.load(Ordering::Acquire)) {
        return;
    }
    let Some(q) = view
        .inputs
        .iter()
        .find(|i| i.state == InputState::Queued)
        .map(|i| i.input_id)
    else {
        return;
    };
    let Ok((modes, last_started)) = entry
        .with_session(sup, sup.now() + SAMPLE_WAIT, |s| {
            (s.modes(), s.observer().last_started_each())
        })
        .await
    else {
        return;
    };
    let opened = match record(sup, open_job(sl, q, modes, last_started), false).await {
        Recorded::Done(Ok(o)) => o,
        Recorded::Done(Err(_)) => return,
        Recorded::Refused(e) => {
            tracing::error!(error = ?e, "an open job refused");
            return;
        }
        Recorded::Unrecorded | Recorded::ShuttingDown => return,
    };
    submit(sup, sl, entry, q, opened).await;
}

/// A refused submission: zero bytes are certain, so the input is cancelled through the
/// completion job (a synthetic `NotStarted` record).
fn refuse(sl: &SendLaunch, input: InputId) {
    lock(&sl.fired).push(Fired {
        record: WriteRecord {
            seq: 0,
            origin: WriteOrigin::Input { input_id: input },
            submitted_at: Instant::now(),
            started_at: None,
            finished_at: None,
            byte_start: 0,
            bytes_written: 0,
            split_writes: 0,
            outcome: WriteOutcome::NotStarted(chaldea_pty_runtime::Refusal::Closed),
        },
        report: None,
    });
    sl.wake.notify_one();
}

/// Step 2: the unit, its guard, and its submission inside `with_session`; `running` and the
/// ack facts are set before `submit`.
async fn submit(
    sup: &Arc<Supervisor>,
    sl: &Arc<SendLaunch>,
    entry: &LiveLaunch,
    input: InputId,
    o: Opened,
) {
    #[cfg(test)]
    super::run_hook(&sl.hooks.before_submit);
    let Ok(enc) = encode(sl.agent, sup.host_os, &o.text, Purpose::Send) else {
        refuse(sl, input);
        return;
    };
    let Some(sub) = enc.submit else {
        refuse(sl, input);
        return;
    };
    let unit = WriteUnit {
        origin: WriteOrigin::Input { input_id: input },
        parts: vec![
            Part::Bytes(enc.text),
            Part::Pause(sub.delay),
            Part::Bytes(sub.bytes.to_vec()),
        ],
    };
    let now = sup.now();
    let guard = Guard {
        not_after: sup.real(now + DISPATCH_WAIT),
        basis: Some(Basis {
            seq: o.basis,
            counted: ClassSet::ALL,
        }),
        bracketed_paste: o.needs_paste.then_some(o.epoch),
        recheck: None,
        fence: Some(Fence {
            word: sl.fence.clone(),
            expect: o.word,
        }),
    };
    let me = sl.clone();
    let competing = o.competing;
    let submitted = entry
        .with_session(sup, now + READ_WAIT, move |s| {
            let before = s.observer().last_started_each()[OriginClass::Input.index()];
            let written = Arc::new(AtomicBool::new(false));
            {
                let mut st = me.state();
                st.dispatch = Some(Dispatch {
                    input_id: input,
                    before,
                    written: written.clone(),
                    competing,
                });
                me.set_running(
                    &mut st,
                    Some(RunningUnit {
                        input_id: input,
                        ticket: None,
                        submitted_at: now,
                        stalled: false,
                    }),
                );
            }
            me.outstanding.fetch_add(1, Ordering::AcqRel);
            let done = me.completion(Some(written), None);
            let r = s.submit(unit, guard, done);
            #[cfg(test)]
            super::run_hook(&me.hooks.after_submit);
            let mut st = me.state();
            match r {
                Ok(t) => {
                    if let Some(run) = st.running.as_mut().filter(|r| r.input_id == input) {
                        run.ticket = Some(t);
                    }
                    true
                }
                Err(_) => {
                    me.outstanding.fetch_sub(1, Ordering::AcqRel);
                    me.set_running(&mut st, None);
                    false
                }
            }
        })
        .await;
    if !matches!(submitted, Ok(true)) {
        refuse(sl, input);
    }
}
