//! Ack attribution (send §2.5): when a hook acks its input, and when it cannot.

use std::time::{Duration, Instant};

use chaldea_core::ids::{Agent, InputId};
use chaldea_core::input::InputState;
use chaldea_pty_runtime::fake::ProbePoint;
use serde_json::json;

use super::gate::set;
use super::outcomes::tick_at;
use super::{Live, SRig};
use crate::supervisor::send::ACK_WINDOW;

/// The `input_id` of the thread's last `user_message`, if it names one.
fn last_ack(rig: &SRig, live: &Live) -> Option<InputId> {
    rig.of_kind(live.t, "user_message")
        .last()
        .and_then(|m| serde_json::from_value(m["input_id"].clone()).ok())
}

async fn prompt(rig: &SRig, live: &Live, text: &str) {
    rig.hook(live, "UserPromptSubmit", &json!({ "prompt": text }))
        .await;
}

/// Blocks a test point until the returned sender is used; the receiver hears it entered.
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

/// PR10b-CL-8: a hook taken in after the unit completed `Written` acks the input, before or
/// after its completion job committed `sent`; one taken in before the completion ran, or
/// after an `Incomplete` record, does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ack_while_sending() {
    // The completion ran; its job held: acked from `sending`.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (entered, go, point) = gate_point();
    set(&rig.sl(&live).hooks.record_job, point);
    let id = rig.send(&live, "fast").await;
    entered.await.unwrap();
    assert_eq!(rig.input(id).state, InputState::Sending);
    prompt(&rig, &live, "fast").await;
    assert_eq!(rig.input(id).state, InputState::Acked);
    go.send(()).unwrap();
    rig.pass(&live).await;
    assert_eq!(
        rig.input(id).state,
        InputState::Acked,
        "the late completion changes nothing"
    );
    rig.stop().await;

    // The completion runs before `submit` returns: `written` is still set.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let (entered, go, point) = gate_point();
    set(&sl.hooks.after_submit, point);
    let id = rig.send(&live, "early").await;
    entered.await.unwrap();
    rig.until("the completion fired", || {
        sl.outstanding.load(std::sync::atomic::Ordering::SeqCst) == 0
    })
    .await;
    prompt(&rig, &live, "early").await;
    assert_eq!(rig.input(id).state, InputState::Acked);
    go.send(()).unwrap();
    rig.stop().await;

    // The completion held after the last write returned: not acked, then `sent`.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (entered, go, point) = gate_point();
    set(&rig.sl(&live).hooks.completion, point);
    let id = rig.send(&live, "held").await;
    entered.await.unwrap();
    prompt(&rig, &live, "held").await;
    assert_eq!(last_ack(&rig, &live), None);
    go.send(()).unwrap();
    rig.until_input(id, InputState::Sent).await;
    rig.stop().await;

    // Parked in the unit's pause (the submit not written): not acked.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let probe = rig.probe(&live).await;
    let park = probe.park(ProbePoint::PauseEntered);
    let id = rig.send(&live, "paused").await;
    let p2 = probe.clone();
    assert!(
        tokio::task::spawn_blocking(move || p2.wait(
            ProbePoint::PauseEntered,
            1,
            Instant::now() + Duration::from_secs(10)
        ))
        .await
        .unwrap()
    );
    prompt(&rig, &live, "paused").await;
    assert_eq!(last_ack(&rig, &live), None);
    drop(park);
    rig.until_input(id, InputState::Sent).await;
    rig.stop().await;

    // The submit part fails: `unconfirmed`, and a later hook does not name it.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let len = u64::try_from(b"\x1b[200~broken\x1b[201~".len()).unwrap();
    live.child
        .fail_writes_after(len, std::io::ErrorKind::BrokenPipe);
    let id = rig.send(&live, "broken").await;
    rig.until_input(id, InputState::Unconfirmed).await;
    prompt(&rig, &live, "broken").await;
    assert_eq!(last_ack(&rig, &live), None);
    assert_eq!(rig.input(id).state, InputState::Unconfirmed);
    rig.stop().await;
}

/// PR10b-CL-7/8: the probe's one observation point orders a completion against the intake.
/// A completion released while the intake holds the launch's state, after `written` was read
/// and before the fence advance or after it, publishes `written` only once the intake is
/// done: the hook's facts do not show the unit written, it names no `input_id`, and the
/// input goes on to `sent`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completion_during_intake() {
    for after_advance in [false, true] {
        let rig = SRig::new().await;
        let live = rig.claude().await;
        let sl = rig.sl(&live);
        // The worker waits after its `submit`, so the launch's state stays free for the probe.
        let (submitted, resume, after_submit) = gate_point();
        set(&sl.hooks.after_submit, after_submit);
        let (entered, go, point) = gate_point();
        set(&sl.hooks.completion, point);
        let id = rig.send(&live, "P").await;
        submitted.await.unwrap();
        entered.await.unwrap();
        let written = sl.state().dispatch.as_ref().unwrap().written.clone();
        let inside = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (w, i) = (written.clone(), inside.clone());
        // Inside the intake: the completion is released and given time to publish.
        let release = move || {
            go.send(()).unwrap();
            let until = Instant::now() + Duration::from_millis(500);
            while Instant::now() < until {
                if w.load(std::sync::atomic::Ordering::Acquire) {
                    i.store(true, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
                std::thread::yield_now();
            }
        };
        if after_advance {
            *crate::supervisor::send::lock(&sl.hooks.observe) = Some(Box::new(move |sup, l| {
                release();
                sup.observe(l)
            }));
        } else {
            set(&sl.hooks.after_written_read, release);
        }
        prompt(&rig, &live, "P").await;
        assert!(crate::supervisor::send::lock(&sl.hooks.observe).is_none());
        assert!(crate::supervisor::send::lock(&sl.hooks.after_written_read).is_none());
        assert!(
            !inside.load(std::sync::atomic::Ordering::SeqCst),
            "the completion published inside the intake (after_advance: {after_advance})"
        );
        assert_eq!(last_ack(&rig, &live), None);
        rig.until("the completion published", || {
            written.load(std::sync::atomic::Ordering::Acquire)
        })
        .await;
        resume.send(()).unwrap();
        rig.until_input(id, InputState::Sent).await;
        rig.stop().await;
    }
}

/// PR10b-CL-8: a `sent` input whose ack window expired is `unconfirmed`; a hook that arrives
/// for it while its launch lives acks it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_ack() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let id = rig.send(&live, "late").await;
    rig.until_input(id, InputState::Sent).await;
    rig.clock.stop();
    // The window runs from the completion job's commit, which the worker records.
    let sl = rig.sl(&live);
    rig.until("the window started", || {
        sl.state().sent_at.contains_key(&id)
    })
    .await;
    let t0 = sl.state().sent_at[&id];
    tick_at(&rig, &live, t0 + ACK_WINDOW - Duration::from_millis(1)).await;
    assert_eq!(rig.input(id).state, InputState::Sent);
    tick_at(&rig, &live, t0 + ACK_WINDOW).await;
    rig.until_input(id, InputState::Unconfirmed).await;
    prompt(&rig, &live, "late").await;
    assert_eq!(rig.input(id).state, InputState::Acked);
    assert!(
        !rig.thread(live.t)
            .state
            .attention
            .contains(&chaldea_core::state::AttentionCause::InputUnconfirmed { input_id: id })
    );
    rig.stop().await;
}

/// PR10b-CL-20: what the CLI prints changes no state: a prompt echo and hook-shaped text on
/// a bound launch with evidence and a `sent` input leave the thread and the input as they
/// were, with no `user_message`, until the ack window expires the input.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn banner_never_acks() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let id = rig.send(&live, "hello").await;
    rig.until_input(id, InputState::Sent).await;
    let before = (rig.thread(live.t).state, rig.events(live.t).len());
    live.child.emit(
        b"hello\r\n\
UserPromptSubmit {\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"hello\"}\r\n\
SessionStart session_id=00000000-0000-0000-0000-000000000000\r\n\
Stop\r\nAllow this command? [y/n]\r\n> \r\n",
    );
    let entry = rig.sup.entry(live.l).unwrap();
    rig.until_async("the banner parsed", || {
        let (sup, entry) = (rig.sup.clone(), entry.clone());
        async move {
            entry
                .snapshot_before(&sup, sup.now() + Duration::from_secs(5))
                .await
                .is_some_and(|s| {
                    s.lines
                        .iter()
                        .any(|l| l.text().contains("Allow this command?"))
                })
        }
    })
    .await;
    rig.pass(&live).await;
    assert_eq!(
        (rig.thread(live.t).state, rig.events(live.t).len()),
        before,
        "output changed nothing"
    );
    assert_eq!(rig.input(id).state, InputState::Sent);
    rig.clock.stop();
    let t0 = rig.sup.now();
    tick_at(&rig, &live, t0).await;
    tick_at(&rig, &live, t0 + ACK_WINDOW).await;
    rig.until_input(id, InputState::Unconfirmed).await;
    assert_eq!(last_ack(&rig, &live), None);
    assert!(rig.of_kind(live.t, "user_message").is_empty());
    rig.stop().await;
}

/// A sent input made `unconfirmed` by its ack window (the clock moved by injected ticks).
async fn unconfirmed(rig: &SRig, live: &Live, text: &str) -> InputId {
    let id = rig.send(live, text).await;
    rig.until_input(id, InputState::Sent).await;
    let t0 = rig.sup.now();
    tick_at(rig, live, t0).await;
    tick_at(rig, live, t0 + ACK_WINDOW).await;
    rig.until_input(id, InputState::Unconfirmed).await;
    rig.clock.advance(ACK_WINDOW);
    id
}

/// PR10b-CL-9: an earlier input of the same text that left flight without a hook ack (acked
/// by the owner, or dismissed) makes the hook unattributable; a different text is acked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn earlier_match() {
    for delivered in [true, false] {
        let rig = SRig::new().await;
        let live = rig.bound(Agent::Codex).await;
        rig.clock.stop();
        let first = unconfirmed(&rig, &live, "same").await;
        rig.cmd_ok(
            "input_resolve",
            &json!({ "input_id": first, "delivered": delivered }),
            200,
        )
        .await;
        rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
            .await;
        let again = rig.send(&live, "same").await;
        rig.until_input(again, InputState::Sent).await;
        prompt(&rig, &live, "same").await;
        assert_eq!(
            rig.input(again).state,
            InputState::Unconfirmed,
            "delivered: {delivered}"
        );
        // A different text is acked.
        rig.cmd_ok(
            "input_resolve",
            &json!({ "input_id": again, "delivered": false }),
            200,
        )
        .await;
        rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
            .await;
        let other = rig.send(&live, "other").await;
        rig.until_input(other, InputState::Sent).await;
        prompt(&rig, &live, "other").await;
        assert_eq!(rig.input(other).state, InputState::Acked);
        rig.stop().await;
    }
}

/// PR10b-CL-10: a raw key after the dispatch, before the hook's intake: not acked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn key_after_send() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let id = rig.send(&live, "then key").await;
    rig.until_input(id, InputState::Sent).await;
    rig.cmd_ok(
        "key",
        &json!({ "thread_id": live.t, "launch_id": live.l, "keys": ["escape"] }),
        200,
    )
    .await;
    prompt(&rig, &live, "then key").await;
    assert_eq!(rig.input(id).state, InputState::Unconfirmed);
    rig.stop().await;
}

/// PR10b-CL-10: an attach unit submitted while the input's unit is in its pause is written
/// after it: the hook does not ack the input.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn human_during_unit() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let probe = rig.probe(&live).await;
    let park = probe.park(ProbePoint::PauseEntered);
    let id = rig.send(&live, "with attach").await;
    let p2 = probe.clone();
    assert!(
        tokio::task::spawn_blocking(move || p2.wait(
            ProbePoint::PauseEntered,
            1,
            Instant::now() + Duration::from_secs(10)
        ))
        .await
        .unwrap()
    );
    let report = rig.attach(&live, b"x").await;
    drop(park);
    rig.until_input(id, InputState::Sent).await;
    let _ = report.await;
    prompt(&rig, &live, "with attach").await;
    assert_eq!(rig.input(id).state, InputState::Unconfirmed);
    rig.stop().await;
}

/// PR10b-CL-10: a gate-closing human write after the last turn start makes the next attempt
/// competing (attach, `ready`, send, delayed hook → `unconfirmed`); without the attach the
/// hook acks; while the attach's `human_write` job is held the send stays `queued`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn competing_human() {
    for attach in [true, false] {
        let rig = SRig::new().await;
        let live = rig.bound(Agent::Codex).await;
        if attach {
            let (entered, go, point) = gate_point();
            set(&rig.sl(&live).hooks.record_job, point);
            let report = rig.attach(&live, b"typed\r").await;
            entered.await.unwrap();
            // The worker is inside the attach's record: a send made now stays queued.
            let held = rig.send(&live, "P").await;
            assert_eq!(rig.sl(&live).state().human_unsettled, 1);
            assert_eq!(
                rig.input(held).state,
                InputState::Queued,
                "the human unit is unsettled"
            );
            go.send(()).unwrap();
            let _ = report.await;
            assert_eq!(rig.sl(&live).state().human_unsettled, 0);
            rig.pass(&live).await;
            assert_eq!(
                rig.input(held).state,
                InputState::Queued,
                "the attach closed the gate"
            );
            rig.cmd_ok("input_cancel", &json!({ "input_id": held }), 200)
                .await;
        }
        rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
            .await;
        let id = rig.send(&live, "P").await;
        rig.until_input(id, InputState::Sent).await;
        prompt(&rig, &live, "P").await;
        let want = if attach {
            InputState::Unconfirmed
        } else {
            InputState::Acked
        };
        assert_eq!(rig.input(id).state, want, "attach: {attach}");
        rig.stop().await;
    }
}

/// PR10b-CL-11: a positional prompt's hook is attributable while no `Human` or `Input` write
/// is above the publication's reference: a VT reply after publication does not count, an
/// attach write does, and an unobservable launch is not attributable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn positional_anchor() {
    for case in ["reply", "attach", "unobservable"] {
        let rig = SRig::new().await;
        let (live, v) = rig.spawn(Agent::Codex, Some("first")).await;
        let id: InputId = serde_json::from_value(v["input_id"].clone()).unwrap();
        assert_eq!(rig.input(id).state, InputState::Sent);
        rig.hook(&live, "SessionStart", &json!({})).await;
        match case {
            "reply" => {
                live.child.emit(b"\x1b[6n");
                rig.until("the reply was written", || !live.child.input().is_empty())
                    .await;
                prompt(&rig, &live, "first").await;
            }
            "attach" => {
                let r = rig.attach(&live, b"x").await;
                let _ = r.await;
                prompt(&rig, &live, "first").await;
            }
            _ => {
                // The live map held across the intake's observation only, by the request
                // task itself: the runtime tasks that look a launch up never wait on it.
                let sl = rig.sl(&live);
                *crate::supervisor::send::lock(&sl.hooks.observe) = Some(Box::new(|sup, l| {
                    let _map = sup.live.lock();
                    sup.observe(l)
                }));
                prompt(&rig, &live, "first").await;
                assert!(
                    crate::supervisor::send::lock(&sl.hooks.observe).is_none(),
                    "observed with the map held"
                );
            }
        }
        let want = if case == "reply" {
            InputState::Acked
        } else {
            InputState::Unconfirmed
        };
        assert_eq!(rig.input(id).state, want, "{case}");
        rig.stop().await;
    }
}

/// PR10b-CL-9: the matcher over constructed rows: no match, one in-flight match (acked when
/// written alone), two in-flight matches (none); and two long texts with the same preview and
/// different tails, compared in full.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn matcher() {
    use chaldea_core::event::TypedEvent;
    use chaldea_core::ids::{Actor, LaunchId, ThreadId};
    use chaldea_core::input::{InputInput, InputReason};
    use chaldea_core::payload::{
        Content, InputCreated, InputEncoding, InputInitialState, InputOrigin, InputStateChanged,
    };

    use crate::supervisor::send::attribution::attribute;
    use crate::supervisor::send::probe::{Inflight, IntakeFactsV1};
    use crate::supervisor::tests::support::event;
    use crate::supervisor::tests::support::hook::{register, reserve, start};

    let rig = SRig::new().await;
    let (t, l) = (ThreadId::from_u128(0x51), LaunchId::from_u128(0x52));
    rig.srv.append(vec![register(t, Agent::Claude)]).await;
    rig.srv
        .append(vec![reserve(t, l, Agent::Claude), start(t, l)])
        .await;
    let sent = |id: InputId, text: &str| {
        let s = chaldea_core::ids::StreamId::Thread(t);
        vec![
            event(
                s,
                &format!("c:{id}"),
                TypedEvent::InputCreated(InputCreated {
                    input_id: id,
                    launch_id: Some(l),
                    initial_state: InputInitialState::Queued,
                    text: Content::text(text),
                    encoding: InputEncoding::BracketedPaste,
                    origin: InputOrigin::Cli,
                    created_by: Actor::Human,
                }),
            ),
            event(
                s,
                &format!("s:{id}"),
                TypedEvent::InputStateChanged(InputStateChanged {
                    input_id: id,
                    from: InputState::Queued,
                    to: InputState::Sending,
                    reason: InputReason::from(&InputInput::GateOpened { sent_after_seq: 1 }),
                    sent_after_seq: Some(1),
                    by: None,
                }),
            ),
        ]
    };
    let (a, b) = (InputId::from_u128(0xA), InputId::from_u128(0xB));
    rig.srv.append(sent(a, "alpha")).await;
    let facts = |id: InputId| IntakeFactsV1 {
        v: 1,
        reference: 0,
        last_started: [5, 0, 0],
        running: false,
        human_running: false,
        inflight: Some(Inflight {
            input_id: id,
            position: Some(5),
            written: true,
            competing: false,
        }),
    };
    let run = |prompt: &'static str, f: IntakeFactsV1| {
        rig.srv.store.read(move |r| {
            attribute(r, t, l, Agent::Claude, prompt, Some(&f), true)
                .map(|x| (x.matched.map(|m| m.input_id), x.attributable))
        })
    };
    assert_eq!(run("nothing", facts(a)), (None, false));
    assert_eq!(run("alpha", facts(a)), (Some(a), true));
    let mut competing = facts(a);
    competing.inflight.as_mut().unwrap().competing = true;
    assert_eq!(run("alpha", competing), (Some(a), false));
    let mut late_human = facts(a);
    late_human.last_started[1] = 6;
    assert_eq!(run("alpha", late_human), (Some(a), false));
    rig.srv.append(sent(b, "alpha")).await;
    assert_eq!(run("alpha", facts(a)), (None, false), "two in flight");
    rig.stop().await;

    // Two long texts with one preview: the full text decides.
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let head = "前".repeat(30 * 1024);
    let (x, y) = (format!("{head}x"), format!("{head}y"));
    let id = rig.send(&live, &x).await;
    rig.until_input(id, InputState::Sent).await;
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": y }))
        .await;
    assert_eq!(last_ack(&rig, &live), None);
    assert_eq!(rig.input(id).state, InputState::Sent);
    rig.hook(&live, "UserPromptSubmit", &json!({ "prompt": x }))
        .await;
    assert_eq!(rig.input(id).state, InputState::Acked);
    rig.stop().await;
}
