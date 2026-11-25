//! The restart reconcilers (design §2.5), each its own job at startup step 9, before the
//! listener exists: `supervisor_launches`, then `supervisor_buffers`. Startup signals no
//! old process and creates none.

use std::collections::BTreeSet;
use std::sync::Arc;

use chaldea_core::ids::IdempotencyKey;
use chaldea_core::payload::{ExitReason, InputState};

use crate::lifecycle::Reconciler;
use crate::store::TokenSelector;

use super::exit::{ExitPlan, ExitStep, exit_in, sweep};

/// `supervisor_launches`: every open launch exited (`daemon_restart` with a recorded pid,
/// else `unknown_after_crash`; death unconfirmed) with the exit steps, every in-flight
/// input recovered, and every launch token revoked. A second restart writes nothing.
pub(crate) fn launches(steps: Vec<Arc<dyn ExitStep>>) -> Reconciler {
    Reconciler {
        name: "supervisor_launches",
        job: Box::new(move |tx| {
            let mut n = 0u64;
            let mut open: Vec<_> = tx
                .read()
                .open_launches()?
                .into_iter()
                .filter(|l| l.exit_reason.is_none())
                .collect();
            open.sort_by_key(|l| (l.thread_id, l.reserved_seq));
            for l in open {
                let reason = if l.pid.is_some() {
                    ExitReason::DaemonRestart
                } else {
                    ExitReason::UnknownAfterCrash
                };
                let mut plan = ExitPlan::new(l.thread_id, l.launch_id, reason);
                plan.restart = true;
                if exit_in(tx, &steps, &plan)?.is_some() {
                    n += 1;
                }
            }
            let threads: BTreeSet<_> = tx
                .read()
                .inputs_in_states(&[InputState::Queued, InputState::Sending, InputState::Sent])?
                .into_iter()
                .map(|i| i.thread_id)
                .collect();
            let cause = IdempotencyKey("restart".to_owned());
            for t in threads {
                sweep(tx, t, None, true, &cause)?;
                n += 1;
            }
            n += tx.token_revoke(TokenSelector::LaunchScoped)?;
            Ok(n)
        }),
    }
}

/// `supervisor_buffers`: `buffer::resolve_unbindable` for every launch with buffered
/// deliveries (every launch is exited by now), including those of earlier runs.
pub(crate) fn buffers() -> Reconciler {
    Reconciler {
        name: "supervisor_buffers",
        job: Box::new(|tx| {
            let mut n = 0u64;
            for l in tx.read().buffered_launches()? {
                n += crate::hook::resolve_unbindable(tx, l)? as u64;
            }
            Ok(n)
        }),
    }
}
