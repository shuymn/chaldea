//! `SupervisorProbe` (send §2.5 "Intake facts"): the hook intake's facts and the fence.

use std::sync::atomic::Ordering;
use std::sync::{Arc, MutexGuard};
use std::time::Instant;

use chaldea_core::ids::{InputId, LaunchId};
use chaldea_pty_runtime::OriginClass;
use serde::{Deserialize, Serialize};

use super::super::live::LaunchObservation;
use super::{SendLaunch, SendSide, SendState, fence_advance, fence_release};
use crate::deadline::Backoff;
use crate::hook::{IntakeFacts, IntakeGuard, IntakeProbe, TurnMark};

/// The facts at a hook's intake, version 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IntakeFactsV1 {
    pub v: u32,
    /// The launch's `Published.reference`.
    pub reference: u64,
    /// `last_started` per `OriginClass::index`.
    pub last_started: [u64; 3],
    /// An input unit of the launch is running.
    pub running: bool,
    /// A human unit was submitted and not yet recorded.
    pub human_running: bool,
    pub inflight: Option<Inflight>,
}

/// The launch's last dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Inflight {
    pub input_id: InputId,
    /// The unit's own seq, when the writer dispatched it.
    pub position: Option<u64>,
    /// The whole unit was written and its completion ran.
    pub written: bool,
    pub competing: bool,
}

impl IntakeFactsV1 {
    /// The facts of a hook, or `None` ("facts missing"): absent, another version, or a shape
    /// that does not parse.
    pub(crate) fn parse(f: Option<&IntakeFacts>) -> Option<IntakeFactsV1> {
        let v: IntakeFactsV1 = serde_json::from_value(f?.0.clone()).ok()?;
        (v.v == 1).then_some(v)
    }

    pub(crate) fn at(&self, c: OriginClass) -> u64 {
        self.last_started[c.index()]
    }

    /// `last_started` over every class (the basis of `ClassSet::ALL`).
    pub(crate) fn position(&self) -> u64 {
        self.last_started.iter().copied().max().unwrap_or(0)
    }
}

/// The real `IntakeProbe` (send §2.5 "Intake facts"). `facts` runs on the intake's blocking
/// task: it waits, by the facts deadline, for the launch's publication while its creation
/// began, then for the launch's state; under the state it copies the send-side facts, stores
/// `latest_intake`, and advances the fence (the intake point), releases it, and observes the
/// launch with that deadline. Its one wait without a deadline of its own is the send side's
/// launch map read, a short write section (one `SendLaunch` constructed and inserted, or one
/// removed; no await, I/O, or other lock inside it); `finished` also takes the launch's state
/// mutex, held only for short sections of the same kind.
pub(crate) struct SupervisorProbe {
    pub side: Arc<SendSide>,
}

/// The launch's state, retried until `f` (real time) with sleeps capped by the time left, and
/// tried once more after the last; `None` when every attempt found it held.
fn state_until(sl: &SendLaunch, f: Instant) -> Option<MutexGuard<'_, SendState>> {
    let mut backoff = Backoff::default();
    loop {
        if let Some(st) = sl.try_state() {
            return Some(st);
        }
        #[cfg(test)]
        super::run_hook(&sl.hooks.state_contended);
        let now = Instant::now();
        if now >= f {
            return None;
        }
        backoff.wait(f - now);
        #[cfg(test)]
        if Instant::now() >= f {
            super::run_hook(&sl.hooks.state_final_wake);
        }
    }
}

impl IntakeProbe for SupervisorProbe {
    fn facts(
        &self,
        launch: LaunchId,
        ordinal: u64,
        f: Instant,
        arm: &IntakeGuard,
    ) -> Option<IntakeFacts> {
        let sl = self.side.launch(launch)?;
        sl.entry.await_published(f);
        let Some(st) = state_until(&sl, f) else {
            sl.latest_intake.store(ordinal, Ordering::SeqCst);
            fence_advance(&sl.fence);
            arm.arm();
            return None;
        };
        // The intake point: the dispatch's `written`, the running unit, and the fence advance
        // under the launch's state. A completion publishes `written` under the same state, so
        // it is either before this intake or after it; a record that commits after the read
        // leaves the unit running for it.
        let written = st
            .dispatch
            .as_ref()
            .is_some_and(|d| d.written.load(Ordering::Acquire));
        let running = st
            .running
            .as_ref()
            .is_some_and(|r| !r.recorded.load(Ordering::Acquire));
        let human_running = st.human_unsettled > 0;
        let dispatch = st
            .dispatch
            .as_ref()
            .map(|d| (d.input_id, d.before, d.competing));
        #[cfg(test)]
        super::run_hook(&sl.hooks.after_written_read);
        sl.latest_intake.store(ordinal, Ordering::SeqCst);
        fence_advance(&sl.fence);
        arm.arm();
        drop(st);
        #[cfg(test)]
        let observed = match super::lock(&sl.hooks.observe).take() {
            Some(o) => o(&sl.entry, Some(f)),
            None => sl.entry.observe(Some(f)),
        };
        #[cfg(not(test))]
        let observed = sl.entry.observe(Some(f));
        let (reference, last_started) = match observed {
            LaunchObservation::Started {
                reference,
                last_started,
            } => (reference, last_started),
            LaunchObservation::Reserved => (0, [0; 3]),
            LaunchObservation::Unobservable => return None,
        };
        let inflight = dispatch.map(|(input_id, before, competing)| {
            let at = last_started[OriginClass::Input.index()];
            Inflight {
                input_id,
                position: (at > before).then_some(at),
                written,
                competing,
            }
        });
        let facts = IntakeFactsV1 {
            v: 1,
            reference,
            last_started,
            running,
            human_running,
            inflight,
        };
        serde_json::to_value(&facts).ok().map(IntakeFacts)
    }

    fn finished(
        &self,
        launch: LaunchId,
        ordinal: u64,
        marks: &[(u64, TurnMark)],
        committed_at: Option<Instant>,
    ) {
        let Some(sl) = self.side.launch(launch) else {
            return;
        };
        #[cfg(test)]
        super::run_hook(&sl.hooks.finished);
        // Each mark carries its job's commit instant, however late this observer runs.
        if let Some(at) = committed_at {
            let mut st = sl.state();
            for (o, m) in marks {
                st.log.insert(*o, *m, at);
                // A card's basis is its own; whether the card is open is the store's, never
                // this observer's order against another hook's. The card's own resolution
                // drops it here by card id, and a key whose store read finds the card closed
                // drops it too; a turn boundary leaves it alone (a key for that card is
                // refused by the store's check).
                match *m {
                    TurnMark::Card {
                        approval_id,
                        human_at,
                    } => {
                        st.cards.insert(approval_id, human_at);
                    }
                    TurnMark::Resolved { approval_id } => {
                        st.cards.remove(&approval_id);
                    }
                    TurnMark::Start | TurnMark::End { .. } | TurnMark::Other => {}
                }
            }
        }
        let _ = ordinal;
        fence_release(&sl.fence);
        sl.wake.notify_one();
    }
}
