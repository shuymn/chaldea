//! Human units, cards, and keys (send §2.6): `human_write` recording, the gate after keys,
//! card lifecycle, and the card key's rule end to end through the hook route.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use chaldea_core::ids::ApprovalId;
use chaldea_core::input::InputState;
use chaldea_core::payload::ApprovalOutcome;
use chaldea_pty_runtime::fake::ProbePoint;
use chaldea_pty_runtime::{Guard, WriteOrigin, WriteUnit};
use serde_json::{Value, json};

use super::gate::set;
use super::{Live, SRig};
use crate::store::test_faults::CommitFault;
use crate::supervisor::send::SETTLE;

/// `key` as the owner: the status and the body.
pub(super) async fn key(
    rig: &SRig,
    live: &Live,
    keys: &[&str],
    card: Option<ApprovalId>,
) -> (u16, Value) {
    let mut args = json!({ "thread_id": live.t, "launch_id": live.l, "keys": keys });
    if let Some(c) = card {
        args["approval_id"] = json!(c);
    }
    let r = rig
        .cmd_as(&rig.owner, "key", crate::supervisor::tests::cid(), &args)
        .await;
    (
        r.status,
        serde_json::from_slice(&r.body).unwrap_or(Value::Null),
    )
}

fn not_written(r: &(u16, Value)) -> bool {
    r.0 == 409 && r.1["code"] == json!("key_not_written")
}

/// A `PermissionRequest` through the hook route; returns the card it opened.
async fn card(rig: &SRig, live: &Live, tool_use_id: &str) -> ApprovalId {
    let h = rig.hook_of(
        live,
        "PermissionRequest",
        &json!({ "tool_name": "Bash", "tool_input": { "command": "ls" }, "tool_use_id": tool_use_id }),
    );
    assert_eq!(rig.post_for(live, &h).await.status, 200);
    ApprovalId(h.emit)
}

pub(super) fn outcome(rig: &SRig, id: ApprovalId) -> Option<ApprovalOutcome> {
    rig.srv
        .store
        .read(move |r| r.approval(id))
        .and_then(|a| a.outcome)
}

/// The `human_write` events' origins, in order.
fn human_writes(rig: &SRig, live: &Live) -> Vec<String> {
    rig.of_kind(live.t, "human_write")
        .into_iter()
        .map(|p| p["origin"].as_str().unwrap().to_owned())
        .collect()
}

/// Blocks a test point until released; the receiver hears it entered.
fn gate_point() -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
    impl FnOnce() + Send + 'static,
) {
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    (entered, go, move || {
        let _ = entered_tx.send(());
        let _ = wait.recv_timeout(Duration::from_secs(30));
    })
}

/// Submits a human unit with `guard` through the session (PR 11's attach path).
async fn human_unit(rig: &SRig, live: &Live, origin: WriteOrigin, guard: Guard) {
    let sl = rig.sl(live);
    let e = rig.sup.entry(live.l).unwrap();
    e.with_session(&rig.sup, rig.sup.now() + Duration::from_secs(5), move |s| {
        sl.submit_human(s, WriteUnit::bytes(origin, b"k".to_vec()), guard, None)
    })
    .await
    .unwrap()
    .unwrap();
}

/// PR10b-CL-12: one `human_write` per `Written` or `Incomplete` human unit with its origin, none
/// for `NotStarted`, in `seq` order; `human_unsettled` returns to 0 only once every unit was
/// recorded or refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn human_writes_recorded() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let far = rig.sup.real(rig.sup.now() + Duration::from_secs(60));
    // Written: attach, key, card key.
    human_unit(&rig, &live, WriteOrigin::Attach, Guard::until(far)).await;
    assert!(key(&rig, &live, &["escape"], None).await.0 == 200);
    let c = card(&rig, &live, "t1").await;
    let r = key(&rig, &live, &["enter"], Some(c)).await;
    assert_eq!(r.0, 200, "{r:?}");
    assert_eq!(r.1["outcome"], json!("written"));
    rig.until("three recorded", || human_writes(&rig, &live).len() == 3)
        .await;
    assert_eq!(human_writes(&rig, &live), ["attach", "key", "card_key"]);
    let cw = rig.of_kind(live.t, "human_write").pop().unwrap();
    assert_eq!(cw["approval_id"], json!(c));
    // NotStarted (expired): none. Incomplete: recorded.
    human_unit(
        &rig,
        &live,
        WriteOrigin::Attach,
        Guard::until(Instant::now()),
    )
    .await;
    let sl = rig.sl(&live);
    rig.until("settled", || sl.state().human_unsettled == 0)
        .await;
    assert_eq!(human_writes(&rig, &live).len(), 3);
    // An arrow key (three bytes) whose write fails after one byte: `incomplete`, recorded.
    let so_far = u64::try_from(live.child.input().len()).unwrap();
    live.child
        .fail_writes_after(so_far + 1, std::io::ErrorKind::BrokenPipe);
    let r = key(&rig, &live, &["up"], None).await;
    assert_eq!(
        (r.0, r.1["outcome"].clone(), r.1["bytes_written"].clone()),
        (200, json!("incomplete"), json!(1)),
        "{r:?}"
    );
    assert_eq!(
        human_writes(&rig, &live),
        ["attach", "key", "card_key", "key"]
    );
    assert_eq!(sl.state().human_unsettled, 0);
    rig.stop().await;

    // Two human units, the first's record job held: both wait, recorded in `seq` order, and
    // no send opens meanwhile.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let (entered, go, point) = gate_point();
    set(&sl.hooks.record_job, point);
    let far = rig.sup.real(rig.sup.now() + Duration::from_secs(60));
    human_unit(&rig, &live, WriteOrigin::Attach, Guard::until(far)).await;
    entered.await.unwrap();
    human_unit(&rig, &live, WriteOrigin::Key, Guard::until(far)).await;
    rig.until("both written", || live.child.input().len() == 2)
        .await;
    assert_eq!(sl.state().human_unsettled, 2);
    let id = rig.send(&live, "waits").await;
    assert_eq!(rig.input(id).state, InputState::Queued);
    go.send(()).unwrap();
    rig.until("settled", || sl.state().human_unsettled == 0)
        .await;
    assert_eq!(human_writes(&rig, &live), ["attach", "key"]);
    rig.stop().await;
}

/// PR10b-CL-12: records the worker still holds at the exit are recorded by the exit job, once,
/// also when its first attempt rolled back after copying `exit_set`; a `NotStarted` record in
/// `exit_set` writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn human_writes_at_exit() {
    for rollback in [false, true] {
        let rig = SRig::new().await;
        rig.clock.resume();
        let live = rig.claude().await;
        let sl = rig.sl(&live);
        let (entered, go, point) = gate_point();
        set(&sl.hooks.record_job, point);
        let far = rig.sup.real(rig.sup.now() + Duration::from_secs(60));
        human_unit(&rig, &live, WriteOrigin::Key, Guard::until(far)).await;
        entered.await.unwrap();
        // A second unit that is refused (`NotStarted`), its record fired behind the held one.
        human_unit(&rig, &live, WriteOrigin::Key, Guard::until(Instant::now())).await;
        rig.until("both fired", || sl.outstanding.load(Ordering::SeqCst) == 0)
            .await;
        // The exit job is the next commit: its first attempt copies `exit_set`, then its
        // `COMMIT` is refused `Busy` and it is retried.
        let faults = rig.srv.store.store().faults();
        if rollback {
            *faults.commit.lock().unwrap() = Some(CommitFault::BusyBeforeCommit);
        }
        live.child.exit(chaldea_pty_runtime::ChildExit {
            code: Some(0),
            signal: None,
        });
        rig.until("the exit job committed", || {
            rig.launch(live.l).exit_reason.is_some()
        })
        .await;
        assert!(
            faults.commit.lock().unwrap().is_none(),
            "the fault was taken"
        );
        assert_eq!(human_writes(&rig, &live), ["key"], "by the exit job");
        // The removal from the live map wakes no worker: released before it, the worker
        // could park for good.
        rig.until("the launch left the live map", || {
            rig.sup.entry(live.l).is_none()
        })
        .await;
        go.send(()).unwrap();
        rig.until("the worker ended", || rig.sup.send.launch(live.l).is_none())
            .await;
        assert_eq!(
            human_writes(&rig, &live),
            ["key"],
            "once (rollback: {rollback})"
        );
        rig.stop().await;
    }
}

/// PR10b-CL-12: after a raw key, a settle of the key's turn does not reopen the gate; the next
/// turn's settle does. After a card key, the turn's next evidence reopens it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_turn_evidence() {
    for card_key in [false, true] {
        let rig = SRig::new().await;
        let live = rig.claude().await;
        rig.clock.stop();
        rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "turn" }))
            .await;
        if card_key {
            let c = card(&rig, &live, "t1").await;
            assert_eq!(key(&rig, &live, &["enter"], Some(c)).await.0, 200);
            rig.hook(&live, "PostToolUse", &json!({ "tool_use_id": "t1" }))
                .await;
        } else {
            assert_eq!(key(&rig, &live, &["escape"], None).await.0, 200);
        }
        rig.until("recorded", || human_writes(&rig, &live).len() == 1)
            .await;
        rig.hook(&live, "Stop", &json!({})).await;
        rig.clock.advance(SETTLE);
        rig.pass(&live).await;
        let id = rig.send(&live, "after").await;
        rig.pass(&live).await;
        if card_key {
            rig.until_input(id, InputState::Sent).await;
        } else {
            assert_eq!(rig.input(id).state, InputState::Queued, "same turn");
            // The next turn's settle reopens it.
            rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "next" }))
                .await;
            rig.hook(&live, "Stop", &json!({})).await;
            rig.clock.advance(SETTLE);
            rig.pass(&live).await;
            rig.until_input(id, InputState::Sent).await;
        }
        rig.stop().await;
    }
}

/// PR10b-CL-13: a card's basis is dropped at the card's own resolution, so cards resolved by
/// their evidence leave nothing behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn card_basis_dropped_at_resolution() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    for k in 0..10 {
        rig.hook(
            &live,
            "UserPromptSubmit",
            &json!({ "prompt": format!("t{k}") }),
        )
        .await;
        let (r, s) = (format!("r{k}"), format!("s{k}"));
        card(&rig, &live, &r).await;
        card(&rig, &live, &s).await;
        assert_eq!(sl.state().cards.len(), 2);
        rig.hook(&live, "PostToolUse", &json!({ "tool_use_id": r }))
            .await;
        assert_eq!(sl.state().cards.len(), 1, "the first resolved card's basis");
        rig.hook(&live, "PostToolUse", &json!({ "tool_use_id": s }))
            .await;
        assert_eq!(
            sl.state().cards.len(),
            0,
            "the second resolved card's basis"
        );
        rig.hook(&live, "Stop", &json!({})).await;
    }
    rig.stop().await;
}

/// PR10b-CL-13: a card whose hook was taken in before a `Stop` but committed after it is open
/// in the store, and its basis stays: a card key for it is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn card_committed_after_later_stop() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "t" }))
        .await;
    let (got, release) = rig.ingest.points.arm_job(rig.next_ordinal(&live));
    let h = rig.hook_of(
        &live,
        "PermissionRequest",
        &json!({ "tool_name": "Bash", "tool_input": { "command": "ls" }, "tool_use_id": "t1" }),
    );
    let reply = crate::supervisor::tests::support::http::spawn_send(&rig.srv, &h.req(&rig.srv));
    got.await.unwrap();
    // The later `Stop` commits and finishes first; the card's job then commits.
    rig.hook(&live, "Stop", &json!({})).await;
    release.send(()).unwrap();
    assert_eq!(reply.await.unwrap().status, 200);
    let id = ApprovalId(h.emit);
    assert_eq!(outcome(&rig, id), None, "open in the store");
    let r = key(&rig, &live, &["enter"], Some(id)).await;
    assert_eq!(
        (r.0, r.1["outcome"].clone()),
        (200, json!("written")),
        "{r:?}"
    );
    rig.stop().await;
}

/// PR10b-CL-13: `ApprovalsAllResolved` only when the last open card resolves; one `Stop`
/// supersedes every open card; cards expire at exit and at restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cards() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "t" }))
        .await;
    let (a, b) = (card(&rig, &live, "ta").await, card(&rig, &live, "tb").await);
    let causes = |rig: &SRig| {
        rig.of_kind(live.t, "status_changed")
            .into_iter()
            .filter(|p| p["cause"] == json!("approvals_all_resolved"))
            .count()
    };
    rig.hook(&live, "PostToolUse", &json!({ "tool_use_id": "ta" }))
        .await;
    assert_eq!(outcome(&rig, a), Some(ApprovalOutcome::Ran));
    assert_eq!(causes(&rig), 0);
    rig.hook(&live, "PostToolUse", &json!({ "tool_use_id": "tb" }))
        .await;
    assert_eq!(outcome(&rig, b), Some(ApprovalOutcome::Ran));
    assert_eq!(causes(&rig), 1);
    // One `Stop` supersedes two; a resolution after it changes nothing.
    let (c, d) = (card(&rig, &live, "tc").await, card(&rig, &live, "td").await);
    rig.hook(&live, "Stop", &json!({})).await;
    assert_eq!(outcome(&rig, c), Some(ApprovalOutcome::Superseded));
    assert_eq!(outcome(&rig, d), Some(ApprovalOutcome::Superseded));
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "u" }))
        .await;
    let e = card(&rig, &live, "te").await;
    rig.hook(&live, "PostToolUse", &json!({ "tool_use_id": "tc" }))
        .await;
    assert_eq!(outcome(&rig, e), None, "the next turn's card stays open");
    // Exit expires it.
    live.child.exit(chaldea_pty_runtime::ChildExit {
        code: Some(0),
        signal: None,
    });
    rig.until("expired at exit", || {
        outcome(&rig, e) == Some(ApprovalOutcome::Expired)
    })
    .await;
    rig.stop().await;

    // Restart expires the open cards of a launch left running.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "t" }))
        .await;
    let f = card(&rig, &live, "tf").await;
    let SRig { rig, .. } = rig;
    let (_, store) = rig.srv.into_store(Duration::from_secs(10)).await;
    drop(rig.sup);
    assert!(crate::supervisor::tests::restart::restart_all(&store) > 0);
    let got = store.read(move |r| r.approval(f)).and_then(|a| a.outcome);
    assert_eq!(got, Some(ApprovalOutcome::Expired));
}

/// PR10b-CL-16: a key whose launch is current and `running` in the store but whose session
/// left its slot (`with_session` → `Gone`) answers `no_live_launch` with zero bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn key_gone() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let entry = rig.sup.entry(live.l).unwrap();
    let taken = entry.ops.lock().await.take();
    assert!(taken.is_some());
    let n = live.child.input().len();
    let r = key(&rig, &live, &["escape"], None).await;
    assert_eq!(r.1["code"], json!("no_live_launch"), "{r:?}");
    assert_eq!(live.child.input().len(), n, "no byte");
    drop(taken);
    rig.stop().await;
}

/// PR10b-CL-13: a turn start whose observer finishes after a later card's hook finished
/// leaves that card's basis: the card is open in the store and its key is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn card_after_late_start() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let (parked, go, point) = gate_point();
    set(&sl.hooks.finished, point);
    let h = rig.hook_of(&live, "UserPromptSubmit", &json!({ "prompt": "go" }));
    let first = crate::supervisor::tests::support::http::spawn_send(&rig.srv, &h.req(&rig.srv));
    parked.await.unwrap();
    let c = card(&rig, &live, "t1").await;
    go.send(()).unwrap();
    assert_eq!(first.await.unwrap().status, 200);
    assert_eq!(outcome(&rig, c), None, "the card is open");
    let r = key(&rig, &live, &["enter"], Some(c)).await;
    assert_eq!(
        (r.0, r.1["outcome"].clone()),
        (200, json!("written")),
        "{r:?}"
    );
    rig.stop().await;
}

/// PR10b-CL-13: the card key's rule end to end, with cards opened through the hook route.
/// Each case stops the clock before its setup, so no request's deadline passes on a slow host
/// or while another request is parked; its setup's `SessionStart` and each card are taken in
/// with the send worker parked outside the live map.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(
    clippy::too_many_lines,
    reason = "one case per refusal path of send §2.6"
)]
async fn card_key_order() {
    // Written; a second card key to the same card (another `command_id`) is refused.
    let rig = SRig::new().await;
    rig.clock.stop();
    let live = rig.claude().await;
    let c = card(&rig, &live, "t1").await;
    let r = key(&rig, &live, &["down", "down", "enter"], Some(c)).await;
    assert_eq!(
        (r.0, r.1["outcome"].clone()),
        (200, json!("written")),
        "{r:?}"
    );
    let n = live.child.input().len();
    assert!(not_written(&key(&rig, &live, &["enter"], Some(c)).await));
    assert_eq!(live.child.input().len(), n, "no byte");
    // Two deciding tokens, or one before the last: 400.
    for keys in [["enter", "enter"], ["enter", "down"]] {
        let r = key(&rig, &live, &keys, Some(c)).await;
        assert_eq!(
            (r.0, r.1["code"].clone()),
            (400, json!("bad_args")),
            "{keys:?}"
        );
    }
    // A raw key between a card's observation and its card key: refused; a new card: written.
    let c2 = card(&rig, &live, "t2").await;
    assert_eq!(key(&rig, &live, &["escape"], None).await.0, 200);
    assert!(not_written(&key(&rig, &live, &["enter"], Some(c2)).await));
    let c3 = card(&rig, &live, "t3").await;
    assert_eq!(key(&rig, &live, &["enter"], Some(c3)).await.0, 200);
    rig.stop().await;

    // A `Stop` taken in while the card key is parked at its recheck, and at its boundary.
    for point in [ProbePoint::RecheckEntered, ProbePoint::Dispatching] {
        let rig = SRig::new().await;
        rig.clock.stop();
        let live = rig.claude().await;
        let c = card(&rig, &live, "t1").await;
        let probe = rig.probe(&live).await;
        let park = probe.park(point);
        let n = live.child.input().len();
        let args = json!({ "thread_id": live.t, "launch_id": live.l, "keys": ["enter"], "approval_id": c });
        let pending = crate::supervisor::tests::support::http::spawn_send(
            &rig.srv,
            &rig.srv.command(
                "key",
                &rig.owner,
                crate::supervisor::tests::cid(),
                &args.to_string(),
            ),
        );
        let p2 = probe.clone();
        assert!(
            tokio::task::spawn_blocking(move || p2.wait(
                point,
                1,
                Instant::now() + Duration::from_secs(10)
            ))
            .await
            .unwrap()
        );
        rig.hook(&live, "Stop", &json!({})).await;
        drop(park);
        let r = pending.await.unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(
            (r.status, v["code"].clone()),
            (409, json!("key_not_written")),
            "{point:?}"
        );
        assert_eq!(live.child.input().len(), n, "{point:?}: no byte");
        rig.stop().await;
    }

    // A hook's intake unfinished at the effect's start.
    let rig = SRig::new().await;
    rig.clock.stop();
    let live = rig.claude().await;
    let c = card(&rig, &live, "t1").await;
    let (got, release) = rig.ingest.points.arm_intake(rig.next_ordinal(&live));
    let h = rig.hook_of(&live, "Notification", &json!({}));
    let reply = crate::supervisor::tests::support::http::spawn_send(&rig.srv, &h.req(&rig.srv));
    got.await.unwrap();
    assert!(not_written(&key(&rig, &live, &["enter"], Some(c)).await));
    release.send(()).unwrap();
    assert_eq!(reply.await.unwrap().status, 200);
    rig.stop().await;

    // A raw key held in its write when the card's hook is taken in: the card's observation
    // shows a human unit unrecorded, so its card key is refused, also after the raw key
    // completes, until a new card arrives.
    let rig = SRig::new().await;
    rig.clock.stop();
    let live = rig.claude().await;
    let hold = live.child.hold_writes();
    let raw = {
        let args = json!({ "thread_id": live.t, "launch_id": live.l, "keys": ["escape"] });
        crate::supervisor::tests::support::http::spawn_send(
            &rig.srv,
            &rig.srv.command(
                "key",
                &rig.owner,
                crate::supervisor::tests::cid(),
                &args.to_string(),
            ),
        )
    };
    assert!(hold.wait_entered(Instant::now() + Duration::from_secs(10)));
    let c = card(&rig, &live, "t1").await;
    assert!(not_written(&key(&rig, &live, &["enter"], Some(c)).await));
    drop(hold);
    let _ = raw.await;
    rig.until("recorded", || human_writes(&rig, &live).len() == 1)
        .await;
    assert!(not_written(&key(&rig, &live, &["enter"], Some(c)).await));
    let c2 = card(&rig, &live, "t2").await;
    assert_eq!(key(&rig, &live, &["enter"], Some(c2)).await.0, 200);
    rig.stop().await;

    // A card opened by a retried delivery (`intake_reliable: false`).
    let rig = SRig::new().await;
    rig.clock.stop();
    let live = rig.claude().await;
    let h = rig
        .hook_of(
            &live,
            "PermissionRequest",
            &json!({ "tool_name": "Bash", "tool_input": {}, "tool_use_id": "t1" }),
        )
        .attempt(Some("2"));
    assert_eq!(rig.post(&h).await.status, 200);
    assert!(not_written(
        &key(&rig, &live, &["enter"], Some(ApprovalId(h.emit))).await
    ));
    // A closed card: card_closed.
    rig.hook(&live, "Stop", &json!({})).await;
    let r = key(&rig, &live, &["enter"], Some(ApprovalId(h.emit))).await;
    assert_eq!((r.0, r.1["code"].clone()), (409, json!("card_closed")));
    rig.stop().await;
}

/// PR10b-CL-16: `key`'s answers: `pending` when its unit is permitted and has no record in
/// time, `key_not_written` when the writer never reached it, after a DECCKM change, or when
/// the session was busy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn key_outcomes() {
    // Pending: held in its write.
    let rig = SRig::new().await;
    rig.clock.resume();
    let live = rig.claude().await;
    let hold = live.child.hold_writes();
    let r = key(&rig, &live, &["escape"], None).await;
    assert_eq!(
        (r.0, r.1["outcome"].clone()),
        (200, json!("pending")),
        "{r:?}"
    );
    assert!(r.1["bytes_written"].is_null());
    drop(hold);
    rig.until("recorded later", || human_writes(&rig, &live).len() == 1)
        .await;
    rig.stop().await;

    // Never reached: queued behind a held unit, cancelled.
    let rig = SRig::new().await;
    rig.clock.resume();
    let live = rig.claude().await;
    let hold = live.child.hold_writes();
    let far = rig.sup.real(rig.sup.now() + Duration::from_secs(60));
    human_unit(&rig, &live, WriteOrigin::Attach, Guard::until(far)).await;
    assert!(hold.wait_entered(Instant::now() + Duration::from_secs(10)));
    let r = key(&rig, &live, &["escape"], None).await;
    assert!(not_written(&r), "{r:?}");
    drop(hold);
    rig.stop().await;

    // DECCKM toggled between the encode and the writer reaching the unit (queued behind a
    // held input unit): the recheck refuses it.
    let rig = SRig::new().await;
    rig.clock.resume();
    let live = rig.claude().await;
    let hold = live.child.hold_writes();
    let _busy = rig.send(&live, "busy").await;
    assert!(hold.wait_entered(Instant::now() + Duration::from_secs(10)));
    let c = card(&rig, &live, "t1").await;
    let before = rig.modes(&live).await.unwrap();
    let args = json!({ "thread_id": live.t, "launch_id": live.l, "keys": ["up", "enter"], "approval_id": c });
    let pending = crate::supervisor::tests::support::http::spawn_send(
        &rig.srv,
        &rig.srv.command(
            "key",
            &rig.owner,
            crate::supervisor::tests::cid(),
            &args.to_string(),
        ),
    );
    let sl = rig.sl(&live);
    rig.until("the key unit was submitted", || {
        sl.state().human_unsettled == 1
    })
    .await;
    live.child.emit(b"\x1b[?1h");
    rig.until_async("DECCKM on", || async {
        rig.modes(&live).await.is_some_and(|m| m != before)
    })
    .await;
    drop(hold);
    let r = pending.await.unwrap();
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(
        (r.status, v["code"].clone()),
        (409, json!("key_not_written"))
    );
    assert!(
        live.child.input().ends_with(b"\x1b[201~\r"),
        "no card key byte"
    );
    rig.stop().await;

    // DECCKM toggled while the writer holds the key unit at its dispatch boundary: the mode
    // word waits for the queue lock, so the key is written in the form it was checked
    // against, before the change.
    let rig = SRig::new().await;
    rig.clock.resume();
    let live = rig.claude().await;
    let rx = rig.watch(&live).await;
    let probe = rig.probe(&live).await;
    let parsed = probe.count(ProbePoint::PieceParsing);
    let park = probe.park(ProbePoint::Dispatching);
    let args = json!({ "thread_id": live.t, "launch_id": live.l, "keys": ["up"] });
    let pending = crate::supervisor::tests::support::http::spawn_send(
        &rig.srv,
        &rig.srv.command(
            "key",
            &rig.owner,
            crate::supervisor::tests::cid(),
            &args.to_string(),
        ),
    );
    let p2 = probe.clone();
    assert!(
        tokio::task::spawn_blocking(move || p2.wait(
            ProbePoint::Dispatching,
            1,
            Instant::now() + Duration::from_secs(10)
        ))
        .await
        .unwrap()
    );
    live.child.emit(b"\x1b[?1h");
    let p3 = probe.clone();
    assert!(
        tokio::task::spawn_blocking(move || p3.wait(
            ProbePoint::PieceParsing,
            parsed + 1,
            Instant::now() + Duration::from_secs(10)
        ))
        .await
        .unwrap()
    );
    // Not delivered while the writer holds the queue lock (the drain waits for it).
    assert!(
        tokio::task::block_in_place(|| rx.recv_timeout(Duration::from_millis(500))).is_err(),
        "the mode change waits for the permission"
    );
    drop(park);
    let r = pending.await.unwrap();
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!((r.status, v["outcome"].clone()), (200, json!("written")));
    assert_eq!(
        live.child.input(),
        b"\x1b[A",
        "the CSI form checked against"
    );
    SRig::delivered(&rx, b"\x1b[?1h").await;
    rig.stop().await;

    // Busy: the op lock held past the effect's wait.
    let rig = SRig::new().await;
    rig.clock.resume();
    let live = rig.claude().await;
    let entry = rig.sup.entry(live.l).unwrap();
    let g = entry.ops.clone().lock_owned().await;
    let r = key(&rig, &live, &["escape"], None).await;
    assert!(not_written(&r), "{r:?}");
    drop(g);
    rig.stop().await;
}

/// PR10b-CL-3: a human unit submitted after the worker sampled the session for the open job,
/// still unrecorded when the job runs, keeps the input `queued` (the job decides on
/// `human_unsettled` too), with none of its bytes written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_waits_for_human() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let hold = live.child.hold_writes();
    let (entered, go, point) = gate_point();
    set(&sl.hooks.open_job, point);
    let id = rig.send(&live, "after-key").await;
    entered.await.unwrap();
    let far = rig.sup.real(rig.sup.now() + Duration::from_secs(60));
    human_unit(&rig, &live, WriteOrigin::Key, Guard::until(far)).await;
    assert!(hold.wait_entered(Instant::now() + Duration::from_secs(10)));
    assert_eq!(sl.state().human_unsettled, 1);
    go.send(()).unwrap();
    rig.pass(&live).await;
    assert_eq!(rig.input(id).state, InputState::Queued);
    drop(hold);
    rig.until("the key recorded", || human_writes(&rig, &live).len() == 1)
        .await;
    rig.pass(&live).await;
    assert_eq!(
        rig.input(id).state,
        InputState::Queued,
        "the key closed the gate"
    );
    assert!(!live.child.input().ends_with(b"after-key\x1b[201~\r"));
    rig.stop().await;
}

/// PR10b-CL-3: a human unit registered while the open job runs, after the job's first check
/// of the human units and before its read, keeps the input `queued`: the job's decision
/// checks again, after its last read, under the state the registration takes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_rechecks_human() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let hold = live.child.hold_writes();
    let (entered, go, point) = gate_point();
    set(&sl.hooks.open_after_flags, point);
    let id = rig.send(&live, "after-key").await;
    entered.await.unwrap();
    let far = rig.sup.real(rig.sup.now() + Duration::from_secs(60));
    human_unit(&rig, &live, WriteOrigin::Key, Guard::until(far)).await;
    assert!(hold.wait_entered(Instant::now() + Duration::from_secs(10)));
    assert_eq!(sl.state().human_unsettled, 1);
    go.send(()).unwrap();
    rig.pass(&live).await;
    assert_eq!(rig.input(id).state, InputState::Queued);
    drop(hold);
    rig.until("the key recorded", || human_writes(&rig, &live).len() == 1)
        .await;
    rig.pass(&live).await;
    assert_eq!(rig.input(id).state, InputState::Queued);
    assert!(!live.child.input().ends_with(b"after-key\x1b[201~\r"));
    rig.stop().await;
}

/// PR10b-CL-16: a key unit held at its dispatch boundary (the queue lock held) past the
/// report's deadline answers `pending` without waiting for the queue lock; released after
/// its dispatch deadline, it is refused with nothing written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn key_past_deadline_at_boundary() {
    let rig = SRig::new().await;
    rig.clock.resume();
    let live = rig.claude().await;
    let probe = rig.probe(&live).await;
    let park = probe.park(ProbePoint::Dispatching);
    let args = json!({ "thread_id": live.t, "launch_id": live.l, "keys": ["escape"] });
    let pending = crate::supervisor::tests::support::http::spawn_send(
        &rig.srv,
        &rig.srv.command(
            "key",
            &rig.owner,
            crate::supervisor::tests::cid(),
            &args.to_string(),
        ),
    );
    // The report's wait is real time (`KEY_WAIT` + `READ_WAIT`); the answer comes while the
    // writer still holds the queue lock.
    rig.until("answered while parked", || pending.is_finished())
        .await;
    let r = pending.await.unwrap();
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!((r.status, v["outcome"].clone()), (200, json!("pending")));
    // Released past its dispatch deadline: the writer refuses it, nothing written.
    drop(park);
    let sl = rig.sl(&live);
    rig.until("settled", || sl.state().human_unsettled == 0)
        .await;
    assert!(human_writes(&rig, &live).is_empty());
    assert!(live.child.input().is_empty());
    rig.stop().await;
}
