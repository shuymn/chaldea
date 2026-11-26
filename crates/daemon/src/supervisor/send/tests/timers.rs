//! Timers (send §2.7): the gate timeout with its snapshot, and the snapshot's lock use.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chaldea_core::ids::{Agent, InputId};
use chaldea_core::input::InputState;
use chaldea_core::state::AttentionCause;
use chaldea_pty_runtime::fake::ProbePoint;

use super::SRig;
use super::gate::set;
use super::outcomes::tick_at;
use crate::supervisor::live::Snapshot;
use crate::supervisor::send::{ACK_WINDOW, GATE_TIMEOUT};

/// PR10b-CL-15: `input_gate_timeout` is raised once, `GATE_TIMEOUT` after the worker first
/// read the input `queued`, with a `vt_snapshot_taken` when one was taken in time, and
/// without one while the VT lock is held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timers() {
    for vt_held in [false, true] {
        let rig = SRig::new().await;
        rig.clock.resume();
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
    assert!(matches!(r, Snapshot::Busy), "{r:?}");
    assert!(entry.ops.try_lock().is_ok(), "no lock taken");
    // In time: a snapshot.
    let r = entry
        .snapshot_before(&rig.sup, rig.sup.now() + Duration::from_secs(5))
        .await;
    assert!(matches!(r, Snapshot::Taken(_)), "{r:?}");
    // The op lock held by a stop: the snapshot answers busy without waiting for it.
    let g = entry.ops.clone().lock_owned().await;
    let started = Instant::now();
    let r = entry
        .snapshot_before(&rig.sup, rig.sup.now() + Duration::from_secs(5))
        .await;
    assert!(matches!(r, Snapshot::Busy), "{r:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "never waits for the op lock"
    );
    drop(g);
    rig.stop().await;
}

/// mvp §2.2 (`input_gate_timeout` only while `queued`): an input that leaves `queued` at the
/// launch's exit loses its gate timeout with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate_timeout_cleared_at_exit() {
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Claude, None).await;
    let id = rig.send(&live, "waits").await;
    rig.pass(&live).await;
    let t0 = rig.sl(&live).state().queued_at[&id];
    tick_at(&rig, &live, t0 + GATE_TIMEOUT).await;
    let cause = AttentionCause::InputGateTimeout { input_id: id };
    rig.until("raised", || {
        rig.thread(live.t).state.attention.contains(&cause)
    })
    .await;
    live.child.exit(chaldea_pty_runtime::ChildExit {
        code: Some(0),
        signal: None,
    });
    rig.until_input(id, chaldea_core::input::InputState::Cancelled)
        .await;
    assert!(
        !rig.thread(live.t).state.attention.contains(&cause),
        "{:?}",
        rig.thread(live.t).state.attention
    );
    rig.stop().await;
}

/// The clock's reading, kept from inside a test point that then moves the clock 1 s on (a
/// late worker or observer).
fn late_by_one_second(rig: &SRig) -> (Arc<Mutex<Option<Instant>>>, impl FnOnce() + Send + 'static) {
    let seen = Arc::new(Mutex::new(None));
    let (s2, clock) = (seen.clone(), rig.clock.clone());
    (seen, move || {
        *s2.lock().unwrap() = Some(crate::http::MonotonicClock::now(&*clock));
        clock.advance(Duration::from_secs(1));
    })
}

/// PR10b-CL-15: the gate timeout runs from the pass's read of the input as `queued`, however
/// late the pass reaches its timers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate_timeout_from_read() {
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Codex, None).await;
    rig.clock.stop();
    let sl = rig.sl(&live);
    rig.pass(&live).await;
    let (read, point) = late_by_one_second(&rig);
    set(&sl.hooks.after_read, point);
    let id = rig.send(&live, "waits").await;
    rig.until("the first read", || sl.state().queued_at.contains_key(&id))
        .await;
    let t0 = read.lock().unwrap().unwrap();
    assert_eq!(sl.state().queued_at[&id], t0);
    let cause = AttentionCause::InputGateTimeout { input_id: id };
    tick_at(&rig, &live, t0 + GATE_TIMEOUT).await;
    rig.until("raised", || {
        rig.thread(live.t).state.attention.contains(&cause)
    })
    .await;
    rig.stop().await;
}

/// PR10b-CL-8: the ack window runs from the completion job's commit, however late the worker
/// applies it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ack_window_from_commit() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    rig.clock.stop();
    let sl = rig.sl(&live);
    let (committed, point) = late_by_one_second(&rig);
    set(&sl.hooks.after_record, point);
    let id = rig.send(&live, "late").await;
    rig.until_input(id, InputState::Sent).await;
    rig.until("the window started", || {
        sl.state().sent_at.contains_key(&id)
    })
    .await;
    let t0 = committed.lock().unwrap().unwrap();
    assert_eq!(sl.state().sent_at[&id], t0);
    tick_at(&rig, &live, t0 + ACK_WINDOW).await;
    rig.until_input(id, InputState::Unconfirmed).await;
    rig.stop().await;
}

/// PR10b-CL-8: the ack window runs from the instant right after the completion job's SQLite
/// `COMMIT`, not from the end of its closure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ack_window_from_sqlite_commit() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    rig.clock.stop();
    let sl = rig.sl(&live);
    let ran = Arc::new(Mutex::new(None));
    let (r2, clock, srv) = (
        ran.clone(),
        rig.clock.clone(),
        rig.srv.store.store().clone(),
    );
    // Armed just before the completion job is submitted.
    set(&sl.hooks.record_job, move || {
        *srv.faults().before_commit.lock().unwrap() = Some(Box::new(move || {
            *r2.lock().unwrap() = Some(crate::http::MonotonicClock::now(&*clock));
            clock.advance(Duration::from_secs(1));
        }));
    });
    let id = rig.send(&live, "late").await;
    rig.until_input(id, InputState::Sent).await;
    rig.until("the window started", || {
        sl.state().sent_at.contains_key(&id)
    })
    .await;
    let t0 = ran.lock().unwrap().unwrap() + Duration::from_secs(1);
    assert_eq!(sl.state().sent_at[&id], t0);
    tick_at(&rig, &live, t0 + ACK_WINDOW - Duration::from_millis(1)).await;
    assert_eq!(rig.input(id).state, InputState::Sent);
    tick_at(&rig, &live, t0 + ACK_WINDOW).await;
    rig.until_input(id, InputState::Unconfirmed).await;
    rig.stop().await;
}

/// PR10b-CL-8: the positional input's window job reads the startup causes again: a
/// `session_start_timeout` committed after the pass read its view and before its job leaves
/// the input `sent`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn positional_window_rereads_startup() {
    let rig = SRig::new().await;
    let (live, v) = rig.spawn(Agent::Codex, Some("first")).await;
    let id: InputId = serde_json::from_value(v["input_id"].clone()).unwrap();
    assert_eq!(rig.input(id).state, InputState::Sent);
    rig.clock.stop();
    let start = rig.sup.entry(live.l).unwrap().committed().unwrap().at;
    let sl = rig.sl(&live);
    let (entered_tx, entered) = tokio::sync::oneshot::channel::<()>();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    set(&sl.hooks.after_read, move || {
        let _ = entered_tx.send(());
        let _ = wait.recv_timeout(Duration::from_secs(20));
    });
    rig.clock
        .advance((start + ACK_WINDOW).saturating_duration_since(rig.sup.now()));
    sl.wake.notify_one();
    entered.await.unwrap();
    rig.sup.tick(rig.sup.now()).await;
    assert!(
        rig.thread(live.t)
            .state
            .attention
            .contains(&AttentionCause::SessionStartTimeout)
    );
    go.send(()).unwrap();
    rig.pass(&live).await;
    assert_eq!(rig.input(id).state, InputState::Sent);
    rig.stop().await;
}

/// PR10b-CL-15 (send §2.9 D2): a snapshot holds the op lock only while it takes the session's
/// screen: a stop started while the snapshot waits to copy the screen takes the lock at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_copy_outside_lock() {
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Codex, None).await;
    let entry = rig.sup.entry(live.l).unwrap();
    let armed = rig.pauses.arm(crate::supervisor::Pause::SnapshotCopy);
    let sup = rig.sup.clone();
    let e2 = entry.clone();
    let snap = tokio::spawn(async move {
        e2.snapshot_before(&sup, sup.now() + Duration::from_secs(10))
            .await
    });
    let reached = armed.reached;
    tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .unwrap();
    assert!(entry.ops.try_lock().is_ok(), "the op lock is free");
    rig.cmd_ok("stop", &serde_json::json!({ "thread_id": live.t }), 200)
        .await;
    armed.release.send(()).unwrap();
    let _ = snap.await.unwrap();
    rig.stop().await;
}

/// PR10b-CL-15 (P-2): with the session's VT failed, the gate timeout is raised bare (no
/// `vt_snapshot_taken`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate_timeout_vt_failed() {
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Codex, None).await;
    let entry = rig.sup.entry(live.l).unwrap();
    entry
        .with_session(&rig.sup, rig.sup.now() + Duration::from_secs(5), |s| {
            s.fail_vt_on_next_piece();
        })
        .await
        .unwrap();
    live.child.emit(b"x");
    rig.until_async("the VT failed", || async {
        entry
            .with_session(&rig.sup, rig.sup.now() + Duration::from_secs(5), |s| {
                s.status().vt_failed
            })
            .await
            .unwrap()
    })
    .await;
    let id = rig.send(&live, "waits").await;
    rig.pass(&live).await;
    let t0 = rig.sl(&live).state().queued_at[&id];
    tick_at(&rig, &live, t0 + GATE_TIMEOUT).await;
    let cause = AttentionCause::InputGateTimeout { input_id: id };
    rig.until("raised", || {
        rig.thread(live.t).state.attention.contains(&cause)
    })
    .await;
    assert!(rig.of_kind(live.t, "vt_snapshot_taken").is_empty());
    rig.stop().await;
}
