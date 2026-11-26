//! `SendExitStep` (send §2.3 "After exit"): sealed input units cancelled, sealed and held
//! human records recorded, and the launch's open cards expired, in the exit job.

use std::sync::Arc;

use chaldea_core::event::TypedEvent;
use chaldea_core::ids::IdempotencyKey;
use chaldea_core::input::{InputInput, InputState};
use chaldea_core::payload::{ApprovalOutcome, ApprovalResolved, SourceKind};
use chaldea_pty_runtime::{WriteOrigin, WriteRecord};

use super::super::events::{ThreadJob, thread_row};
use super::super::exit::{ExitFacts, ExitStep};
use super::completion::{human_write_in, report_of, settle_human};
use super::{SendSide, lock};
use crate::store::{JobError, StoreError, WriteTx};

pub(crate) struct SendExitStep(pub(crate) Arc<SendSide>);

fn store_err(e: JobError) -> StoreError {
    match e {
        JobError::Store(s) => s,
        other @ JobError::Refused(_) => StoreError::Decode(format!("{other:?}")),
    }
}

impl SendExitStep {
    /// The human records the worker holds unrecorded, copied at the exit job's first attempt
    /// and the same on every retry.
    fn exit_set(&self, exit: &ExitFacts) -> Vec<WriteRecord> {
        let Some(sl) = self.0.launch(exit.launch_id) else {
            return Vec::new();
        };
        let fired: Vec<super::Fired> = std::mem::take(&mut *lock(&sl.fired));
        let mut st = sl.state();
        let mut inputs = Vec::new();
        for f in fired {
            if matches!(f.record.origin, WriteOrigin::Input { .. }) {
                inputs.push(f);
            } else {
                let at = st.held.partition_point(|h| h.record.seq < f.record.seq);
                st.held.insert(at, f);
            }
        }
        lock(&sl.fired).extend(inputs);
        if st.exit_set.is_none() {
            st.exit_set = Some(st.held.iter().map(|f| f.record.clone()).collect());
        }
        st.exit_set.clone().unwrap_or_default()
    }
}

impl ExitStep for SendExitStep {
    fn in_exit_job(&self, tx: &mut WriteTx<'_>, exit: &ExitFacts) -> Result<(), StoreError> {
        let (thread, launch) = (exit.thread_id, exit.launch_id);
        // Sealed units wrote zero bytes.
        for rec in &exit.sealed {
            match rec.origin {
                WriteOrigin::Input { input_id } => {
                    sealed_input(tx, exit, input_id, rec).map_err(store_err)?;
                }
                _ => human_write_in(tx, thread, launch, rec).map_err(store_err)?,
            }
        }
        for rec in self.exit_set(exit) {
            human_write_in(tx, thread, launch, &rec).map_err(store_err)?;
        }
        // The launch's open cards expire.
        let cards: Vec<_> = tx
            .read()
            .open_approvals(thread)?
            .into_iter()
            .filter(|c| c.launch_id == launch)
            .collect();
        if !cards.is_empty() {
            let t = thread_row(tx, thread).map_err(store_err)?;
            let kind = if exit.restart {
                SourceKind::Daemon
            } else {
                SourceKind::Process
            };
            let mut job = ThreadJob::new(thread, t.agent, t.state, kind, None);
            for c in cards {
                job.push(
                    IdempotencyKey(format!("launch:{launch}:expired:{}", c.approval_id)),
                    TypedEvent::ApprovalResolved(ApprovalResolved {
                        approval_id: c.approval_id,
                        outcome: ApprovalOutcome::Expired,
                    }),
                    Some(launch),
                );
            }
            job.flush(tx)?;
        }
        Ok(())
    }

    fn after_commit(&self, exit: &ExitFacts) {
        let Some(sl) = self.0.launch(exit.launch_id) else {
            return;
        };
        let set = sl.state().exit_set.take().unwrap_or_default();
        for rec in set {
            settle_human(&sl, rec.seq, report_of(&rec));
        }
        sl.wake.notify_one();
    }
}

/// A sealed input unit's record, applied in the exit job after `launch_exited`: the input,
/// if still `sending`, is cancelled (zero bytes).
fn sealed_input(
    tx: &mut WriteTx<'_>,
    exit: &ExitFacts,
    input: chaldea_core::ids::InputId,
    rec: &WriteRecord,
) -> Result<(), JobError> {
    let Some(row) = tx.read().input(input)? else {
        return Ok(());
    };
    if row.state != InputState::Sending {
        return Ok(());
    }
    let t = thread_row(tx, exit.thread_id)?;
    let mut job = ThreadJob::new(exit.thread_id, t.agent, t.state, SourceKind::Process, None);
    let cause = IdempotencyKey(format!("sealed:{}:{}", exit.launch_id, rec.seq));
    job.step_input(
        &cause,
        &row,
        &InputInput::RecheckFailed,
        None,
        Some(exit.launch_id),
    )?;
    job.flush(tx)?;
    Ok(())
}
