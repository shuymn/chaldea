//! Gate evidence (send §2.4): each kind by mode, and the settle completion.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use chaldea_core::adapter::ReadinessMode;
use chaldea_core::ids::{Agent, InputId};
use chaldea_core::input::InputState;
use chaldea_core::state::TurnState;
use serde_json::json;

use super::gate::set;
use super::{Live, SRig};
use crate::hook::{IntakeGuard, IntakeProbe};
use crate::supervisor::send::SETTLE;
use crate::supervisor::send::probe::SupervisorProbe;

/// The `readiness_observed` evidences of the thread, in order.
pub(crate) fn evidences(rig: &SRig, live: &Live) -> Vec<String> {
    rig.of_kind(live.t, "readiness_observed")
        .into_iter()
        .map(|p| p["evidence"].as_str().unwrap().to_owned())
        .collect()
}

pub(super) fn count(rig: &SRig, live: &Live, kind: &str) -> usize {
    evidences(rig, live).iter().filter(|k| *k == kind).count()
}

/// A turn: its `UserPromptSubmit` and its `Stop`.
async fn turn(rig: &SRig, live: &Live, prompt: &str) {
    rig.hook(live, "UserPromptSubmit", &json!({ "prompt": prompt }))
        .await;
    rig.hook(live, "Stop", &json!({})).await;
}

/// The commit instant of the turn log's highest entry.
fn last_at(rig: &SRig, live: &Live) -> Instant {
    rig.sl(live).state().log.0.values().next_back().unwrap().1
}

/// Moves the stopped clock `by` forward and runs a pass.
pub(super) async fn advance(rig: &SRig, live: &Live, by: Duration) {
    rig.clock.advance(by);
    rig.pass(live).await;
}

/// A launch of `agent` bound by its `SessionStart` with DECSET 2004 on, under `mode`.
async fn bound_in(mode: ReadinessMode, agent: Agent) -> (SRig, Live) {
    let rig = SRig::with(|c| c.readiness_mode = Some(mode)).await;
    let (live, _) = rig.spawn(agent, None).await;
    rig.paste(&live, true).await;
    rig.hook(&live, "SessionStart", &json!({})).await;
    (rig, live)
}

/// PR10b-CL-4: each evidence kind is recorded only where its mode and row allow it, with its
/// `status_changed` setting the turn `idle`; a retried hook and a hook without facts record
/// none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evidence_kinds() {
    for mode in [
        ReadinessMode::StopSettle,
        ReadinessMode::PostDecision,
        ReadinessMode::OwnerOnly,
    ] {
        for agent in [Agent::Claude, Agent::Codex] {
            let what = format!("{mode:?} {agent:?}");
            let (rig, live) = bound_in(mode, agent).await;
            let session_start = agent == Agent::Claude && mode != ReadinessMode::OwnerOnly;
            assert_eq!(
                count(&rig, &live, "session_start"),
                usize::from(session_start),
                "{what}"
            );
            if session_start {
                assert_eq!(rig.thread(live.t).state.turn, TurnState::Idle, "{what}");
                let basis = rig.launch(live.l).write_basis.unwrap();
                assert_eq!(basis.launch_id, live.l);
            }
            // A turn, then its settle.
            rig.clock.stop();
            turn(&rig, &live, "x").await;
            advance(&rig, &live, SETTLE).await;
            let settled = mode == ReadinessMode::StopSettle;
            assert_eq!(
                count(&rig, &live, "stop_settled"),
                usize::from(settled),
                "{what}"
            );
            assert_eq!(
                rig.thread(live.t).state.turn == TurnState::Idle,
                settled,
                "{what}"
            );
            // A decision hook.
            rig.hook(&live, "PostDecision", &json!({})).await;
            let decided = mode == ReadinessMode::PostDecision;
            assert_eq!(
                count(&rig, &live, "post_decision"),
                usize::from(decided),
                "{what}"
            );
            if decided {
                assert_eq!(rig.thread(live.t).state.turn, TurnState::Idle, "{what}");
            }
            // The owner's assertion, always.
            rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "y" }))
                .await;
            rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
                .await;
            assert_eq!(count(&rig, &live, "owner_asserted"), 1, "{what}");
            assert_eq!(rig.thread(live.t).state.turn, TurnState::Idle, "{what}");
            rig.stop().await;
        }
    }
    // A retried `SessionStart` (`intake_reliable: false`) binds without evidence.
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Claude, None).await;
    let h = rig
        .hook_of(&live, "SessionStart", &json!({}))
        .attempt(Some("2"));
    assert_eq!(rig.post(&h).await.status, 200);
    assert!(rig.launch(live.l).session_id.is_some(), "bound");
    assert_eq!(count(&rig, &live, "session_start"), 0);
    // A `PostDecision` without facts (the launch unobservable at its intake) records none.
    rig.stop().await;
    let (rig, live) = bound_in(ReadinessMode::PostDecision, Agent::Codex).await;
    let sl = rig.sl(&live);
    *crate::supervisor::send::lock(&sl.hooks.observe) = Some(Box::new(|_, _| {
        crate::supervisor::live::LaunchObservation::Unobservable
    }));
    rig.hook(&live, "PostDecision", &json!({})).await;
    assert!(crate::supervisor::send::lock(&sl.hooks.observe).is_none());
    assert_eq!(count(&rig, &live, "post_decision"), 0);
    rig.hook(&live, "PostDecision", &json!({})).await;
    assert_eq!(count(&rig, &live, "post_decision"), 1);
    rig.stop().await;
}

/// A Codex launch with `ready` asserted and the clock stopped: settles come only from
/// [`advance`].
pub(super) async fn settle_rig() -> (SRig, Live) {
    let rig = SRig::new().await;
    let live = rig.bound(Agent::Codex).await;
    rig.clock.stop();
    (rig, live)
}

/// PR10b-CL-4: the settle completion comes `SETTLE` after the `Stop`'s commit on the
/// monotonic clock, not before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_timing() {
    let (rig, live) = settle_rig().await;
    // A VT reply before the turn: the settle's basis counts it.
    live.child.emit(b"\x1b[6n");
    rig.until("the reply was written", || !live.child.input().is_empty())
        .await;
    turn(&rig, &live, "p").await;
    assert_eq!(rig.thread(live.t).state.turn, TurnState::Unknown);
    let at = last_at(&rig, &live);
    assert_eq!(at, rig.sup.now());
    advance(&rig, &live, SETTLE - Duration::from_millis(1)).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    advance(&rig, &live, Duration::from_millis(1)).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 1);
    assert_eq!(rig.thread(live.t).state.turn, TurnState::Idle);
    // The basis is sampled when the worker builds the job.
    let basis = rig.launch(live.l).write_basis.unwrap();
    let pos = rig
        .sup
        .entry(live.l)
        .unwrap()
        .published()
        .unwrap()
        .observer
        .last_started_each();
    assert_eq!(basis.position, pos.into_iter().max().unwrap());
    assert!(basis.position >= 1);
    rig.stop().await;
}

/// PR10b-CL-4: a later hook ends the candidate: a raw hook; a hook taken in and held before
/// its job; a hook taken in after the pass chose the candidate, before the settle job; a hook
/// whose request ended before its job was submitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_later_hooks() {
    // A later raw hook.
    let (rig, live) = settle_rig().await;
    turn(&rig, &live, "p").await;
    rig.hook(&live, "Notification", &json!({})).await;
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    rig.stop().await;

    // A later hook held before its job: the fence is unfinished, then its mark ends it.
    let (rig, live) = settle_rig().await;
    turn(&rig, &live, "p").await;
    let (got, release) = rig.ingest.points.arm_job(rig.next_ordinal(&live));
    let h = rig.hook_of(&live, "Notification", &json!({}));
    let reply = crate::supervisor::tests::support::http::spawn_send(&rig.srv, &h.req(&rig.srv));
    got.await.unwrap();
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    release.send(()).unwrap();
    // The clock moved past the hook's deadline while it was held: its job expired, so its
    // intake finished without a mark, and the candidate stays ended by it.
    assert_eq!(reply.await.unwrap().status, 408);
    rig.pass(&live).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    rig.stop().await;

    // A hook taken in after the pass chose the candidate, before the settle job ran.
    let (rig, live) = settle_rig().await;
    turn(&rig, &live, "p").await;
    let sl = rig.sl(&live);
    let (srv_port, h) = (rig.srv.port, rig.hook_of(&live, "Notification", &json!({})));
    let bytes = h.req(&rig.srv).bytes();
    let word = sl.fence.load(Ordering::SeqCst);
    let sl2 = sl.clone();
    let rt = tokio::runtime::Handle::current();
    set(&sl.hooks.settle_job, move || {
        rt.spawn(
            async move { crate::supervisor::tests::support::http::send(srv_port, &bytes).await },
        );
        let end = Instant::now() + Duration::from_secs(10);
        while sl2.fence.load(Ordering::SeqCst) == word {
            assert!(Instant::now() < end, "the hook was taken in");
            std::thread::yield_now();
        }
    });
    advance(&rig, &live, SETTLE).await;
    rig.until("the hook committed", || {
        rig.of_kind(live.t, "raw_unknown").len() == 1
    })
    .await;
    rig.pass(&live).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    rig.stop().await;

    // A hook taken in after the `Stop` whose request ended before its job was submitted.
    let (rig, live) = settle_rig().await;
    turn(&rig, &live, "p").await;
    let p: Arc<dyn IntakeProbe> = Arc::new(SupervisorProbe {
        side: rig.sup.send.clone(),
    });
    let ord = rig.next_ordinal(&live);
    let g = IntakeGuard::new(p.clone(), live.l, ord);
    let _ = p.facts(live.l, ord, Instant::now() + Duration::from_secs(10), &g);
    drop(g);
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    rig.stop().await;
}

/// Holds the input unit's first write call and returns the hold and the input.
async fn running_unit(
    rig: &SRig,
    live: &Live,
    text: &str,
) -> (chaldea_pty_runtime::fake::WriteHold, InputId) {
    let hold = live.child.hold_writes();
    let id = rig.send(live, text).await;
    assert!(hold.wait_entered(Instant::now() + Duration::from_secs(10)));
    (hold, id)
}

/// PR10b-CL-4: a `Stop` taken in while an input unit may still write never settles, even
/// after the unit ends: one taken in while the unit is held in its write, and one taken in
/// after the unit's first permission while `submit`'s return and the unit's completion are
/// held (`running` is set before `submit`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_running() {
    // Held in its write.
    let (rig, live) = settle_rig().await;
    let (hold, id) = running_unit(&rig, &live, "held").await;
    rig.hook(&live, "Stop", &json!({})).await;
    drop(hold);
    rig.until_input(id, InputState::Sent).await;
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    rig.stop().await;

    // After the permission, while `submit` has not returned.
    let (rig, live) = settle_rig().await;
    let sl = rig.sl(&live);
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let (entered_tx, entered) = tokio::sync::oneshot::channel::<()>();
    set(&sl.hooks.after_submit, move || {
        let _ = entered_tx.send(());
        let _ = rx.recv_timeout(Duration::from_secs(20));
    });
    // The unit's completion held too: the `Stop` is taken in before it is `Written`.
    let (complete, held) = std::sync::mpsc::channel::<()>();
    set(&sl.hooks.completion, move || {
        let _ = held.recv_timeout(Duration::from_secs(20));
    });
    let probe = rig.probe(&live).await;
    let id = rig.send(&live, "permitted").await;
    entered.await.unwrap();
    let p2 = probe.clone();
    assert!(
        tokio::task::spawn_blocking(move || p2.wait(
            chaldea_pty_runtime::fake::ProbePoint::Permitted,
            1,
            Instant::now() + Duration::from_secs(10)
        ))
        .await
        .unwrap()
    );
    rig.hook(&live, "Stop", &json!({})).await;
    let marks: Vec<_> = sl.state().log.0.values().map(|(m, _)| *m).collect();
    assert_eq!(
        marks.last(),
        Some(&crate::hook::TurnMark::End { settle: false }),
        "the facts showed the unit running"
    );
    complete.send(()).unwrap();
    tx.send(()).unwrap();
    rig.until_input(id, InputState::Sent).await;
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    // New evidence is needed: a turn of its own settles.
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "permitted" }))
        .await;
    assert_eq!(rig.input(id).state, InputState::Acked);
    rig.hook(&live, "Stop", &json!({})).await;
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 1);
    rig.stop().await;
}

/// PR10b-CL-4: the settle clock runs from the `Stop`'s commit, however late its observer
/// finishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_from_commit() {
    let (rig, live) = settle_rig().await;
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "p" }))
        .await;
    let sl = rig.sl(&live);
    let committed = Arc::new(std::sync::Mutex::new(None));
    let (c2, clock) = (committed.clone(), rig.clock.clone());
    set(&sl.hooks.finished, move || {
        *c2.lock().unwrap() = Some(crate::http::MonotonicClock::now(&*clock));
        clock.advance(Duration::from_secs(1));
    });
    rig.hook(&live, "Stop", &json!({})).await;
    let t0 = committed.lock().unwrap().unwrap();
    assert_eq!(last_at(&rig, &live), t0);
    // The clock is 1 s past the commit: `SETTLE` after it is 1 s short of `SETTLE` from now.
    advance(
        &rig,
        &live,
        SETTLE - Duration::from_secs(1) - Duration::from_millis(1),
    )
    .await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    advance(&rig, &live, Duration::from_millis(1)).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 1);
    rig.stop().await;
}

/// PR10b-CL-4: the settle clock runs from the instant right after the `Stop`'s SQLite
/// `COMMIT`, not from the end of its job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_from_sqlite_commit() {
    let (rig, live) = settle_rig().await;
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "p" }))
        .await;
    let ran = Arc::new(std::sync::Mutex::new(None));
    let (r2, clock) = (ran.clone(), rig.clock.clone());
    *rig.srv.store.store().faults().before_commit.lock().unwrap() = Some(Box::new(move || {
        *r2.lock().unwrap() = Some(crate::http::MonotonicClock::now(&*clock));
        clock.advance(Duration::from_secs(1));
    }));
    rig.hook(&live, "Stop", &json!({})).await;
    let t0 = ran.lock().unwrap().unwrap() + Duration::from_secs(1);
    assert_eq!(last_at(&rig, &live), t0);
    advance(&rig, &live, SETTLE - Duration::from_millis(1)).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    advance(&rig, &live, Duration::from_millis(1)).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 1);
    rig.stop().await;
}

/// PR10b-CL-4 (P-1): a `Stop` taken in after the input's unit was written and acked, before
/// the completion job recorded it, is a candidate decided again after the record: it settles,
/// and the next send is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_after_input_record() {
    let (rig, live) = settle_rig().await;
    let sl = rig.sl(&live);
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let (entered_tx, entered) = tokio::sync::oneshot::channel::<()>();
    set(&sl.hooks.record_job, move || {
        let _ = entered_tx.send(());
        let _ = rx.recv_timeout(Duration::from_secs(20));
    });
    let id = rig.send(&live, "p").await;
    entered.await.unwrap();
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "p" }))
        .await;
    assert_eq!(rig.input(id).state, InputState::Acked);
    rig.hook(&live, "Stop", &json!({})).await;
    assert!(sl.state().running.is_some(), "not yet recorded");
    tx.send(()).unwrap();
    rig.until("the unit was recorded", || sl.state().running.is_none())
        .await;
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 1);
    let next = rig.send(&live, "q").await;
    rig.until_input(next, InputState::Sent).await;
    rig.stop().await;
}

/// PR10b-CL-4: a `Stop` taken in while the next input's unit is `Written` but neither recorded
/// nor acked (the previous turn's late `Stop`) is no candidate: the input's own turn may still
/// run, so a later send waits for new evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_before_next_ack() {
    let (rig, live) = settle_rig().await;
    let sl = rig.sl(&live);
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "p1" }))
        .await;
    // P1's `Stop` is held back; the owner asserts the thread ready.
    rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
        .await;
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let (entered_tx, entered) = tokio::sync::oneshot::channel::<()>();
    set(&sl.hooks.record_job, move || {
        let _ = entered_tx.send(());
        let _ = rx.recv_timeout(Duration::from_secs(20));
    });
    let p2 = rig.send(&live, "p2").await;
    entered.await.unwrap();
    // P2's `UserPromptSubmit` is not delivered; P1's `Stop` arrives.
    rig.hook(&live, "Stop", &json!({})).await;
    tx.send(()).unwrap();
    rig.until("the unit was recorded", || sl.state().running.is_none())
        .await;
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    advance(&rig, &live, crate::supervisor::send::ACK_WINDOW).await;
    rig.until_input(p2, InputState::Unconfirmed).await;
    rig.cmd_ok(
        "input_resolve",
        &json!({ "input_id": p2, "delivered": true }),
        200,
    )
    .await;
    let p3 = rig.send(&live, "p3").await;
    rig.pass(&live).await;
    assert_eq!(rig.input(p3).state, InputState::Queued);
    rig.stop().await;
}

/// PR10b-CL-4: an owner's `input resolve --delivered` is no hook attribution: a late `Stop`
/// taken in while the next input's unit is `Written`, unrecorded, and acked only by the owner
/// is no candidate, so a later send waits for new evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_before_owner_ack() {
    let (rig, live) = settle_rig().await;
    let sl = rig.sl(&live);
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "p1" }))
        .await;
    // P1's `Stop` is held back; the owner asserts the thread ready.
    rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
        .await;
    let hold = live.child.hold_writes();
    let p2 = rig.send(&live, "p2").await;
    assert!(hold.wait_entered(Instant::now() + Duration::from_secs(10)));
    let t0 = rig.sup.now();
    let stalled = t0 + crate::supervisor::send::WRITE_STALL + Duration::from_secs(1);
    super::outcomes::tick_at(&rig, &live, stalled).await;
    assert_eq!(rig.input(p2).state, InputState::Unconfirmed);
    rig.clock.advance(stalled - t0);
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let (entered_tx, entered) = tokio::sync::oneshot::channel::<()>();
    set(&sl.hooks.record_job, move || {
        let _ = entered_tx.send(());
        let _ = rx.recv_timeout(Duration::from_secs(20));
    });
    drop(hold);
    entered.await.unwrap();
    // P2's `UserPromptSubmit` is not delivered; the owner resolves it delivered, then P1's
    // `Stop` arrives.
    rig.cmd_ok(
        "input_resolve",
        &json!({ "input_id": p2, "delivered": true }),
        200,
    )
    .await;
    assert_eq!(rig.input(p2).state, InputState::Acked);
    rig.hook(&live, "Stop", &json!({})).await;
    tx.send(()).unwrap();
    rig.until("the unit was recorded", || sl.state().running.is_none())
        .await;
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    let p3 = rig.send(&live, "p3").await;
    rig.pass(&live).await;
    assert_eq!(rig.input(p3).state, InputState::Queued);
    rig.stop().await;
}

/// PR10b-CL-4/12: a `Stop` taken in while a card key's completion is held settles only after
/// that key's `human_write`, so the next send is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_after_human_record() {
    let (rig, live) = settle_rig().await;
    let sl = rig.sl(&live);
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "p" }))
        .await;
    let h = rig.hook_of(
        &live,
        "PermissionRequest",
        &json!({ "tool_name": "Bash", "tool_input": { "command": "ls" }, "tool_use_id": "t1" }),
    );
    assert_eq!(rig.post(&h).await.status, 200);
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let (entered_tx, entered) = tokio::sync::oneshot::channel::<()>();
    set(&sl.hooks.completion, move || {
        let _ = entered_tx.send(());
        let _ = rx.recv_timeout(Duration::from_secs(20));
    });
    let args = json!({ "thread_id": live.t, "launch_id": live.l, "keys": ["enter"], "approval_id": h.emit });
    let key = crate::supervisor::tests::support::http::spawn_send(
        &rig.srv,
        &rig.srv.command(
            "key",
            &rig.owner,
            crate::supervisor::tests::cid(),
            &args.to_string(),
        ),
    );
    entered.await.unwrap();
    rig.hook(&live, "Stop", &json!({})).await;
    advance(&rig, &live, SETTLE).await;
    assert_eq!(
        count(&rig, &live, "stop_settled"),
        0,
        "the key is not recorded"
    );
    tx.send(()).unwrap();
    rig.until("the settle after the key's record", || {
        count(&rig, &live, "stop_settled") == 1
    })
    .await;
    assert_eq!(key.await.unwrap().status, 200);
    let next = rig.send(&live, "after").await;
    rig.until_input(next, InputState::Sent).await;
    rig.stop().await;
}

/// PR10b-CL-4 (P-1): a Codex launch whose first turn's `UserPromptSubmit` and `Stop` are
/// taken in before the binding `SessionStart` committed (both buffered, promoted at the
/// binding) acks the positional input. Received after the `SessionStart`, the promoted `Stop`
/// is the log's highest entry and settles, so the next send is written; received before it,
/// the binding's own mark is the highest entry (a later hook ends a candidate, §2.4 step 2),
/// so the next send waits for `ready`, as after any turn the log cannot place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn buffered_stop() {
    for stop_first in [false, true] {
        let rig = SRig::new().await;
        let (live, v) = rig.spawn(Agent::Codex, Some("first")).await;
        let id: InputId = serde_json::from_value(v["input_id"].clone()).unwrap();
        rig.paste(&live, true).await;
        rig.clock.stop();
        if stop_first {
            rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "first" }))
                .await;
            rig.hook(&live, "Stop", &json!({})).await;
            rig.hook(&live, "SessionStart", &json!({})).await;
        } else {
            let (got, release) = rig.ingest.points.arm_job(rig.next_ordinal(&live));
            let h = rig.hook_of(&live, "SessionStart", &json!({}));
            let binding =
                crate::supervisor::tests::support::http::spawn_send(&rig.srv, &h.req(&rig.srv));
            got.await.unwrap();
            rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "first" }))
                .await;
            rig.hook(&live, "Stop", &json!({})).await;
            release.send(()).unwrap();
            assert_eq!(binding.await.unwrap().status, 200);
        }
        assert!(rig.launch(live.l).session_id.is_some(), "bound");
        assert_eq!(
            rig.input(id).state,
            InputState::Acked,
            "stop first: {stop_first}"
        );
        advance(&rig, &live, SETTLE).await;
        assert_eq!(
            count(&rig, &live, "stop_settled"),
            usize::from(!stop_first),
            "stop first: {stop_first}"
        );
        let next = rig.send(&live, "next").await;
        if stop_first {
            rig.pass(&live).await;
            assert_eq!(rig.input(next).state, InputState::Queued);
            rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
                .await;
        }
        rig.until_input(next, InputState::Sent).await;
        rig.stop().await;
    }
}

/// `FakeDecoder` that runs a one-shot test point inside the job of the next `Stop`.
struct HoldStop(std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>);

impl crate::supervisor::send::HookDecoder for HoldStop {
    fn decode(
        &self,
        tx: &crate::store::ReadTx<'_>,
        hook: &crate::hook::AcceptedHook<'_>,
    ) -> Result<
        (
            crate::supervisor::send::hooks::HookFact,
            Vec<crate::hook::HookEvent>,
        ),
        crate::store::StoreError,
    > {
        if hook.event_name == "Stop" {
            let f = self.0.lock().unwrap().take();
            if let Some(f) = f {
                f();
            }
        }
        crate::supervisor::send::HookDecoder::decode(
            &crate::supervisor::send::fake_decoder::FakeDecoder,
            tx,
            hook,
        )
    }
}

/// PR10b-CL-4 (P-1): a `Stop` whose request ended `outcome_unknown` (its job still running
/// at the reply cutoff) and whose job committed after it reaches the turn log: it settles, and
/// the next send is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_after_outcome_unknown() {
    let hold = Arc::new(HoldStop(std::sync::Mutex::new(None)));
    let h2 = hold.clone();
    let rig = SRig::with(move |c| c.decoders = crate::supervisor::send::Decoders::all(h2)).await;
    let live = rig.bound(Agent::Codex).await;
    rig.clock.stop();
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "p" }))
        .await;
    let (entered_tx, entered) = tokio::sync::oneshot::channel::<()>();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    *hold.0.lock().unwrap() = Some(Box::new(move || {
        let _ = entered_tx.send(());
        let _ = wait.recv_timeout(Duration::from_secs(20));
    }));
    let h = rig.hook_of(&live, "Stop", &json!({}));
    let stop = crate::supervisor::tests::support::http::spawn_send(&rig.srv, &h.req(&rig.srv));
    entered.await.unwrap();
    // Past the request's reply cutoff while its job runs.
    rig.clock.advance(Duration::from_secs(10));
    go.send(()).unwrap();
    let r = stop.await.unwrap();
    assert_eq!(
        (r.status, r.code()),
        (503, Some("outcome_unknown".to_owned())),
        "{r:?}"
    );
    rig.until("the mark reached the log", || {
        rig.sl(&live)
            .state()
            .log
            .0
            .values()
            .next_back()
            .map(|(m, _)| *m)
            == Some(crate::hook::TurnMark::End { settle: true })
    })
    .await;
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 1);
    let next = rig.send(&live, "next").await;
    rig.until_input(next, InputState::Sent).await;
    rig.stop().await;
}

/// PR10b-CL-4: the candidate needs the current turn's start to follow an end the daemon saw:
/// a `ready` before `P1`'s `Stop` leaves `P2`'s turn unsettled until `ready` and `P3`; a
/// second `Stop` on an `unknown` turn does not settle. Both commit orders of a start and its
/// `Stop` are in `turn_order`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_turn_order() {
    // `ready` before P1's `Stop`; P2 sent and acked; P1's `Stop`; P2's `Stop`.
    let (rig, live) = settle_rig().await;
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "P1" }))
        .await;
    rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
        .await;
    let p2 = rig.send(&live, "P2").await;
    rig.until_input(p2, InputState::Sent).await;
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "P2" }))
        .await;
    assert_eq!(rig.input(p2).state, InputState::Acked);
    rig.hook(&live, "Stop", &json!({})).await;
    rig.hook(&live, "Stop", &json!({})).await;
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    // Then `ready`, P3, and its `Stop`: settles.
    rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
        .await;
    let p3 = rig.send(&live, "P3").await;
    rig.until_input(p3, InputState::Sent).await;
    turn(&rig, &live, "P3").await;
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 1);
    // A second `Stop` on the `unknown` turn after a new start does not settle twice.
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "P4" }))
        .await;
    rig.hook(&live, "Stop", &json!({})).await;
    rig.hook(&live, "Stop", &json!({})).await;
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 1);
    rig.stop().await;
}

/// L-11 (accepted limit): a turn whose `UserPromptSubmit` was lost, its `Stop` taken in
/// after the next turn's start (sent after `ready`), settles while that turn runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_limit_l11() {
    let (rig, live) = settle_rig().await;
    // P1 runs; its `UserPromptSubmit` is lost. The owner asserts readiness and sends P2.
    rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
        .await;
    let p2 = rig.send(&live, "P2").await;
    rig.until_input(p2, InputState::Sent).await;
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": "P2" }))
        .await;
    // P1's late `Stop`.
    rig.hook(&live, "Stop", &json!({})).await;
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 1, "the accepted gap");
    rig.stop().await;
}

/// A Codex `Stop` turn whose settle is due: `ready` asserted, the clock stopped, the turn's
/// hooks in, and the clock moved `SETTLE` on without a pass.
async fn due_settle() -> (SRig, Live) {
    let (rig, live) = settle_rig().await;
    turn(&rig, &live, "p").await;
    rig.clock.advance(SETTLE);
    (rig, live)
}

/// SF-11: the settle's one observation takes no lock: with the PTY writer parked at its
/// dispatch boundary (its queue lock held), the due candidate settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_beside_parked_writer() {
    let (rig, live) = due_settle().await;
    let (park, _) = rig.writer_q_held(&live).await;
    rig.pass(&live).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 1);
    drop(park);
    rig.stop().await;
}

/// SF-11: with the live map held from the pass's read to its settle, the due candidate
/// settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_beside_held_map() {
    let (rig, live) = due_settle().await;
    let sl = rig.sl(&live);
    let (read_tx, read) = std::sync::mpsc::channel();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    set(&sl.hooks.after_read, move || {
        read_tx.send(()).unwrap();
        let _ = wait.recv_timeout(Duration::from_secs(30));
    });
    sl.wake.notify_one();
    crate::supervisor::tests::support::http::reached_sync(read).await;
    let (locked_tx, locked) = std::sync::mpsc::channel();
    let (free, until) = std::sync::mpsc::channel::<()>();
    let sup = rig.sup.clone();
    let holder = std::thread::spawn(move || {
        let _map = sup.live.lock();
        locked_tx.send(()).unwrap();
        let _ = until.recv_timeout(Duration::from_secs(30));
    });
    locked.recv().unwrap();
    go.send(()).unwrap();
    rig.until("settled with the map held", || {
        count(&rig, &live, "stop_settled") == 1
    })
    .await;
    free.send(()).unwrap();
    holder.join().unwrap();
    rig.stop().await;
}

/// SF-11 (kept): a launch removed from the live map between the pass's read and its
/// observation does not settle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_after_removal() {
    let (rig, live) = due_settle().await;
    let sl = rig.sl(&live);
    let sup = rig.sup.clone();
    let l = live.l;
    set(&sl.hooks.after_read, move || sup.remove(l));
    rig.pass(&live).await;
    assert!(super::super::lock(&sl.hooks.after_read).is_none());
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    rig.stop().await;
}

/// SF-11: a launch removed between the settle's observation and its job (no exit recorded)
/// does not settle: the job refuses a removed entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_removed_before_job() {
    let (rig, live) = due_settle().await;
    let sl = rig.sl(&live);
    let sup = rig.sup.clone();
    let l = live.l;
    set(&sl.hooks.settle_job, move || sup.remove(l));
    rig.pass(&live).await;
    assert!(
        super::super::lock(&sl.hooks.settle_job).is_none(),
        "the job ran"
    );
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    rig.stop().await;
}
