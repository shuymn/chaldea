//! The restart prober (`docs/design/supervisor-recovery.md` §2.3): after `attach`, each start
//! signals by verified identity the recorded leader of every unconfirmed `daemon_restart`
//! launch and the recorded child of every unconfirmed `spawn_failed` launch whose
//! `failed_start` names a pid and start time, read from the store, and records what it
//! observed as `launch_evidence`. A probe never confirms death and changes no state (§2.1
//! rule 1).
//!
//! Each probe's blocking task owns its concurrency slot and its tracker token
//! ([`ProbeAdmission`]) until it ends, by a return, an unwind, or the pool dropping it unrun:
//! exactly while it can signal (§2.1 rule 3).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chaldea_core::adapter::HostOs;
use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{LaunchId, StreamId, ThreadId};
use chaldea_core::payload::{
    EvidenceSource, ExitReason, IdentityObservation, LaunchEvidence, Observed, SourceKind,
    Unchecked,
};
use chaldea_pty_runtime::ProcessIdentity;
use chaldea_pty_runtime::orphan::{Fate, OrphanReport};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::deadline::{Settled, settle_before};
use crate::store::{LaunchRow, ReadTx, StoreError};

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

/// A launch the prober checks and the identity it signals.
struct Target {
    thread: ThreadId,
    launch: LaunchId,
    identity: ProcessIdentity,
}

/// The prober's candidates, read in one transaction (§2.3 step 2): every launch exited with
/// death unconfirmed that has a recorded identity: a `daemon_restart` launch's leader from
/// `launch_started`, and a `spawn_failed` launch's child from its `launch_exited`'s
/// `failed_start` when that names both a pid and a start time. Nothing more is validated
/// here: an identity `kill_recorded` refuses is recorded `invalid`.
fn targets(r: &ReadTx<'_>, rows: Vec<LaunchRow>) -> Result<Vec<Target>, StoreError> {
    let mut out = Vec::new();
    for l in rows {
        if l.death_confirmed {
            continue;
        }
        let identity = match l.exit_reason {
            Some(ExitReason::DaemonRestart) => l.pid.map(|pid| ProcessIdentity {
                pid,
                start_time: l.start_time.clone().unwrap_or_default(),
                group: l.pgid_or_job.clone().unwrap_or_default(),
            }),
            Some(ExitReason::SpawnFailed) => {
                let exited = r.event_by_key(
                    StreamId::Thread(l.thread_id),
                    &launch_key(l.launch_id, "exited"),
                )?;
                match exited.as_ref().map(TypedEvent::decode) {
                    Some(Ok(TypedEvent::LaunchExited(x))) => x.failed_start.and_then(|f| {
                        let start = f.start_time.filter(|s| !s.is_empty())?;
                        Some(ProcessIdentity {
                            pid: f.pid,
                            start_time: start,
                            group: f.pgid_or_job.unwrap_or_default(),
                        })
                    }),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(identity) = identity {
            out.push(Target {
                thread: l.thread_id,
                launch: l.launch_id,
                identity,
            });
        }
    }
    Ok(out)
}

/// The prober (§2.3 steps 1–4): returns once every candidate's task is spawned, with the
/// record tasks' handles (the daemon drops them; tests await them).
pub(crate) async fn run(sup: Arc<Supervisor>) -> Vec<JoinHandle<()>> {
    let mut records = Vec::new();
    // Step 1.
    sup.pauses().at(Pause::ProberStart).await;
    if !sup.tracker.accepting() {
        return records;
    }
    // Step 2: one read transaction, bounded by `PROBE_READ`.
    let until = sup.now() + PROBE_READ;
    let store = sup.store.clone();
    let wait = sup.real(until);
    let read = sup.rt.spawn_blocking(move || {
        store.read(wait, |r| {
            let rows = r.open_launches()?;
            targets(r, rows)
        })
    });
    let candidates = match settle_before(|| sup.now(), until, sup.timer(until), read)
        .await
        .in_time()
    {
        Some(Ok(Ok(targets))) => targets,
        other => {
            tracing::warn!(read = ?other.map(|r| r.map(|r| r.map(|v| v.len()))), "the restart prober read no candidates in time; the next start reads again");
            return records;
        }
    };
    // Step 3.
    for (i, l) in candidates.iter().enumerate() {
        // 3.1: a slot; a permit settled late is dropped at once.
        let by = sup.now() + SLOT_WAIT;
        let acquire = sup.probe_slots.clone().acquire_owned();
        let Some(Ok(permit)) = settle_before(|| sup.now(), by, sup.timer(by), acquire)
            .await
            .in_time()
        else {
            let left: Vec<LaunchId> = candidates[i..].iter().map(|l| l.launch).collect();
            tracing::warn!(
                ?left,
                "no probe slot in time; the next start probes these launches"
            );
            return records;
        };
        // 3.2: admission, atomic with shutdown's close of it.
        let Some(token) = sup.tracker.admit() else {
            return records;
        };
        let admission = ProbeAdmission::new(token);
        // 3.3.
        let identity = l.identity.clone();
        let attempt = Uuid::new_v4();
        let deadline = sup.now() + PROBE_BUDGET;
        // 3.4: the blocking task owns the permit and the admission.
        let s = sup.clone();
        let target = identity.clone();
        let started = Arc::new(AtomicBool::new(false));
        let on_start = started.clone();
        let join = sup.rt.spawn_blocking(move || {
            let _slot = permit;
            let _admission = admission;
            s.pauses().blocking(Pause::ProbeQueued);
            // The start mark is set under admission's mutex: a probe either started before
            // admission closed or never starts.
            s.tracker.while_accepting(|| {
                s.pauses().blocking(Pause::ProbeStarting);
                on_start.store(true, Ordering::SeqCst);
            })?;
            Some((s.cfg.kill_recorded)(
                std::slice::from_ref(&target),
                s.real(deadline),
            ))
        });
        // 3.5: a task of its own awaits the join and records the evidence.
        // The join's timer is set at dispatch, with the blocking task.
        let cutoff = deadline + PROBE_JOIN_GRACE;
        let probe = Probe {
            thread: l.thread,
            launch: l.launch,
            identity,
            attempt,
            started,
            cutoff,
            timer: sup.timer(cutoff),
        };
        records.push(sup.rt.spawn(record_probe(sup.clone(), probe, join)));
    }
    records
}

/// One probe's facts for its record.
struct Probe {
    thread: ThreadId,
    launch: LaunchId,
    identity: ProcessIdentity,
    attempt: Uuid,
    /// Set by the blocking task under admission's mutex before `kill_recorded`, apart from the join: a task the
    /// join did not answer in time made an OS call only when this is set.
    started: Arc<AtomicBool>,
    /// `deadline + PROBE_JOIN_GRACE`, and its timer.
    cutoff: std::time::Instant,
    timer: tokio::time::Instant,
}

/// §2.3 step 3.5: the join awaited until `deadline + PROBE_JOIN_GRACE`; a late report is
/// still an observation; a started probe whose join elapsed or that panicked is `not_read`
/// with `result_not_received`; a probe that had not started its OS work (cancelled at
/// shutdown, or not yet run when its join elapsed) records nothing.
async fn record_probe(sup: Arc<Supervisor>, p: Probe, join: JoinHandle<Option<OrphanReport>>) {
    let settled = settle_before(|| sup.now(), p.cutoff, p.timer, join).await;
    let (fates, received) = match settled {
        Settled::InTime(Ok(Some(r))) | Settled::Late(Ok(Some(r))) => (r.fates, true),
        Settled::InTime(Ok(None)) | Settled::Late(Ok(None)) => return,
        Settled::InTime(Err(e)) | Settled::Late(Err(e)) if e.is_cancelled() => return,
        Settled::InTime(Err(_)) | Settled::Late(Err(_)) | Settled::Elapsed => {
            if !p.started.load(Ordering::SeqCst) {
                tracing::warn!(launch = %p.launch, "the probe had not started when its join ended; nothing recorded, the next start probes again");
                return;
            }
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
