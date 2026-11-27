//! The launch lease (`docs/design/t1-attach.md` D7, I-3): its conversion into the running
//! unit, and what a known or unknown open result does with it.

use std::time::Duration;

use chaldea_core::input::InputState;

use super::gate::set;
use super::{Live, SRig};

/// Parks `live`'s next submission between the open job's commit and the submit; returns
/// the release.
fn park_submit(
    rig: &SRig,
    live: &Live,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let sl = rig.sl(live);
    let (entered_tx, entered) = std::sync::mpsc::channel::<()>();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    set(&sl.hooks.before_submit, move || {
        let _ = entered_tx.send(());
        tokio::task::block_in_place(|| {
            let _ = wait.recv();
        });
    });
    (entered, go)
}

/// I-3: the exit takes the lease while the opened input's submission is delayed: no unit
/// is submitted, and the input is cancelled with zero bytes (not left `unconfirmed`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalidation_races_conversion() {
    let rig = SRig::new().await;
    rig.clock.resume();
    let live = rig.claude().await;
    let before = live.child.input().len();
    let (entered, go) = park_submit(&rig, &live);
    let id = rig.send(&live, "prompt").await;
    tokio::task::block_in_place(|| entered.recv_timeout(Duration::from_secs(30)))
        .expect("the submission parked");
    assert_eq!(rig.input(id).state, InputState::Sending);
    live.child.exit(chaldea_pty_runtime::ChildExit {
        code: Some(0),
        signal: None,
    });
    rig.until("the exit job committed", || {
        rig.launch(live.l).exit_reason.is_some()
    })
    .await;
    assert_eq!(rig.input(id).state, InputState::Cancelled);
    go.send(()).unwrap();
    rig.until("the worker ended", || rig.sup.send.launch(live.l).is_none())
        .await;
    assert_eq!(live.child.input().len(), before, "zero bytes");
    assert_eq!(rig.input(id).state, InputState::Cancelled);
    rig.stop().await;
}

/// A queued input behind a closed gate does not wake its own worker: after a pass that
/// opened nothing (its speculative lease released), the worker sleeps with no wake pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_gate_worker_sleeps() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let report = rig.attach(&live, b"x").await;
    assert!(report.await.is_ok());
    rig.until("recorded", || {
        !rig.of_kind(live.t, "human_write").is_empty()
    })
    .await;
    let id = rig.send(&live, "after").await;
    rig.pass(&live).await;
    assert_eq!(rig.input(id).state, InputState::Queued);
    let slept = sl.slept.load(std::sync::atomic::Ordering::SeqCst);
    sl.wake.notify_one();
    rig.until("the worker sleeps with no wake pending", || {
        sl.slept.load(std::sync::atomic::Ordering::SeqCst) > slept
    })
    .await;
    assert_eq!(rig.input(id).state, InputState::Queued);
    rig.stop().await;
}

/// I-3: the open's lease replaced by another holder while the session is still reachable
/// (a test seam between the open job and the submission): the submission does not convert
/// a lease that is not its own, so nothing is submitted and the input is cancelled with
/// zero bytes; the launch stays live.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreign_lease_not_converted() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let before = live.child.input().len();
    let me = sl.clone();
    set(&sl.hooks.before_submit, move || {
        let mut st = me.state();
        me.clear_lease(&mut st);
        let other = me.take_lease(&mut st, crate::supervisor::send::LeaseKind::Resize);
        assert!(other.is_some());
    });
    let id = rig.send(&live, "prompt").await;
    rig.until_input(id, InputState::Cancelled).await;
    assert_eq!(live.child.input().len(), before, "zero bytes");
    assert!(
        rig.sup.live(live.t, live.l).is_some(),
        "the session is reachable"
    );
    rig.stop().await;
}
