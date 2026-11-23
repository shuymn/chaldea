//! Intake: receipt ordinals, intake facts, and the binding order
//! (`docs/design/hook-ingest.md` §2.4).

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use chaldea_core::ids::LaunchId;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

/// The facts PR 10 needs at a hook's intake (the PTY writer's position, dispatched
/// writes), opaque to PR 9.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntakeFacts(pub serde_json::Value);

/// Supplies a hook's intake facts.
pub trait IntakeProbe: Send + Sync + 'static {
    /// The facts at this hook's intake, or `None` (no session yet). Non-blocking: it runs
    /// under the intake mutex.
    fn facts(&self, launch: LaunchId) -> Option<IntakeFacts>;
}

/// The default probe until PR 10: no facts.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoIntakeFacts;

impl IntakeProbe for NoIntakeFacts {
    fn facts(&self, _launch: LaunchId) -> Option<IntakeFacts> {
        None
    }
}

struct LaunchIntake {
    next: u64,
    pending: BTreeSet<u64>,
    released: Arc<Notify>,
}

/// Per launch: the next receipt ordinal (seeded from the stored maximum) and the ordinals
/// taken but not yet submitted to the writer or released. Memory holds nothing else.
#[derive(Default)]
pub struct IntakeOrder {
    launches: Mutex<HashMap<LaunchId, LaunchIntake>>,
    /// Run once between a waiter's check of `pending` and its await (tests only).
    #[cfg(test)]
    pub(crate) between_check_and_await: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// One taken ordinal; dropping it releases the ordinal for the binding order.
pub struct IntakeTicket {
    launch: LaunchId,
    ordinal: u64,
    order: Arc<IntakeOrder>,
}

impl std::fmt::Debug for IntakeTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntakeTicket")
            .field("launch", &self.launch)
            .field("ordinal", &self.ordinal)
            .finish_non_exhaustive()
    }
}

impl IntakeTicket {
    #[must_use]
    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }
}

impl Drop for IntakeTicket {
    fn drop(&mut self) {
        let mut map = self
            .order
            .launches
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(e) = map.get_mut(&self.launch) {
            e.pending.remove(&self.ordinal);
            e.released.notify_waiters();
        }
    }
}

impl IntakeOrder {
    /// The next ordinal of `l` (an absent entry starts at `stored_max + 1`), `probe`'s facts,
    /// and the intake instant from `now` (the pid wait's origin), in one critical section, so
    /// receipt order, the order of intake facts, and the pid wait's start agree.
    pub fn intake(
        self: &Arc<Self>,
        l: LaunchId,
        stored_max: u64,
        probe: &dyn IntakeProbe,
        now: impl FnOnce() -> Instant,
    ) -> (IntakeTicket, Option<IntakeFacts>, Instant) {
        let mut map = self.launches.lock().unwrap_or_else(PoisonError::into_inner);
        let e = map.entry(l).or_insert_with(|| LaunchIntake {
            next: stored_max.saturating_add(1),
            pending: BTreeSet::new(),
            released: Arc::new(Notify::new()),
        });
        let ordinal = e.next;
        e.next = e.next.saturating_add(1);
        e.pending.insert(ordinal);
        let facts = probe.facts(l);
        let at = now();
        drop(map);
        (
            IntakeTicket {
                launch: l,
                ordinal,
                order: self.clone(),
            },
            facts,
            at,
        )
    }

    /// Resolves once no ordinal lower than `t`'s is pending for its launch. The
    /// `Notified` future is created and enabled before `pending` is checked, so a release
    /// between the check and the await wakes it.
    pub async fn earlier_released(&self, t: &IntakeTicket) {
        loop {
            let released = {
                let map = self.launches.lock().unwrap_or_else(PoisonError::into_inner);
                match map.get(&t.launch) {
                    Some(e) => e.released.clone(),
                    None => return,
                }
            };
            let notified = released.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let lower_pending = {
                let map = self.launches.lock().unwrap_or_else(PoisonError::into_inner);
                map.get(&t.launch)
                    .is_some_and(|e| e.pending.range(..t.ordinal).next().is_some())
            };
            if !lower_pending {
                return;
            }
            #[cfg(test)]
            {
                let f = self
                    .between_check_and_await
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let Some(f) = f {
                    f();
                }
            }
            notified.await;
        }
    }

    /// Whether `ordinal` of `l` is pending (for tests).
    #[cfg(test)]
    pub(crate) fn is_pending(&self, l: LaunchId, ordinal: u64) -> bool {
        self.launches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&l)
            .is_some_and(|e| e.pending.contains(&ordinal))
    }
}
