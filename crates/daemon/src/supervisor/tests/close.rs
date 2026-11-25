//! Close (design §2.3 "Close"; PR10a1-CL-25, CL-39).

use chaldea_core::ids::LaunchId;
use chaldea_pty_runtime::ChildExit;
use chaldea_pty_runtime::fake::FakeEvent;
use serde_json::json;

use super::support::http::spawn_send;
use super::{Rig, cid, json};
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
/// the stop's seal: `stop` is `already_exited`, the thread closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_result() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let start = rig.pauses.arm(Pause::StopStart);
    let reply = spawn_send(
        &rig.srv,
        &rig.srv.command(
            "close",
            &rig.owner,
            cid(),
            &json!({ "thread_id": t, "expected_launch_id": l, "stop": true }).to_string(),
        ),
    );
    start.wait();
    child.exit(ChildExit {
        code: Some(0),
        signal: None,
    });
    rig.until("the exit is recorded", || {
        rig.launch(l).exit_reason.is_some()
    })
    .await;
    start.go();
    let r = reply.await.unwrap();
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = json(&r);
    assert_eq!(v["stop"]["outcome"], "already_exited", "{v}");
    assert_eq!(v["stop"]["death_confirmed"], true);
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
