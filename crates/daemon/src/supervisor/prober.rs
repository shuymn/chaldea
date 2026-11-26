//! The restart prober (`docs/design/supervisor-recovery.md` §2.3): after `attach`, each start
//! signals by verified identity the recorded leader of every unconfirmed `daemon_restart`
//! launch, read from the store, and records what it observed as `launch_evidence`. A probe
//! never confirms death and changes no state (§2.1 rule 1).
//!
//! Each probe's blocking task owns its concurrency slot and its tracker token
//! ([`ProbeAdmission`]) until it ends, by a return, an unwind, or the pool dropping it unrun:
//! exactly while it can signal (§2.1 rule 3).

use std::sync::Arc;

use chaldea_core::adapter::HostOs;
use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{LaunchId, ThreadId};
use chaldea_core::payload::{
    EvidenceSource, ExitReason, IdentityObservation, LaunchEvidence, Observed, SourceKind,
    Unchecked,
};
use chaldea_pty_runtime::ProcessIdentity;
use chaldea_pty_runtime::orphan::{Fate, OrphanReport};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::deadline::{Settled, settle_before};
use crate::store::LaunchRow;

use super::events::{ThreadJob, launch_key, thread_row};
use super::record::{JobOf, Recorded, record};
use super::tracker::TrackToken;
use super::{PROBE_BUDGET, PROBE_JOIN_GRACE, PROBE_READ, Pause, SLOT_WAIT, Supervisor};

/// A probe's tracker token, released when dropped: on the blocking task's return, on an unwind
/// out of `kill_recorded`, and when the pool drops the task unrun. In each the worker can no
/// longer signal, so this is the probe's own release rule (it owns no process or handle).
pub(crate) struct ProbeAdmission(Option<TrackToken>);

impl ProbeAdmission {
    pub(crate) fn new(token: TrackToken) -> ProbeAdmission {
        ProbeAdmission(Some(token))
    }
}

impl Drop for ProbeAdmission {
    fn drop(&mut self) {
        if let Some(t) = self.0.take() {
            t.release();
        }
    }
}

/// A launch the prober checks: exited `daemon_restart` by an earlier run, death unconfirmed,
/// a pid recorded.
fn candidate(l: &LaunchRow) -> bool {
    l.exit_reason == Some(ExitReason::DaemonRestart) && !l.death_confirmed && l.pid.is_some()
}

/// The prober (§2.3 steps 1–4): returns once every candidate's task is spawned.
pub(crate) async fn run(sup: Arc<Supervisor>) {
    // Step 1.
    sup.pauses().at(Pause::ProberStart).await;
    if !sup.tracker.accepting() {
        return;
    }
    // Step 2: one read transaction, bounded by `PROBE_READ`.
    let until = sup.now() + PROBE_READ;
    let store = sup.store.clone();
    let wait = sup.real(until);
    let read = sup.rt.spawn_blocking(move || {
        #[allow(
            clippy::redundant_closure_for_method_calls,
            reason = "the reader's lifetime is the closure's own"
        )]
        store.read(wait, |r| r.open_launches())
    });
    let rows = match settle_before(|| sup.now(), until, sup.timer(until), read)
        .await
        .in_time()
    {
        Some(Ok(Ok(rows))) => rows,
        other => {
            tracing::warn!(read = ?other.map(|r| r.map(|r| r.map(|v| v.len()))), "the restart prober read no candidates in time; the next start reads again");
            return;
        }
    };
    let candidates: Vec<LaunchRow> = rows.into_iter().filter(candidate).collect();
    // Step 3.
    for (i, l) in candidates.iter().enumerate() {
        // 3.1: a slot; a permit settled late is dropped at once.
        let by = sup.now() + SLOT_WAIT;
        let acquire = sup.probe_slots.clone().acquire_owned();
        let Some(Ok(permit)) = settle_before(|| sup.now(), by, sup.timer(by), acquire)
            .await
            .in_time()
        else {
            let left: Vec<LaunchId> = candidates[i..].iter().map(|l| l.launch_id).collect();
            tracing::warn!(
                ?left,
                "no probe slot in time; the next start probes these launches"
            );
            return;
        };
        // 3.2: admission, atomic with shutdown's close of it.
        let Some(token) = sup.tracker.admit() else {
            return;
        };
        let admission = ProbeAdmission::new(token);
        // 3.3.
        let identity = ProcessIdentity {
            pid: l.pid.unwrap_or_default(),
            start_time: l.start_time.clone().unwrap_or_default(),
            group: l.pgid_or_job.clone().unwrap_or_default(),
        };
        let attempt = Uuid::new_v4();
        let deadline = sup.now() + PROBE_BUDGET;
        // 3.4: the blocking task owns the permit and the admission.
        let s = sup.clone();
        let target = identity.clone();
        let join = sup.rt.spawn_blocking(move || {
            let _slot = permit;
            let _admission = admission;
            s.pauses().blocking(Pause::ProbeQueued);
            if !s.tracker.accepting() {
                return None;
            }
            let report = (s.cfg.kill_recorded)(std::slice::from_ref(&target), s.real(deadline));
            s.pauses().blocking(Pause::ProbeTask);
            Some(report)
        });
        // 3.5: a task of its own awaits the join and records the evidence.
        let probe = Probe {
            thread: l.thread_id,
            launch: l.launch_id,
            identity,
            attempt,
            deadline,
        };
        sup.rt.spawn(record_probe(sup.clone(), probe, join));
    }
}

/// One probe's facts for its record.
struct Probe {
    thread: ThreadId,
    launch: LaunchId,
    identity: ProcessIdentity,
    attempt: Uuid,
    deadline: std::time::Instant,
}

/// §2.3 step 3.5: the join awaited until `deadline + PROBE_JOIN_GRACE`; a late report is
/// still an observation; an elapsed join or a panic is `not_read` with `result_not_received`;
/// a task cancelled at shutdown before its OS work records nothing.
async fn record_probe(sup: Arc<Supervisor>, p: Probe, join: JoinHandle<Option<OrphanReport>>) {
    let cutoff = p.deadline + PROBE_JOIN_GRACE;
    let settled = settle_before(|| sup.now(), cutoff, sup.timer(cutoff), join).await;
    let (fates, received) = match settled {
        Settled::InTime(Ok(Some(r))) | Settled::Late(Ok(Some(r))) => (r.fates, true),
        Settled::InTime(Ok(None)) | Settled::Late(Ok(None)) => return,
        Settled::InTime(Err(e)) | Settled::Late(Err(e)) if e.is_cancelled() => return,
        Settled::InTime(Err(_)) | Settled::Late(Err(_)) | Settled::Elapsed => {
            (vec![(p.identity.clone(), Fate::NotRead)], false)
        }
    };
    let mut unchecked = vec![match sup.host_os {
        HostOs::Windows => Unchecked::UnrecordedJobAndOutsideJob,
        HostOs::Macos => Unchecked::UnrecordedSessionMembers,
    }];
    if !received {
        unchecked.push(Unchecked::ResultNotReceived);
    }
    let evidence = LaunchEvidence {
        launch_id: p.launch,
        source: EvidenceSource::Probe,
        identities: fates
            .into_iter()
            .map(|(id, f)| observation(&id, observed(f)))
            .collect(),
        unchecked,
        probe_run: Some(sup.instance.clone()),
    };
    let key = launch_key(p.launch, &format!("evidence:probe:{}", p.attempt));
    let thread = p.thread;
    let launch = p.launch;
    let job: JobOf<()> = Arc::new(move |tx| {
        let t = thread_row(tx, thread)?;
        let mut job = ThreadJob::new(thread, t.agent, t.state, SourceKind::Daemon, None);
        job.push(
            key.clone(),
            TypedEvent::LaunchEvidence(evidence.clone()),
            Some(launch),
        );
        job.flush(tx)?;
        #[cfg(test)]
        attempts::record(launch, &key, &evidence);
        Ok(())
    });
    match record(&sup, job, false).await {
        Recorded::Done(()) => {}
        Recorded::Refused(e) => {
            tracing::error!(%launch, error = ?e, "the probe's evidence job refused");
        }
        Recorded::Unrecorded | Recorded::ShuttingDown => {
            tracing::warn!(%launch, "the probe's evidence was not recorded; the next start probes again");
        }
    }
}

pub(crate) fn observed(f: Fate) -> Observed {
    match f {
        Fate::Gone => Observed::Gone,
        Fate::Replaced => Observed::Replaced,
        Fate::Alive => Observed::Alive,
        Fate::Unreadable => Observed::Unreadable,
        Fate::NotRead => Observed::NotRead,
        Fate::WaitFailed => Observed::WaitFailed,
        Fate::Invalid => Observed::Invalid,
    }
}

/// An identity as evidence records it: an empty start as `null`.
pub(crate) fn observation(id: &ProcessIdentity, observed: Observed) -> IdentityObservation {
    IdentityObservation {
        pid: id.pid,
        start_time: (!id.start_time.is_empty()).then(|| id.start_time.clone()),
        observed,
    }
}

/// Each evidence job attempt's key and payload, as the job appended them (unit tests).
#[cfg(test)]
pub(crate) mod attempts {
    use std::sync::Mutex;

    use chaldea_core::ids::{IdempotencyKey, LaunchId};
    use chaldea_core::payload::LaunchEvidence;

    static SEEN: Mutex<Vec<(LaunchId, String, String)>> = Mutex::new(Vec::new());

    pub(crate) fn record(launch: LaunchId, key: &IdempotencyKey, e: &LaunchEvidence) {
        let payload = serde_json::to_string(e).unwrap_or_default();
        SEEN.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((launch, key.0.clone(), payload));
    }

    /// The `(key, payload)` of every attempt for `launch`.
    pub(crate) fn of(launch: LaunchId) -> Vec<(String, String)> {
        SEEN.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(l, _, _)| *l == launch)
            .map(|(_, k, p)| (k.clone(), p.clone()))
            .collect()
    }
}
