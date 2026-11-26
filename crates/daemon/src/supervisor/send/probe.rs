//! `SupervisorProbe` (send §2.5 "Intake facts"): the hook intake's facts and the fence.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Instant;

use chaldea_core::ids::{InputId, LaunchId};
use chaldea_pty_runtime::OriginClass;
use serde::{Deserialize, Serialize};

use super::super::Supervisor;
use super::super::live::LaunchObservation;
use super::{SendSide, fence_advance, fence_release};
use crate::hook::{IntakeFacts, IntakeProbe, TurnMark};

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

/// The real `IntakeProbe`. `facts` waits on nothing but its read of the send side's launch
/// map, which may wait on a short write section (one `SendLaunch` constructed and inserted,
/// or one removed; no await, I/O, or other lock inside it); `finished` also takes the
/// launch's state mutex, held only for short sections of the same kind. Neither wait has a
/// deadline of its own.
pub(crate) struct SupervisorProbe {
    pub sup: Weak<Supervisor>,
    pub side: Arc<SendSide>,
}

impl IntakeProbe for SupervisorProbe {
    fn facts(&self, launch: LaunchId, ordinal: u64) -> Option<IntakeFacts> {
        let sl = self.side.launch(launch)?;
        // One observation point (send §2.5): the launch's state is held across the read of
        // the dispatch's `written` and the fence advance, and a completion publishes
        // `written` under the same state, so it is either before this intake or after it.
        let st = sl.try_state();
        let written = st
            .as_ref()
            .and_then(|st| st.dispatch.as_ref())
            .is_some_and(|d| d.written.load(Ordering::Acquire));
        #[cfg(test)]
        super::run_hook(&sl.hooks.after_written_read);
        sl.latest_intake.store(ordinal, Ordering::SeqCst);
        fence_advance(&sl.fence);
        let sup = self.sup.upgrade()?;
        #[cfg(test)]
        let observed = match super::lock(&sl.hooks.observe).take() {
            Some(f) => f(&sup, launch),
            None => sup.observe(launch),
        };
        #[cfg(not(test))]
        let observed = sup.observe(launch);
        let (reference, last_started) = match observed {
            LaunchObservation::Started {
                reference,
                last_started,
            } => (reference, last_started),
            LaunchObservation::Reserved => (0, [0; 3]),
            LaunchObservation::Unobservable => return None,
        };
        let st = st?;
        let inflight = st.dispatch.as_ref().map(|d| {
            let at = last_started[OriginClass::Input.index()];
            Inflight {
                input_id: d.input_id,
                position: (at > d.before).then_some(at),
                written,
                competing: d.competing,
            }
        });
        let f = IntakeFactsV1 {
            v: 1,
            reference,
            last_started,
            // A unit whose record committed has ended for the intake, in the writer's order.
            running: st
                .running
                .as_ref()
                .is_some_and(|r| !r.recorded.load(Ordering::Acquire)),
            human_running: st.human_unsettled > 0,
            inflight,
        };
        drop(st);
        serde_json::to_value(&f).ok().map(IntakeFacts)
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
