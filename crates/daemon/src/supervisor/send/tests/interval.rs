//! The attach `human_write` interval (`docs/design/t1-attach.md` I-5, D11): one event per
//! interval between readiness evidence, turn starts, and input attempts; key and card-key
//! units keep one event each and neither open nor satisfy an interval; the decision for
//! each record is made once, by the worker or the exit job.

use chaldea_core::ids::{IdempotencyKey, StreamId};
use chaldea_core::input::InputState;
use serde_json::json;

use super::gate::set;
use super::human::{card_hook, key};
use super::{Live, SRig};
use crate::supervisor::send::KeyReport;

/// The `human_write` events' origins, in order.
fn origins(rig: &SRig, live: &Live) -> Vec<String> {
    rig.of_kind(live.t, "human_write")
        .into_iter()
        .map(|p| p["origin"].as_str().unwrap().to_owned())
        .collect()
}

/// An attach unit of `bytes`, written and recorded.
async fn typed(rig: &SRig, live: &Live, bytes: &[u8]) {
    let r = rig.attach(live, bytes).await.await.unwrap();
    assert!(
        matches!(r, KeyReport::Recorded { complete: true, .. }),
        "{r:?}"
    );
}

/// Whether the attach event of the interval starting at `start` exists.
fn interval_recorded(rig: &SRig, live: &Live, start: u64) -> bool {
    let key = IdempotencyKey(format!("derived:human_write:attach:{}:{start}", live.l));
    let t = live.t;
    rig.srv
        .store
        .read(move |r| r.event_by_key(StreamId::Thread(t), &key))
        .is_some()
}

async fn ready(rig: &SRig, live: &Live) -> u64 {
    rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
        .await;
    rig.launch(live.l).readiness.unwrap().seq
}

/// I-5: `ready → key → attach` records the attach: the key does not satisfy the interval.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ready_key_attach_records() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    typed(&rig, &live, b"a").await;
    let r = ready(&rig, &live).await;
    assert_eq!(key(&rig, &live, &["escape"], None).await.0, 200);
    typed(&rig, &live, b"b").await;
    assert_eq!(origins(&rig, &live), ["attach", "key", "attach"]);
    assert!(interval_recorded(&rig, &live, r));
    rig.stop().await;
}

/// I-5: `attach → card_key → attach` records the attach once: the card key does not open
/// an interval.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_card_key_attach_skips() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    typed(&rig, &live, b"a").await;
    let h = card_hook(&rig, &live, "t1");
    assert_eq!(rig.post_for(&live, &h).await.status, 200);
    let card = chaldea_core::ids::ApprovalId(h.emit);
    let r = key(&rig, &live, &["enter"], Some(card)).await;
    assert_eq!(r.0, 200, "{r:?}");
    typed(&rig, &live, b"b").await;
    assert_eq!(origins(&rig, &live), ["attach", "card_key"]);
    rig.stop().await;
}

/// I-5: many units in one interval record one event; a new readiness evidence, an input
/// attempt, and a turn start each open one more.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typing_records_once() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    for b in [b"h", b"e", b"l", b"l", b"o"] {
        typed(&rig, &live, b).await;
    }
    assert_eq!(origins(&rig, &live), ["attach"]);
    // Readiness evidence.
    let r = ready(&rig, &live).await;
    typed(&rig, &live, b"x").await;
    typed(&rig, &live, b"y").await;
    assert!(interval_recorded(&rig, &live, r));
    assert_eq!(origins(&rig, &live).len(), 2);
    // An input attempt after the readiness: the interval starts at the attempt.
    let r = ready(&rig, &live).await;
    let id = rig.send(&live, "prompt").await;
    rig.until_input(id, InputState::Sent).await;
    let l = live.l;
    let attempt = rig.srv.store.read(move |r| r.last_attempt_seq(l)).unwrap();
    assert!(attempt > r);
    typed(&rig, &live, b"z").await;
    typed(&rig, &live, b"z").await;
    assert!(interval_recorded(&rig, &live, attempt));
    assert!(!interval_recorded(&rig, &live, r));
    assert_eq!(origins(&rig, &live).len(), 3);
    // A turn start.
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "prompt" }))
        .await;
    let start = rig.launch(live.l).last_turn_start_seq.unwrap();
    assert!(start > attempt);
    typed(&rig, &live, b"w").await;
    typed(&rig, &live, b"w").await;
    assert!(interval_recorded(&rig, &live, start));
    assert_eq!(origins(&rig, &live), ["attach"; 4]);
    rig.stop().await;
}

/// I-5: a worker job committed and not yet settled when the exit job copies its set: the
/// exit job applies the worker's decision (appended, or skipped) even though a boundary
/// committed in between, so the interval has one event and the skipped record none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_exit_overlap() {
    for skipped in [false, true] {
        let rig = SRig::new().await;
        rig.clock.resume();
        let live = rig.claude().await;
        let sl = rig.sl(&live);
        if skipped {
            typed(&rig, &live, b"a").await;
        }
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        set(&sl.hooks.after_human_record, move || {
            let _ = entered_tx.send(());
            let _ = wait.recv();
        });
        let _report = rig.attach(&live, b"b").await;
        entered.await.unwrap();
        assert_eq!(origins(&rig, &live), ["attach"], "skipped: {skipped}");
        // A boundary between the worker's decision and the exit job's copy.
        ready(&rig, &live).await;
        live.child.exit(chaldea_pty_runtime::ChildExit {
            code: Some(0),
            signal: None,
        });
        rig.until("the exit job committed", || {
            rig.launch(live.l).exit_reason.is_some()
        })
        .await;
        go.send(()).unwrap();
        rig.until("the worker ended", || rig.sup.send.launch(live.l).is_none())
            .await;
        assert_eq!(origins(&rig, &live), ["attach"], "skipped: {skipped}");
        rig.stop().await;
    }
}

/// I-5: a zero-byte input attempt opens an interval too (one extra event, never one
/// fewer): the attempt's unit is refused before its submission (its lease taken by another
/// holder through a test seam), and the next attach unit records at the attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_byte_attempt_opens_interval() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    typed(&rig, &live, b"a").await;
    let r = ready(&rig, &live).await;
    let sl = rig.sl(&live);
    let me = sl.clone();
    set(&sl.hooks.before_submit, move || {
        let mut st = me.state();
        me.clear_lease(&mut st);
        assert!(
            me.take_lease(&mut st, crate::supervisor::send::LeaseKind::Resize)
                .is_some()
        );
    });
    let before = live.child.input().len();
    let id = rig.send(&live, "prompt").await;
    rig.until_input(id, InputState::Cancelled).await;
    assert_eq!(live.child.input().len(), before, "zero bytes");
    let l = live.l;
    let attempt = rig.srv.store.read(move |r| r.last_attempt_seq(l)).unwrap();
    assert!(attempt > r);
    typed(&rig, &live, b"z").await;
    assert!(interval_recorded(&rig, &live, attempt));
    assert!(!interval_recorded(&rig, &live, r));
    assert_eq!(origins(&rig, &live), ["attach", "attach"]);
    rig.stop().await;
}
