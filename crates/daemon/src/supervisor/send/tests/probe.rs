//! `SupervisorProbe` and the fence word.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chaldea_core::ids::{Agent, LaunchId};
use chaldea_core::input::InputState;
use serde_json::json;

use super::gate::set;
use super::{Live, SRig};
use crate::hook::{IntakeGuard, IntakeProbe};
use crate::http::{OnReply, WriteFailure};
use crate::store::{StoreError, WriteError};
use crate::supervisor::send::probe::{IntakeFactsV1, SupervisorProbe};
use crate::supervisor::send::{
    FENCE_CLOSED, OPEN_BITS, OPEN_MASK, SendLaunch, fence_advance, fence_quiet, fence_release,
};

pub(crate) fn probe_of(rig: &SRig) -> Arc<dyn IntakeProbe> {
    Arc::new(SupervisorProbe {
        side: rig.sup.send.clone(),
    })
}

/// A facts deadline far enough away for any wait the test does not hold.
pub(crate) fn soon() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

/// One intake of `l` through `p` with the facts deadline `f`: its facts and its guard (armed
/// when the intake was taken in; dropping it finishes the intake).
pub(crate) fn intake(
    p: &Arc<dyn IntakeProbe>,
    l: LaunchId,
    ordinal: u64,
    f: Instant,
) -> (Option<IntakeFactsV1>, IntakeGuard) {
    let g = IntakeGuard::new(p.clone(), l, ordinal);
    let facts = p.facts(l, ordinal, f, &g);
    (IntakeFactsV1::parse(facts.as_ref()), g)
}

/// PR10b-CL-7: the probe advances the fence once at every intake and answers `None` for an
/// unknown launch and for a `SendLaunch` held past the facts deadline (one attempt when it is
/// already due), and the facts of §2.5 otherwise, the live map held or not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let p = probe_of(&rig);
    let sl = rig.sl(&live);
    let word = |sl: &crate::supervisor::send::SendLaunch| sl.fence.load(Ordering::SeqCst);
    // An unknown launch: no fence, an unarmed guard.
    let (f, g) = intake(&p, LaunchId::from_u128(99), 1, soon());
    assert!(f.is_none());
    drop(g);
    // Observed: the facts.
    let w0 = word(&sl);
    let (f, g) = intake(&p, live.l, 10, soon());
    assert_eq!(word(&sl), w0 + (1 << OPEN_BITS) + 1, "advanced");
    assert_eq!(sl.latest_intake.load(Ordering::SeqCst), 10);
    let f = f.expect("facts");
    let published = rig
        .sup
        .entry(live.l)
        .unwrap()
        .published()
        .unwrap()
        .reference;
    assert_eq!(f.reference, published);
    assert!(!f.running && !f.human_running && f.inflight.is_none());
    drop(g);
    // The `SendLaunch` mutex held past a facts deadline already due: one attempt, `None`, the
    // fence still advanced.
    {
        let _held = sl.state();
        let w = word(&sl);
        let (f, g) = intake(&p, live.l, 11, Instant::now());
        assert!(f.is_none());
        assert_eq!(word(&sl), w + (1 << OPEN_BITS) + 1);
        drop(g);
    }
    // The live map held: the observation takes no lock, so the facts are present.
    {
        let _map = rig.sup.live.lock();
        let (f, g) = intake(&p, live.l, 12, soon());
        assert!(f.is_some());
        drop(g);
    }
    assert!(fence_quiet(word(&sl)));
    // A dispatched input shows in the facts.
    let id = rig.send(&live, "seen").await;
    rig.until_input(id, InputState::Sent).await;
    let (f, g) = intake(&p, live.l, 13, soon());
    drop(g);
    let inflight = f.expect("facts").inflight.unwrap();
    assert_eq!(inflight.input_id, id);
    assert!(inflight.written && inflight.position.is_some());
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
        let (_, g) = intake(&p, live.l, ord, soon());
        path(Box::new(g));
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

/// PR10b-CL-7: the intake does not wait on another reader of the send side's launch map (a
/// command or the driver looking a launch up).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(
    clippy::await_holding_lock,
    reason = "the held map guard is the test's subject"
)]
async fn probe_beside_map_reader() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let p = probe_of(&rig);
    let held = rig.sup.send.map.read().unwrap();
    let (l, p2) = (live.l, p.clone());
    let h = std::thread::spawn(move || {
        let (f, g) = intake(&p2, l, 50, soon());
        drop(g);
        f.is_some()
    });
    rig.until("the probe answered", || h.is_finished()).await;
    drop(held);
    assert!(h.join().unwrap());
    rig.stop().await;
}

/// Holds `sl`'s state on its own thread until the returned sender is used or dropped.
fn hold_state(sl: &Arc<SendLaunch>) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (held_tx, held) = std::sync::mpsc::channel();
    let (free, until) = std::sync::mpsc::channel::<()>();
    let s2 = sl.clone();
    let h = std::thread::spawn(move || {
        let _st = s2.state();
        held_tx.send(()).unwrap();
        let _ = until.recv_timeout(Duration::from_secs(30));
    });
    held.recv().unwrap();
    (free, h)
}

/// SF-1: a Claude `SessionStart` taken in with the PTY writer parked at its dispatch boundary
/// (its queue lock held) has its facts: the binding job records `session_start`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_start_beside_parked_writer() {
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Claude, None).await;
    rig.paste(&live, true).await;
    let (park, _) = rig.writer_q_held(&live).await;
    rig.hook(&live, "SessionStart", &json!({})).await;
    drop(park);
    assert_eq!(super::evidence::evidences(&rig, &live), ["session_start"]);
    rig.stop().await;
}

/// SF-5: the probe waits for a held `SendLaunch` until the facts deadline: released after the
/// probe's `try_lock` failed, the facts; held past the deadline, `None`. The fence advances
/// once either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_wait() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let p = probe_of(&rig);
    let sl = rig.sl(&live);
    let word = || sl.fence.load(Ordering::SeqCst);
    // Released while the probe waits.
    let (free, holder) = hold_state(&sl);
    set(&sl.hooks.state_contended, move || free.send(()).unwrap());
    let w = word();
    let (f, g) = intake(&p, live.l, 20, soon());
    assert!(f.is_some(), "the facts after the release");
    assert_eq!(word(), w + (1 << OPEN_BITS) + 1, "advanced once");
    drop(g);
    holder.join().unwrap();
    // Held past the facts deadline.
    let (free, holder) = hold_state(&sl);
    let w = word();
    let (f, g) = intake(&p, live.l, 21, Instant::now() + Duration::from_millis(100));
    assert!(f.is_none(), "facts missing");
    assert_eq!(word(), w + (1 << OPEN_BITS) + 1, "advanced once");
    free.send(()).unwrap();
    holder.join().unwrap();
    drop(g);
    rig.stop().await;
}

/// SF-10: a wait for the launch's state that reaches the facts deadline makes one more
/// attempt: released between the last sleep and that attempt, the facts are present.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_at_facts_deadline() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let p = probe_of(&rig);
    let sl = rig.sl(&live);
    let (free, holder) = hold_state(&sl);
    let s2 = sl.clone();
    set(&sl.hooks.state_final_wake, move || {
        free.send(()).unwrap();
        holder.join().unwrap();
        drop(s2);
    });
    let (f, g) = intake(&p, live.l, 20, Instant::now() + Duration::from_millis(50));
    assert!(
        super::super::lock(&sl.hooks.state_final_wake).is_none(),
        "the final wake ran"
    );
    assert!(f.is_some(), "the final attempt took the state");
    drop(g);
    rig.stop().await;
}

/// SF-10: an intake whose blocking task starts past its facts deadline, the request deadline
/// far away, still takes its ordinal and makes one attempt at each wait: an uncontended
/// `SessionStart` has its facts (`session_start`) and the reply is 200.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_past_facts_deadline() {
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Claude, None).await;
    rig.paste(&live, true).await;
    *rig.ingest.order.task_started.lock().unwrap() =
        Some(Box::new(|| std::thread::sleep(Duration::from_millis(700))));
    let ord = rig.next_ordinal(&live);
    rig.hook(&live, "SessionStart", &json!({})).await;
    assert!(rig.ingest.order.task_started.lock().unwrap().is_none());
    assert_eq!(rig.sl(&live).latest_intake.load(Ordering::SeqCst), ord);
    assert_eq!(super::evidence::evidences(&rig, &live), ["session_start"]);
    rig.stop().await;
}
