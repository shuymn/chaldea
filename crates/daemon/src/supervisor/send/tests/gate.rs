//! The gate at the open job and the writer's checks before the first permission.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use chaldea_core::adapter::ReadinessMode;
use chaldea_core::ids::{Agent, InputId};
use chaldea_core::input::InputState;
use chaldea_pty_runtime::fake::ProbePoint;
use serde_json::json;

use super::{Live, PASTE_OFF, PASTE_ON, SRig};
use crate::supervisor::lock;

/// Sets a test point of the launch's send side.
pub(crate) fn set(
    slot: &Mutex<Option<Box<dyn FnOnce() + Send>>>,
    f: impl FnOnce() + Send + 'static,
) {
    *lock(slot) = Some(Box::new(f));
}

/// Whether the child's input holds `text`'s bytes.
fn wrote(live: &Live, text: &str) -> bool {
    live.child
        .input()
        .windows(text.len())
        .any(|w| w == text.as_bytes())
}

/// A send that must stay `queued` with none of its bytes written, after two passes.
async fn stays_queued(rig: &SRig, live: &Live, what: &str) -> InputId {
    let id = rig.send(live, "probe-text").await;
    rig.pass(live).await;
    rig.pass(live).await;
    assert_eq!(rig.input(id).state, InputState::Queued, "{what}");
    assert!(!wrote(live, "probe-text"), "{what}: no byte");
    id
}

/// Waits on a blocking thread until the probe saw the `n`-th `p`.
async fn reached(rig: &SRig, live: &Live, p: ProbePoint, n: u64) {
    let probe = rig.probe(live).await;
    let ok = tokio::task::spawn_blocking(move || {
        probe.wait(p, n, Instant::now() + Duration::from_secs(10))
    })
    .await
    .unwrap();
    assert!(ok, "{p:?} #{n} reached");
}

/// PR10b-CL-3: from an open baseline, each missing gate condition alone keeps the input
/// `queued` with zero bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_condition_alone() {
    // The baseline opens.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let id = rig.send(&live, "baseline").await;
    rig.until_input(id, InputState::Sent).await;
    rig.stop().await;

    // No evidence (Codex binds without any).
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Codex, None).await;
    rig.paste(&live, true).await;
    rig.hook(&live, "SessionStart", &json!({})).await;
    stays_queued(&rig, &live, "no evidence").await;
    rig.stop().await;

    // A human write after the evidence (a raw key).
    let rig = SRig::new().await;
    let live = rig.claude().await;
    rig.cmd_ok(
        "key",
        &json!({ "thread_id": live.t, "launch_id": live.l, "keys": ["escape"] }),
        200,
    )
    .await;
    stays_queued(&rig, &live, "human write after evidence").await;
    rig.stop().await;

    // A `Stop` inside the settle interval: the turn is unknown.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let p1 = rig.send(&live, "p1").await;
    rig.until_input(p1, InputState::Sent).await;
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "p1" }))
        .await;
    rig.hook(&live, "Stop", &json!({})).await;
    stays_queued(&rig, &live, "stop inside the settle interval").await;
    rig.stop().await;

    // An interrupt (Codex).
    let rig = SRig::new().await;
    let live = rig.bound(Agent::Codex).await;
    rig.hook(&live, "Interrupt", &json!({})).await;
    stays_queued(&rig, &live, "interrupt").await;
    rig.stop().await;

    // DECSET 2004 off.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    rig.paste(&live, false).await;
    stays_queued(&rig, &live, "2004 off").await;
    rig.stop().await;

    // Unbound.
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Claude, None).await;
    rig.paste(&live, true).await;
    stays_queued(&rig, &live, "unbound").await;
    rig.stop().await;

    // `OwnerOnly` without `ready`: the binding records no evidence.
    let rig = SRig::with(|c| c.readiness_mode = Some(ReadinessMode::OwnerOnly)).await;
    let (live, _) = rig.spawn(Agent::Claude, None).await;
    rig.paste(&live, true).await;
    rig.hook(&live, "SessionStart", &json!({})).await;
    assert!(rig.launch(live.l).readiness.is_none());
    let id = stays_queued(&rig, &live, "owner only").await;
    rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
        .await;
    rig.until_input(id, InputState::Sent).await;
    rig.stop().await;

    // A counted write after the basis with no event (a VT reply).
    let rig = SRig::new().await;
    let live = rig.claude().await;
    live.child.emit(b"\x1b[6n");
    rig.until("the reply was written", || !live.child.input().is_empty())
        .await;
    stays_queued(&rig, &live, "reply after the basis").await;
    rig.stop().await;

    // An unsettled human unit (an attach unit parked before its first permission, so no
    // human write has started).
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let park = rig.probe(&live).await.park(ProbePoint::Dispatching);
    let _report = rig.attach(&live, b"typed").await;
    reached(&rig, &live, ProbePoint::Dispatching, 1).await;
    stays_queued(&rig, &live, "unsettled human unit").await;
    drop(park);
    rig.stop().await;

    // A running input unit (held in its write).
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let hold = live.child.hold_writes();
    let first = rig.send(&live, "first").await;
    assert!(hold.wait_entered(Instant::now() + Duration::from_secs(10)));
    assert!(rig.sl(&live).state().running.is_some());
    stays_queued(&rig, &live, "running input unit").await;
    drop(hold);
    rig.until_input(first, InputState::Sent).await;
    rig.stop().await;

    // An unfinished hook intake (a hook held after its intake, before its job).
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (got, release) = rig.ingest.points.arm_intake(rig.next_ordinal(&live));
    let h = rig.hook_of(&live, "Notification", &json!({}));
    let reply = crate::supervisor::tests::support::http::spawn_send(&rig.srv, &h.req(&rig.srv));
    got.await.unwrap();
    let id = stays_queued(&rig, &live, "unfinished intake").await;
    release.send(()).unwrap();
    assert_eq!(reply.await.unwrap().status, 200);
    rig.until_input(id, InputState::Sent).await;
    rig.stop().await;
}

/// PR10b-CL-5: a hook taken in before the unit's first permission refuses it `FenceMoved`
/// with zero bytes (`cancelled`); one taken in after the permission does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fence() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let probe = rig.probe(&live).await;
    let park = probe.park(ProbePoint::Dispatching);
    let id = rig.send(&live, "fenced").await;
    reached(&rig, &live, ProbePoint::Dispatching, 1).await;
    // The card's hook is taken in and committed while the writer holds the queue lock.
    rig.hook(
        &live,
        "PermissionRequest",
        &json!({ "tool_name": "Bash", "tool_input": {}, "tool_use_id": "t1" }),
    )
    .await;
    drop(park);
    rig.until_input(id, InputState::Cancelled).await;
    assert!(live.child.input().is_empty(), "zero bytes");
    rig.stop().await;

    // Taken in after the permission: written.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let probe = rig.probe(&live).await;
    let park = probe.park(ProbePoint::Permitted);
    let id = rig.send(&live, "permitted").await;
    reached(&rig, &live, ProbePoint::Permitted, 1).await;
    rig.hook(&live, "Notification", &json!({})).await;
    drop(park);
    rig.until_input(id, InputState::Sent).await;
    assert!(wrote(&live, "permitted"));
    rig.stop().await;
}

/// PR10b-CL-5: between the open job and the unit's first permission, a counted write after
/// the basis refuses it `WriteSinceBasis` and a 2004 toggle `BracketedPasteChanged`; a
/// DECCKM-only change does not refuse it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writer_checks() {
    // A DSR answered after the open job, before the unit dequeues.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let child = live.child.clone();
    set(&rig.sl(&live).hooks.before_submit, move || {
        child.emit(b"\x1b[6n");
        let end = Instant::now() + Duration::from_secs(10);
        while child.input().is_empty() {
            assert!(Instant::now() < end, "the reply was written");
            std::thread::yield_now();
        }
    });
    let id = rig.send(&live, "after-reply").await;
    rig.until_input(id, InputState::Cancelled).await;
    assert!(!wrote(&live, "after-reply"));
    rig.stop().await;

    // 2004 toggled between the open job and the submit.
    for (bytes, want) in [
        ([PASTE_OFF, PASTE_ON].concat(), InputState::Cancelled),
        (b"\x1b[?1h".to_vec(), InputState::Sent),
    ] {
        let rig = SRig::new().await;
        let live = rig.claude().await;
        let child = live.child.clone();
        let entry = rig.sup.entry(live.l).unwrap();
        let before = rig.modes(&live).await.unwrap();
        set(&rig.sl(&live).hooks.before_submit, move || {
            child.emit(&bytes);
            let end = Instant::now() + Duration::from_secs(10);
            loop {
                let now = entry
                    .ops
                    .try_lock()
                    .ok()
                    .and_then(|g| g.as_ref().map(|s| s.modes()));
                if now.is_some_and(|m| m != before) {
                    break;
                }
                assert!(Instant::now() < end, "the modes changed");
                std::thread::yield_now();
            }
        });
        let id = rig.send(&live, "mode-text").await;
        rig.until_input(id, want).await;
        assert_eq!(wrote(&live, "mode-text"), want == InputState::Sent);
        rig.stop().await;
    }
}

/// PR10b-CL-5 (SB1): after a zero-byte attempt the next input opens when the gate otherwise
/// holds, and stays `queued` when the basis is stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_byte_attempt() {
    for stale in [false, true] {
        let rig = SRig::new().await;
        let live = rig.claude().await;
        let probe = rig.probe(&live).await;
        let park = probe.park(ProbePoint::Dispatching);
        let a = rig.send(&live, "a-text").await;
        reached(&rig, &live, ProbePoint::Dispatching, 1).await;
        rig.hook(&live, "Notification", &json!({})).await;
        drop(park);
        rig.until_input(a, InputState::Cancelled).await;
        if stale {
            live.child.emit(b"\x1b[6n");
            rig.until("the reply was written", || !live.child.input().is_empty())
                .await;
            stays_queued(&rig, &live, "stale basis").await;
        } else {
            let b = rig.send(&live, "b-text").await;
            rig.until_input(b, InputState::Sent).await;
            assert!(wrote(&live, "b-text"));
        }
        rig.stop().await;
    }
}

/// PR10b-CL-5: the drain publishes a mode change under the queue lock, so one that arrives
/// while the writer holds a unit at its dispatch boundary is ordered after the unit's first
/// permission: the unit is written in the mode it was checked against, and the change is
/// delivered after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn modes_at_dispatch() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let rx = rig.watch(&live).await;
    let probe = rig.probe(&live).await;
    let parsed = probe.count(ProbePoint::PieceParsing);
    let park = probe.park(ProbePoint::Dispatching);
    let id = rig.send(&live, "日本語😀").await;
    reached(&rig, &live, ProbePoint::Dispatching, 1).await;
    live.child.emit(PASTE_OFF);
    // The drain holds the piece (its mode word waits for the queue lock).
    reached(&rig, &live, ProbePoint::PieceParsing, parsed + 1).await;
    // Not delivered while the writer holds the queue lock (the drain waits for it).
    assert!(
        tokio::task::block_in_place(|| rx.recv_timeout(Duration::from_millis(500))).is_err(),
        "the mode change waits for the permission"
    );
    drop(park);
    rig.until_input(id, InputState::Sent).await;
    assert!(
        live.child.input().starts_with(b"\x1b[200~"),
        "written as a paste"
    );
    SRig::delivered(&rx, PASTE_OFF).await;
    rig.stop().await;
}
