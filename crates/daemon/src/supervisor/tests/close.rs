//! Close (design §2.3 "Close"; PR10a1-CL-25, CL-39).

use chaldea_core::ids::LaunchId;
use chaldea_pty_runtime::ChildExit;
use chaldea_pty_runtime::fake::FakeEvent;
use serde_json::json;
use std::time::Duration;

use super::support::http::spawn_send;
use super::{Rig, cid, json};
use crate::store::tests::support::waits;
use crate::supervisor::Pause;

fn code(r: &super::support::http::Reply) -> String {
    json(r)["code"].as_str().unwrap_or_default().to_owned()
}

/// PR10a1-CL-25: a stale expected launch is `launch_changed` before any op, with and without
/// `--stop`; a `daemon_restart` launch with `--stop` is `death_unconfirmed` with no kill.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cases() {
    let rig = Rig::new().await;
    let (t, _l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let stale = LaunchId(uuid::Uuid::new_v4());
    for stop in [false, true] {
        let r = rig
            .cmd(
                "close",
                &json!({ "thread_id": t, "expected_launch_id": stale, "stop": stop }),
            )
            .await;
        assert_eq!((r.status, code(&r).as_str()), (409, "launch_changed"));
    }
    assert!(
        !child
            .log()
            .iter()
            .any(|e| matches!(e, FakeEvent::Kill { .. }))
    );
    // A launch the restart recorded `daemon_restart` (its session still held here).
    let (u, ul) = rig.spawn_codex(None).await;
    let uchild = rig.pty.take_child().unwrap();
    let r = crate::supervisor::restart::launches(Vec::new());
    tokio::task::block_in_place(|| rig.srv.store.run(r.job).unwrap());
    assert_eq!(
        rig.launch(ul).exit_reason,
        Some(chaldea_core::payload::ExitReason::DaemonRestart)
    );
    let r = rig
        .cmd(
            "close",
            &json!({ "thread_id": u, "expected_launch_id": ul, "stop": true }),
        )
        .await;
    assert_eq!((r.status, code(&r).as_str()), (409, "death_unconfirmed"));
    assert!(
        !uchild
            .log()
            .iter()
            .any(|e| matches!(e, FakeEvent::Kill { .. }))
    );
    assert!(!rig.thread(u).closed);
    rig.srv.stop().await;
}

/// PR10a1-CL-39 `stop_result`: a launch that exits by itself between the close's intent and
/// the stop's seal: `stop` is `already_exited`, the thread closed; its
/// `confirmed_within_deadline` is true when the stop's re-read returned by `RequestStart` +
/// 5 s, and false when the clock passed that before the re-read: late, 2 s pass before the
/// stop starts and 4 s after, so the re-read is past `RequestStart` + 5 s but not past the
/// stop's start + 5 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_result() {
    for late in [false, true] {
        stop_result_with(late).await;
    }
}

async fn stop_result_with(late: bool) {
    // The clock stays stopped: the holds below spend no budget, and only `advance` makes
    // the re-read late.
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let start = rig.pauses.arm(Pause::StopStart);
    let runner = crate::http::tests::shared(&rig.srv)
        .faults
        .arm_runner_gate();
    let reply = spawn_send(
        &rig.srv,
        &rig.srv.command(
            "close",
            &rig.owner,
            cid(),
            &json!({ "thread_id": t, "expected_launch_id": l, "stop": true }).to_string(),
        ),
    );
    let (submitting, submit) = runner;
    waits::bounded(Duration::from_secs(10), submitting)
        .await
        .expect("the close's runner starts")
        .unwrap();
    if late {
        rig.clock.advance(Duration::from_secs(2));
    }
    submit.send(()).unwrap();
    start.wait();
    child.exit(ChildExit {
        code: Some(0),
        signal: None,
    });
    rig.until("the exit is recorded", || {
        rig.launch(l).exit_reason.is_some()
    })
    .await;
    if late {
        rig.clock.advance(Duration::from_secs(4));
    }
    start.go();
    let r = reply.await.unwrap();
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = json(&r);
    assert_eq!(v["stop"]["outcome"], "already_exited", "{v}");
    assert_eq!(v["stop"]["death_confirmed"], true);
    assert_eq!(v["stop"]["confirmed_within_deadline"], !late, "{v}");
    assert!(rig.thread(t).closed);
    // One exit, the natural one: the stop recorded none.
    let exits: Vec<_> = rig
        .events(t)
        .into_iter()
        .filter(|(k, _)| k == "launch_exited")
        .collect();
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0].1["reason"], "exit");
    rig.srv.stop().await;
}

/// PR10a1-CL-20 `nudges`: one socket nudge per committed revoking job (a stop's exit job, a
/// close's intent; a `close --stop`'s intent, exit job, and closing job), none for a refused
/// close.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nudges() {
    let rig = Rig::new().await;
    let count = || *rig.srv.server.shared().revocations.borrow();
    let (t, l) = rig.spawn_codex(None).await;
    let n = std::cell::Cell::new(count());
    let step = |name: &str, r: super::support::http::Reply, want: u64| {
        assert_eq!(
            r.status,
            200,
            "{name}: {}",
            String::from_utf8_lossy(&r.body)
        );
        let now = count();
        assert_eq!(now - n.get(), want, "{name}");
        n.set(now);
    };
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    step("stop", r, 1);
    let r = rig
        .cmd("close", &json!({ "thread_id": t, "expected_launch_id": l }))
        .await;
    step("close", r, 1);
    let (u, ul) = rig.spawn_codex(None).await;
    n.set(count());
    let r = rig
        .cmd(
            "close",
            &json!({ "thread_id": u, "expected_launch_id": ul, "stop": true }),
        )
        .await;
    step("close --stop", r, 3);
    // Refused: nothing committed, no nudge.
    let r = rig
        .cmd(
            "close",
            &json!({ "thread_id": u, "expected_launch_id": LaunchId(uuid::Uuid::new_v4()) }),
        )
        .await;
    assert_eq!(r.status, 409);
    assert_eq!(count(), n.get(), "a refused close");
    rig.srv.stop().await;
}

/// PR10a1-CL-25 `stop_then_newer_launch`: a `close --stop` held after its stop, before its
/// closing job, while a resume starts a newer launch: the closing job answers
/// `launch_changed`, the thread stays open, and the newer launch keeps running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_then_newer_launch() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    rig.bind(t, l, "the-session").await;
    let held = rig.pauses.arm(Pause::BeforeClosingJob);
    let reply = spawn_send(
        &rig.srv,
        &rig.srv.command(
            "close",
            &rig.owner,
            cid(),
            &json!({ "thread_id": t, "expected_launch_id": l, "stop": true }).to_string(),
        ),
    );
    let held = tokio::task::spawn_blocking(move || {
        held.wait();
        held
    })
    .await
    .unwrap();
    assert!(rig.launch(l).death_confirmed, "the stop half ran");
    let r = rig
        .cmd(
            "resume",
            &json!({ "thread_id": t, "expected_launch_id": l }),
        )
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let l2: LaunchId = serde_json::from_value(json(&r)["launch_id"].clone()).unwrap();
    held.go();
    let r = reply.await.unwrap();
    assert_eq!(
        (r.status, code(&r).as_str()),
        (409, "launch_changed"),
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert!(!rig.thread(t).closed);
    assert_eq!(rig.thread(t).current_launch_id, Some(l2));
    assert_eq!(rig.launch(l2).exit_reason, None, "the newer launch runs");
    rig.srv.stop().await;
}
