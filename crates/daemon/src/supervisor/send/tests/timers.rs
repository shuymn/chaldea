//! Timers (send §2.7): the gate timeout with its snapshot, and the snapshot's lock use.

use std::time::{Duration, Instant};

use chaldea_core::ids::Agent;
use chaldea_core::state::AttentionCause;
use chaldea_pty_runtime::fake::ProbePoint;

use super::SRig;
use super::outcomes::tick_at;
use crate::supervisor::send::GATE_TIMEOUT;

/// PR10b-CL-15: `input_gate_timeout` is raised once, `GATE_TIMEOUT` after the worker first
/// read the input `queued`, with a `vt_snapshot_taken` when one was taken in time, and
/// without one while the VT lock is held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timers() {
    for vt_held in [false, true] {
        let rig = SRig::new().await;
        let (live, _) = rig.spawn(Agent::Codex, None).await;
        // The clock runs (the snapshot's 250 ms bound is real); the timers see injected ticks.
        let id = rig.send(&live, "waits").await;
        rig.pass(&live).await;
        // The worker's first observation of the input as queued.
        let t0 = rig.sl(&live).state().queued_at[&id];
        let raised = |rig: &SRig| {
            rig.thread(live.t)
                .state
                .attention
                .contains(&AttentionCause::InputGateTimeout { input_id: id })
        };
        tick_at(&rig, &live, t0 + GATE_TIMEOUT - Duration::from_millis(1)).await;
        assert!(!raised(&rig));
        let park = if vt_held {
            let probe = rig.probe(&live).await;
            let park = probe.park(ProbePoint::PieceParsing);
            live.child.emit(b"x");
            let p2 = probe.clone();
            assert!(
                tokio::task::spawn_blocking(move || p2.wait(
                    ProbePoint::PieceParsing,
                    1,
                    Instant::now() + Duration::from_secs(10)
                ))
                .await
                .unwrap()
            );
            Some(park)
        } else {
            None
        };
        tick_at(&rig, &live, t0 + GATE_TIMEOUT).await;
        rig.until("raised", || raised(&rig)).await;
        drop(park);
        tick_at(&rig, &live, t0 + GATE_TIMEOUT + Duration::from_secs(1)).await;
        let snaps = rig.of_kind(live.t, "vt_snapshot_taken").len();
        assert_eq!(snaps, usize::from(!vt_held), "vt held: {vt_held}");
        // Once: a later tick writes nothing more.
        let n = rig.events(live.t).len();
        tick_at(&rig, &live, t0 + GATE_TIMEOUT + Duration::from_secs(2)).await;
        assert_eq!(rig.events(live.t).len(), n, "once");
        rig.stop().await;
    }
}

/// PR10b-CL-15: a snapshot whose task starts after its deadline takes no lock, and a snapshot
/// never holds the op lock while it waits for the blocking pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_pool() {
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Codex, None).await;
    let entry = rig.sup.entry(live.l).unwrap();
    // Past its deadline: busy at once, the op lock free.
    let r = entry.snapshot_before(&rig.sup, rig.sup.now()).await;
    assert!(r.is_err());
    assert!(entry.ops.try_lock().is_ok(), "no lock taken");
    // In time: a snapshot.
    let r = entry
        .snapshot_before(&rig.sup, rig.sup.now() + Duration::from_secs(5))
        .await;
    assert!(r.is_ok(), "{:?}", r.err());
    // The op lock held by a stop: the snapshot answers busy without waiting for it.
    let g = entry.ops.clone().lock_owned().await;
    let started = Instant::now();
    let r = entry
        .snapshot_before(&rig.sup, rig.sup.now() + Duration::from_secs(5))
        .await;
    assert!(r.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "never waits for the op lock"
    );
    drop(g);
    rig.stop().await;
}
