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
            let _ = wait.recv_timeout(Duration::from_secs(30));
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
