//! The worker's record jobs (send §2.3 step 4, §2.6 "Human units"): each input unit's
//! completion applied to its input, each human unit's `human_write`, in `record.seq` order.

use std::sync::Arc;

use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{IdempotencyKey, InputId, LaunchId, StreamId, ThreadId};
use chaldea_core::input::{InputInput, InputState};
use chaldea_core::payload::{HumanWrite, SourceKind};
use chaldea_pty_runtime::{WriteOrigin, WriteOutcome, WriteRecord};

use super::super::Supervisor;
use super::super::events::{ThreadJob, thread_row};
use super::super::record::{JobOf, Recorded, record};
use super::{Fired, KeyReport, SendLaunch};
use crate::store::{CommitStamp, JobError, WriteTx};

/// `derived:human_write:<launch_id>:<seq>`: a retried job finds its event.
pub(crate) fn human_key(l: LaunchId, seq: u64) -> IdempotencyKey {
    IdempotencyKey(format!("derived:human_write:{l}:{seq}"))
}

/// The cause key of an input's moves from one write record.
fn write_key(l: LaunchId, seq: u64, input: InputId) -> IdempotencyKey {
    IdempotencyKey(format!("write:{l}:{seq}:{input}"))
}

/// Moves `input` by `by` in its own job, keyed under `cause`, when it is still in `from` and
/// its launch has no exit; clears the cause an input leaves with its state. `Ok(true)`: moved.
///
/// # Errors
///
/// A store error or a rule refusal: the job rolls back.
pub(crate) fn move_input(
    tx: &mut WriteTx<'_>,
    thread: ThreadId,
    launch: LaunchId,
    input: InputId,
    from: InputState,
    by: &InputInput,
    cause: &IdempotencyKey,
) -> Result<bool, JobError> {
    let r = tx.read();
    let (Some(row), Some(l)) = (r.input(input)?, r.launch(launch)?) else {
        return Ok(false);
    };
    if row.state != from || l.exit_reason.is_some() {
        return Ok(false);
    }
    let t = thread_row(tx, thread)?;
    let mut job = ThreadJob::new(thread, t.agent, t.state, SourceKind::Daemon, None);
    job.step_input(cause, &row, by, None, Some(launch))?;
    job.flush(tx)?;
    Ok(true)
}

/// The completion job of an input unit's record: `Written` → `sent`, `NotStarted` →
/// `cancelled`, `Incomplete` → `unconfirmed`, only for an input still `sending` of a launch
/// with no exit; with the clock right after the job's SQLite `COMMIT` (the instant the ack
/// window starts from).
fn completion_job(
    sup: &Supervisor,
    sl: &SendLaunch,
    rec: &WriteRecord,
    input: InputId,
) -> JobOf<(bool, CommitStamp)> {
    let (thread, launch) = (sl.thread_id, sl.launch_id);
    let by = match &rec.outcome {
        WriteOutcome::Written => InputInput::WriteCompleted,
        WriteOutcome::NotStarted(_) => InputInput::RecheckFailed,
        WriteOutcome::Incomplete { .. } | WriteOutcome::InProgress => {
            InputInput::WriteOutcomeUnknown
        }
    };
    let cause = write_key(launch, rec.seq, input);
    let clock = sup.cfg.clock.clone();
    Arc::new(move |tx| {
        let moved = move_input(tx, thread, launch, input, InputState::Sending, &by, &cause)?;
        let clock = clock.clone();
        Ok((moved, tx.commit_stamp(move || clock.now())))
    })
}

/// Appends `rec`'s `human_write` once: for `Written` or `Incomplete`, nothing for
/// `NotStarted`.
///
/// # Errors
///
/// A store error or a rule refusal: the job rolls back.
pub(crate) fn human_write_in(
    tx: &mut WriteTx<'_>,
    thread: ThreadId,
    launch: LaunchId,
    rec: &WriteRecord,
) -> Result<(), JobError> {
    let Some(origin) = rec.origin.human_write() else {
        return Ok(());
    };
    if matches!(rec.outcome, WriteOutcome::NotStarted(_)) {
        return Ok(());
    }
    let key = human_key(launch, rec.seq);
    if tx
        .read()
        .event_by_key(StreamId::Thread(thread), &key)?
        .is_some()
    {
        return Ok(());
    }
    let approval_id = match rec.origin {
        WriteOrigin::CardKey { approval_id } => Some(approval_id),
        _ => None,
    };
    let t = thread_row(tx, thread)?;
    let mut job = ThreadJob::new(thread, t.agent, t.state, SourceKind::Daemon, None);
    job.push(
        key,
        TypedEvent::HumanWrite(HumanWrite {
            launch_id: launch,
            origin,
            approval_id,
        }),
        Some(launch),
    );
    job.flush(tx)?;
    Ok(())
}

/// The report a key's effect gets for its unit's record.
pub(crate) fn report_of(rec: &WriteRecord) -> KeyReport {
    match &rec.outcome {
        WriteOutcome::NotStarted(_) => KeyReport::NotStarted,
        WriteOutcome::Written => KeyReport::Recorded {
            complete: true,
            bytes: rec.bytes_written,
        },
        _ => KeyReport::Recorded {
            complete: false,
            bytes: rec.bytes_written,
        },
    }
}

/// Takes the fired completions: input records are applied now, human records join `held`
/// and are recorded in `seq` order. A job not recorded keeps its record (and every later
/// human record) for the next pass.
pub(crate) async fn apply_records(sup: &Arc<Supervisor>, sl: &Arc<SendLaunch>) {
    let fired: Vec<Fired> = std::mem::take(&mut *super::lock(&sl.fired));
    let mut inputs = Vec::new();
    {
        let mut st = sl.state();
        for f in fired {
            match f.record.origin {
                WriteOrigin::Input { input_id } => inputs.push((f, input_id)),
                _ => {
                    let at = st.held.partition_point(|h| h.record.seq < f.record.seq);
                    st.held.insert(at, f);
                }
            }
        }
    }
    let mut kept = Vec::new();
    for (f, id) in inputs {
        if !kept.is_empty() {
            kept.push(f);
            continue;
        }
        #[cfg(test)]
        super::run_hook(&sl.hooks.record_job);
        match record(sup, completion_job(sup, sl, &f.record, id), false).await {
            Recorded::Done((moved, at)) => {
                #[cfg(test)]
                super::run_hook(&sl.hooks.after_record);
                let mut st = sl.state();
                if st.running.as_ref().is_some_and(|r| r.input_id == id) {
                    sl.set_running(&mut st, None);
                }
                if moved && f.record.outcome == WriteOutcome::Written {
                    // A committed job's stamp is filled before its reply.
                    st.sent_at.insert(id, at.get().unwrap_or_else(|| sup.now()));
                }
            }
            Recorded::Refused(e) => {
                tracing::error!(error = ?e, "a completion job refused");
                let mut st = sl.state();
                if st.running.as_ref().is_some_and(|r| r.input_id == id) {
                    sl.set_running(&mut st, None);
                }
            }
            Recorded::Unrecorded | Recorded::ShuttingDown => kept.push(f),
        }
    }
    if !kept.is_empty() {
        let mut back = super::lock(&sl.fired);
        kept.append(&mut back);
        *back = kept;
    }
    record_humans(sup, sl).await;
}

/// Records `held` in `seq` order until one job is not recorded.
async fn record_humans(sup: &Arc<Supervisor>, sl: &Arc<SendLaunch>) {
    loop {
        let rec = {
            let st = sl.state();
            match st.held.first() {
                Some(f) => f.record.clone(),
                None => return,
            }
        };
        #[cfg(test)]
        super::run_hook(&sl.hooks.record_job);
        let (thread, launch) = (sl.thread_id, sl.launch_id);
        let r = rec.clone();
        let job: JobOf<()> = Arc::new(move |tx| human_write_in(tx, thread, launch, &r));
        match record(sup, job, false).await {
            Recorded::Done(()) => settle_human(sl, rec.seq, report_of(&rec)),
            Recorded::Refused(e) => {
                tracing::error!(error = ?e, "a human_write job refused");
                return;
            }
            Recorded::Unrecorded | Recorded::ShuttingDown => return,
        }
    }
}

/// A human record is recorded: it leaves `held` (once), the count goes down, and a waiting
/// key effect gets its report.
pub(crate) fn settle_human(sl: &SendLaunch, seq: u64, report: KeyReport) {
    let mut st = sl.state();
    let Some(i) = st.held.iter().position(|f| f.record.seq == seq) else {
        return;
    };
    let f = st.held.remove(i);
    sl.human_down(&mut st);
    drop(st);
    if let Some(tx) = f.report {
        let _ = tx.send(report);
    }
}
