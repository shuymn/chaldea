//! The tick (design §2.6; PR10a1-CL-17).

use std::time::Duration;

use chaldea_core::ids::{LaunchId, ThreadId};
use serde_json::json;

use super::{Rig, json};

fn raised(rig: &Rig, t: ThreadId) -> bool {
    rig.events(t)
        .iter()
        .any(|(_, p)| p.to_string().contains("session_start_timeout"))
}

async fn claude(rig: &Rig) -> (ThreadId, LaunchId) {
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let r = rig
        .cmd("spawn", &json!({ "agent": "claude", "cwd": cwd }))
        .await;
    let v = json(&r);
    (
        serde_json::from_value(v["thread_id"].clone()).unwrap(),
        serde_json::from_value(v["launch_id"].clone()).unwrap(),
    )
}

/// `session_start_timeout`: none at +19.999 s, raised at +20 s from the committed start;
/// none for a bound, an exited, or a manual launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_start_timeout() {
    let rig = Rig::new().await;
    let at = |l: LaunchId| rig.sup.entry(l).unwrap().committed().unwrap().at;
    let (t, l) = claude(&rig).await;
    let start = at(l);
    rig.sup
        .tick(start + Duration::from_secs(20) - Duration::from_millis(1))
        .await;
    assert!(!raised(&rig, t), "none before 20 s");
    rig.sup.tick(start + Duration::from_secs(20)).await;
    assert!(raised(&rig, t), "raised at 20 s");
    // Raised once.
    let n = rig.kinds(t).len();
    rig.sup.tick(start + Duration::from_secs(40)).await;
    assert_eq!(rig.kinds(t).len(), n);
    // Bound: none.
    let (b, bl) = claude(&rig).await;
    rig.bind(b, bl, "bound").await;
    rig.sup.tick(at(bl) + Duration::from_secs(21)).await;
    assert!(!raised(&rig, b));
    // Manual Codex: none.
    let (m, ml) = rig.spawn_codex(None).await;
    rig.sup.tick(at(ml) + Duration::from_secs(21)).await;
    assert!(!raised(&rig, m));
    // Exited between the tick's candidates and its job: the job re-reads and writes none.
    let (e, el) = claude(&rig).await;
    let late = at(el) + Duration::from_secs(21);
    let entry = rig.sup.entry(el).unwrap();
    rig.cmd("stop", &json!({ "thread_id": e })).await;
    assert!(rig.sup.entry(el).is_none());
    drop(entry);
    rig.sup.tick(late).await;
    assert!(!raised(&rig, e));
    rig.srv.stop().await;
}
