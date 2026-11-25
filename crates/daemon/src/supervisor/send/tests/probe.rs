//! `SupervisorProbe` and the fence word.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chaldea_core::ids::{Agent, LaunchId};
use chaldea_core::input::InputState;
use serde_json::json;

use super::{Live, SRig};
use crate::hook::{IntakeGuard, IntakeProbe};
use crate::http::{OnReply, WriteFailure};
use crate::store::{StoreError, WriteError};
use crate::supervisor::send::probe::{IntakeFactsV1, SupervisorProbe};
use crate::supervisor::send::{
    FENCE_CLOSED, OPEN_BITS, OPEN_MASK, fence_advance, fence_quiet, fence_release,
};

fn probe_of(rig: &SRig) -> Arc<dyn IntakeProbe> {
    Arc::new(SupervisorProbe {
        sup: Arc::downgrade(&rig.sup),
        side: rig.sup.send.clone(),
    })
}

/// PR10b-CL-7: the probe never waits; it advances the fence at every intake, answers `None`
/// for an unknown or unobservable launch and for a contended `SendLaunch`, and the facts of
/// §2.5 otherwise.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let p = probe_of(&rig);
    let sl = rig.sl(&live);
    let word = |sl: &crate::supervisor::send::SendLaunch| sl.fence.load(Ordering::SeqCst);
    // An unknown launch.
    assert!(p.facts(LaunchId::from_u128(99), 1).is_none());
    // Observed: the facts.
    let w0 = word(&sl);
    let f = IntakeFactsV1::parse(p.facts(live.l, 10).as_ref()).unwrap();
    assert_eq!(word(&sl), w0 + (1 << OPEN_BITS) + 1, "advanced");
    assert_eq!(sl.latest_intake.load(Ordering::SeqCst), 10);
    let published = rig
        .sup
        .entry(live.l)
        .unwrap()
        .published()
        .unwrap()
        .reference;
    assert_eq!(f.reference, published);
    assert!(!f.running && !f.human_running && f.inflight.is_none());
    p.finished(live.l, 10, &[]);
    // The `SendLaunch` mutex held: `None` at once, the fence still advanced.
    {
        let _held = sl.state();
        let w = word(&sl);
        assert!(p.facts(live.l, 11).is_none());
        assert_eq!(word(&sl), w + (1 << OPEN_BITS) + 1);
    }
    p.finished(live.l, 11, &[]);
    // The live map held: unobservable, `None`.
    {
        let _map = rig.sup.live.lock();
        assert!(p.facts(live.l, 12).is_none());
    }
    p.finished(live.l, 12, &[]);
    assert!(fence_quiet(word(&sl)));
    // A dispatched input shows in the facts.
    let id = rig.send(&live, "seen").await;
    rig.until_input(id, InputState::Sent).await;
    let f = IntakeFactsV1::parse(p.facts(live.l, 13).as_ref()).unwrap();
    let inflight = f.inflight.unwrap();
    assert_eq!(inflight.input_id, id);
    assert!(inflight.written && inflight.position.is_some());
    p.finished(live.l, 13, &[]);
    rig.stop().await;
}

/// PR10b-CL-7: the word's arithmetic: an add that would overflow either field closes it, the
/// closed word never changes, and a release at zero unfinished does nothing.
#[test]
fn fence_arithmetic() {
    let w = AtomicU64::new(0);
    fence_advance(&w);
    fence_advance(&w);
    assert_eq!(w.load(Ordering::SeqCst), (2 << OPEN_BITS) + 2);
    fence_release(&w);
    assert_eq!(w.load(Ordering::SeqCst), (2 << OPEN_BITS) + 1);
    fence_release(&w);
    fence_release(&w);
    assert_eq!(
        w.load(Ordering::SeqCst),
        2 << OPEN_BITS,
        "no release below zero"
    );
    assert!(fence_quiet(w.load(Ordering::SeqCst)));
    // The open count at its maximum.
    let w = AtomicU64::new(OPEN_MASK);
    fence_advance(&w);
    assert_eq!(w.load(Ordering::SeqCst), FENCE_CLOSED);
    // The intake count at its maximum.
    let w = AtomicU64::new((u64::MAX >> OPEN_BITS) << OPEN_BITS);
    assert_eq!(w.load(Ordering::SeqCst) & OPEN_MASK, 0);
    fence_advance(&w);
    assert_eq!(w.load(Ordering::SeqCst), FENCE_CLOSED);
    // Closed stays closed through a release and an advance.
    fence_release(&w);
    fence_advance(&w);
    assert_eq!(w.load(Ordering::SeqCst), FENCE_CLOSED);
    assert!(!fence_quiet(FENCE_CLOSED));
}

/// One way an intake's guard ends.
type GuardPath = Box<dyn FnOnce(Box<IntakeGuard>)>;

/// A send that stays `queued` and a raw key answered `key_not_written`: the fence holds.
async fn refused(rig: &SRig, live: &Live, what: &str) {
    let id = rig.send(live, "fenced-out").await;
    rig.pass(live).await;
    assert_eq!(rig.input(id).state, InputState::Queued, "{what}");
    let v = rig
        .cmd_ok(
            "key",
            &json!({ "thread_id": live.t, "launch_id": live.l, "keys": ["escape"] }),
            409,
        )
        .await;
    assert_eq!(v["code"], json!("key_not_written"), "{what}");
    rig.cmd_ok("input_cancel", &json!({ "input_id": id }), 200)
        .await;
}

/// PR10b-CL-7: the fence at the closed value refuses sends and keys; every `IntakeGuard` path
/// finishes the intake once (dropped before submission, a decided reply) or leaves it
/// unfinished (`CommitUnknown`, `WriterGone`, an aborted observer), which refuses sends and
/// keys until the launch exits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fence_word() {
    // Closed.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    sl.fence.store(OPEN_MASK, Ordering::SeqCst);
    fence_advance(&sl.fence);
    assert_eq!(sl.fence.load(Ordering::SeqCst), FENCE_CLOSED);
    fence_release(&sl.fence);
    refused(&rig, &live, "closed").await;
    rig.stop().await;

    let unknown = |e: WriteError| -> GuardPath {
        Box::new(move |mut g: Box<IntakeGuard>| {
            OnReply::<crate::hook::route::Ran>::submitted(&mut *g);
            g.replied(&Err(WriteFailure::Write(e)));
        })
    };
    let paths: Vec<(&str, bool, GuardPath)> = vec![
        ("dropped before submission", true, Box::new(drop)),
        (
            "rolled back",
            true,
            Box::new(|mut g: Box<IntakeGuard>| {
                OnReply::<crate::hook::route::Ran>::submitted(&mut *g);
                g.replied(&Err(WriteFailure::Write(WriteError::Panicked)));
            }),
        ),
        (
            "commit unknown",
            false,
            unknown(WriteError::CommitUnknown(StoreError::Decode("x".into()))),
        ),
        ("writer gone", false, unknown(WriteError::WriterGone)),
        (
            "observer aborted",
            false,
            Box::new(|mut g: Box<IntakeGuard>| {
                OnReply::<crate::hook::route::Ran>::submitted(&mut *g);
                drop(g);
            }),
        ),
    ];
    for (what, finishes, path) in paths {
        let rig = SRig::new().await;
        let live = rig.claude().await;
        let p = probe_of(&rig);
        let sl = rig.sl(&live);
        let ord = rig.next_ordinal(&live);
        let _ = p.facts(live.l, ord);
        let g = Box::new(IntakeGuard::new(p.clone(), live.l, ord));
        path(g);
        assert_eq!(
            fence_quiet(sl.fence.load(Ordering::SeqCst)),
            finishes,
            "{what}"
        );
        if finishes {
            let id = rig.send(&live, "goes").await;
            rig.until_input(id, InputState::Sent).await;
        } else {
            refused(&rig, &live, what).await;
        }
        rig.stop().await;
    }
}

/// PR10b-CL-7: a matching hook taken in before the input's dispatch is a `user_message`
/// without `input_id`, and the input is unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undispatched_match() {
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Codex, None).await;
    rig.paste(&live, true).await;
    rig.hook(&live, "SessionStart", &json!({})).await;
    let id = rig.send(&live, "not yet").await;
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "not yet" }))
        .await;
    let msgs = rig.of_kind(live.t, "user_message");
    assert_eq!(msgs.len(), 1);
    assert!(
        msgs[0]
            .get("input_id")
            .is_none_or(serde_json::Value::is_null),
        "{msgs:?}"
    );
    assert_eq!(rig.input(id).state, InputState::Queued);
    rig.stop().await;
}
