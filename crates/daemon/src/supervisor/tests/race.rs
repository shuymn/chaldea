//! A resume and a close expecting the same launch (PR10a1-CL-4).

use chaldea_core::ids::{LaunchId, ThreadId};
use serde_json::json;

use super::support::http::spawn_send;
use super::{Rig, cid, json};

fn code(r: &super::support::http::Reply) -> String {
    json(r)["code"].as_str().unwrap_or_default().to_owned()
}

/// PR10a1-CL-4: of a resume and a close expecting the same launch, one succeeds and the other
/// answers `thread_closed`, `launch_changed`, or `launch_live`: in both orders, released
/// together through a parked writer, and with the resumed launch exited and confirmed before
/// the close.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_close() {
    let rig = Rig::new().await;
    let resume = |t: ThreadId, l: LaunchId| json!({ "thread_id": t, "expected_launch_id": l });
    // Resume first.
    let (t, l) = rig.stopped_bound().await;
    assert_eq!(rig.cmd("resume", &resume(t, l)).await.status, 200);
    let r = rig.cmd("close", &resume(t, l)).await;
    assert_eq!((r.status, code(&r).as_str()), (409, "launch_changed"));
    assert!(!rig.thread(t).closed);
    // Close first.
    let (t, l) = rig.stopped_bound().await;
    assert_eq!(rig.cmd("close", &resume(t, l)).await.status, 200);
    let r = rig.cmd("resume", &resume(t, l)).await;
    assert_eq!((r.status, code(&r).as_str()), (409, "thread_closed"));
    assert_eq!(rig.thread(t).current_launch_id, Some(l));
    // Together, several times.
    for _ in 0..5 {
        let (t, l) = rig.stopped_bound().await;
        let release = rig.srv.hold_writer().await;
        let a = spawn_send(
            &rig.srv,
            &rig.srv
                .command("resume", &rig.owner, cid(), &resume(t, l).to_string()),
        );
        let b = spawn_send(
            &rig.srv,
            &rig.srv
                .command("close", &rig.owner, cid(), &resume(t, l).to_string()),
        );
        drop(release);
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        let ok = [&a, &b].iter().filter(|r| r.status == 200).count();
        assert_eq!(
            ok,
            1,
            "{} {}",
            String::from_utf8_lossy(&a.body),
            String::from_utf8_lossy(&b.body)
        );
        let loser = if a.status == 200 { &b } else { &a };
        assert!(
            ["thread_closed", "launch_changed", "launch_live"].contains(&code(loser).as_str()),
            "{}",
            String::from_utf8_lossy(&loser.body)
        );
        let row = rig.thread(t);
        assert_eq!(row.closed, b.status == 200);
        assert_eq!(row.current_launch_id == Some(l), b.status == 200);
    }
    // The resumed launch exits and is confirmed before the close runs.
    let (t, l) = rig.stopped_bound().await;
    assert_eq!(rig.cmd("resume", &resume(t, l)).await.status, 200);
    assert_eq!(
        rig.cmd("stop", &json!({ "thread_id": t })).await.status,
        200
    );
    let r = rig.cmd("close", &resume(t, l)).await;
    assert_eq!((r.status, code(&r).as_str()), (409, "launch_changed"));
    assert!(!rig.thread(t).closed);
    rig.srv.stop().await;
}
