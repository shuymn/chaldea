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

fn count(rig: &SRig, live: &Live, kind: &str) -> usize {
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
async fn advance(rig: &SRig, live: &Live, by: Duration) {
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
    // A `PostDecision` without facts (the launch unobservable at its intake: the live map
    // held) records none.
    rig.stop().await;
    let (rig, live) = bound_in(ReadinessMode::PostDecision, Agent::Codex).await;
    let (locked_tx, locked) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let sup = rig.sup.clone();
    let holder = tokio::task::spawn_blocking(move || {
        let _map = sup.live.lock();
        let _ = locked_tx.send(());
        let _ = released.recv_timeout(Duration::from_secs(20));
    });
    locked.await.unwrap();
    rig.hook(&live, "PostDecision", &json!({})).await;
    release.send(()).unwrap();
    holder.await.unwrap();
    assert_eq!(count(&rig, &live, "post_decision"), 0);
    rig.hook(&live, "PostDecision", &json!({})).await;
    assert_eq!(count(&rig, &live, "post_decision"), 1);
    rig.stop().await;
}

/// A Codex launch with `ready` asserted and the clock stopped: settles come only from
/// [`advance`].
async fn settle_rig() -> (SRig, Live) {
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
        sup: Arc::downgrade(&rig.sup),
        side: rig.sup.send.clone(),
    });
    let ord = rig.next_ordinal(&live);
    let _ = p.facts(live.l, ord);
    drop(IntakeGuard::new(p, live.l, ord));
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

/// PR10b-CL-4: the candidate needs the current turn's start to follow an end the daemon saw:
/// out-of-order commits of a start and its `Stop` settle once both are in; a `ready` before
/// `P1`'s `Stop` leaves `P2`'s turn unsettled until `ready` and `P3`; a second `Stop` on an
/// `unknown` turn does not settle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_turn_order() {
    // The start's job held while its `Stop` commits.
    let (rig, live) = settle_rig().await;
    let start_ord = rig.next_ordinal(&live);
    let (got, release) = rig.ingest.points.arm_job(start_ord);
    let start = rig.hook_of(&live, "UserPromptSubmit", &json!({ "prompt": "p" }));
    let reply = crate::supervisor::tests::support::http::spawn_send(&rig.srv, &start.req(&rig.srv));
    got.await.unwrap();
    let stop = rig.hook_of(&live, "Stop", &json!({}));
    let stop_reply =
        crate::supervisor::tests::support::http::spawn_send(&rig.srv, &stop.req(&rig.srv));
    rig.until("the stop was taken in", || {
        rig.sl(&live).latest_intake.load(Ordering::SeqCst) == start_ord + 1
    })
    .await;
    release.send(()).unwrap();
    assert_eq!(reply.await.unwrap().status, 200);
    assert_eq!(stop_reply.await.unwrap().status, 200);
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 1);
    rig.stop().await;

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
