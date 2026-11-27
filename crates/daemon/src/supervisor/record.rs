//! `supervisor::record`: every job after the runner's intent (design §2.1 rule 1, §2.10
//! "Store jobs"). Attempts of `COMPLETION_ATTEMPT` through `write_with`, retried only on
//! `retryable` outcomes with the runner's backoff, admitted for `COMPLETION_BUDGET`; an
//! admitted attempt is awaited to the writer's answer.

use std::sync::Arc;

use crate::http::{
    BACKOFF_FIRST, BACKOFF_MAX, COMPLETION_ATTEMPT, COMPLETION_BUDGET, WriteFailure, backoff_until,
    retryable, write_with,
};
use crate::store::{JobError, SubmitError, WriteError, WriteTx};

use super::Supervisor;

/// One job, run again on each attempt.
pub(crate) type Job = Arc<dyn Fn(&mut WriteTx<'_>) -> Result<(), JobError> + Send + Sync>;

/// A job returning a value, run again on each attempt.
pub(crate) type JobOf<R> = Arc<dyn Fn(&mut WriteTx<'_>) -> Result<R, JobError> + Send + Sync>;

/// How a recorded job ended.
#[derive(Debug)]
pub(crate) enum Recorded<R> {
    /// Committed.
    Done(R),
    /// The job refused (rolled back): a value, not a failure.
    Refused(JobError),
    /// Not recorded within the budget, or a fatal outcome `write_with` reported, while the
    /// daemon runs; whether the job may have committed.
    Unrecorded(Commit),
    /// Not recorded once admission closed, or the store refused it as `Closed`.
    ShuttingDown,
}

/// Whether an unrecorded job may have committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Commit {
    /// Known not committed: every attempt was refused before it ran, or rolled back.
    NotCommitted,
    /// The last attempt's `COMMIT` result is not known (`CommitUnknown`, `WriterGone`, or a
    /// job left queued by `WouldDeadlock`).
    Unknown,
}

/// The commit a non-retryable failure leaves.
fn commit_of(f: &WriteFailure) -> Commit {
    match f {
        WriteFailure::Write(
            WriteError::CommitUnknown(_) | WriteError::WriterGone | WriteError::WouldDeadlock,
        ) => Commit::Unknown,
        _ => Commit::NotCommitted,
    }
}

/// Runs `job` by the completion policy. `nudge`: the job ends a token's validity, so the
/// revocation nudge runs once it committed.
pub(crate) async fn record<R: Send + 'static>(
    sup: &Arc<Supervisor>,
    job: JobOf<R>,
    nudge: bool,
) -> Recorded<R> {
    let budget_end = sup.now() + COMPLETION_BUDGET;
    let mut backoff = BACKOFF_FIRST;
    loop {
        let now = sup.now();
        if now >= budget_end {
            return sup.unrecorded(Commit::NotCommitted);
        }
        let not_after = (now + COMPLETION_ATTEMPT).min(budget_end);
        let j = job.clone();
        let after_ok: Option<crate::http::AfterOk> = nudge.then(|| {
            let s = sup.clone();
            Box::new(move || s.nudge()) as crate::http::AfterOk
        });
        let r = write_with(
            &sup.store,
            sup.sink.as_ref(),
            sup.clock(),
            not_after,
            Box::new(move |tx| j(tx)),
            after_ok,
            || {},
        )
        .await;
        match r {
            Ok(v) => return Recorded::Done(v),
            Err(WriteFailure::Write(WriteError::Rejected(e))) => return Recorded::Refused(e),
            Err(WriteFailure::Submit(SubmitError::Closed)) => return Recorded::ShuttingDown,
            Err(f) if retryable(&f) => {}
            Err(f) => {
                tracing::warn!(failure = ?f, "a supervisor job was not recorded");
                return sup.unrecorded(commit_of(&f));
            }
        }
        let from = sup.now();
        let wake = (from + backoff).min(budget_end);
        backoff_to(sup, wake).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// How often a test build re-reads the supervisor's clock during a backoff.
#[cfg(test)]
const RECHECK: std::time::Duration = std::time::Duration::from_millis(5);

/// Waits until the supervisor's clock reads `wake`, its timer recomputed from the clock on
/// every iteration. On a real clock that is one sleep; a test build also re-reads the clock
/// every [`RECHECK`], so a stopped test clock moved past `wake` ends the wait.
async fn backoff_to(sup: &Supervisor, wake: std::time::Instant) {
    #[cfg(test)]
    sup.pauses().at(super::Pause::RecordBackoff).await;
    while sup.now() < wake {
        let at = sup.timer(wake);
        #[cfg(test)]
        let at = at.min(tokio::time::Instant::now() + RECHECK);
        backoff_until(at).await;
    }
}

impl Supervisor {
    /// `Unrecorded` while admission is open, `ShuttingDown` once it closed.
    pub(crate) fn unrecorded<R>(&self, commit: Commit) -> Recorded<R> {
        if self.tracker.accepting() {
            Recorded::Unrecorded(commit)
        } else {
            Recorded::ShuttingDown
        }
    }
}
