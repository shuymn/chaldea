//! Stop (design §2.3 "Stop"; PR10a1-CL-10, CL-11, CL-12).

use std::time::Duration;

use chaldea_pty_runtime::ProcessIdentity;
use chaldea_pty_runtime::fake::FakeEvent;
use serde_json::json;

use super::support::http::spawn_send;
use super::{Rig, json};
use crate::supervisor::Pause;

fn kills(events: &[FakeEvent]) -> Vec<Vec<ProcessIdentity>> {
    events
        .iter()
        .filter_map(|e| match e {
            FakeEvent::Kill { extra } => Some(extra.clone()),
            _ => None,
        })
        .collect()
}

/// Every kill and close gets the launch's full recorded list (PR10a1-CL-10).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_list_every_call() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let id = ProcessIdentity {
        pid: 4242,
        start_time: "s".to_owned(),
        group: "g".to_owned(),
    };
    rig.sup.entry(l).unwrap().record_extra(id.clone());
    let child = rig.pty.take_child().unwrap();
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    assert_eq!(r.status, 200);
    let ks = kills(&child.log());
    assert!(!ks.is_empty());
    assert!(ks.iter().all(|k| k == std::slice::from_ref(&id)), "{ks:?}");
    rig.srv.stop().await;
}

/// The op lock held past the op's start + 10 s: 504 `launch_busy`, nothing sealed, no
/// kill, no `StopRequested` (PR10a1-CL-11 `busy_before_seal`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_before_seal() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let entry = rig.sup.entry(l).unwrap();
    let held = entry.ops.clone().lock_owned().await;
    let start = rig.pauses.arm(Pause::StopStart);
    let reply = spawn_send(
        &rig.srv,
        &rig.srv.command(
            "stop",
            &rig.owner,
            super::cid(),
            &json!({ "thread_id": t }).to_string(),
        ),
    );
    start.wait();
    rig.clock.skew(Duration::from_secs(11));
    start.go();
    let r = reply.await.unwrap();
    assert_eq!(r.status, 504, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(json(&r)["code"], "launch_busy");
    drop(held);
    assert!(kills(&child.log()).is_empty());
    assert!(!entry.stop_requested());
    assert_eq!(
        rig.thread(t).state.process,
        chaldea_core::state::ProcessState::Running
    );
    rig.srv.stop().await;
}
