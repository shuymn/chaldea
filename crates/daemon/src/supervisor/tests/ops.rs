//! Launch operations as supervisor tasks (design §2.1 rule 2, SD17; PR10a1-CL-32).

use chaldea_core::ids::{Actor, Agent, CommandId, LaunchId, ThreadId};
use serde_json::json;

use super::{Rig, json};
use crate::http::{CommandEffect, EffectRun, mint_token};
use crate::supervisor::Pause;
use crate::supervisor::commands::SpawnHandler;
use crate::supervisor::ops::{LaunchKind, LaunchReq};

/// The runner's effect future dropped after the reservation commit: the op runs on, records
/// `launch_started`, and a later stop finds its session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outlive_runner() {
    let rig = Rig::new().await;
    let (t, l) = (
        ThreadId(uuid::Uuid::new_v4()),
        LaunchId(uuid::Uuid::new_v4()),
    );
    let req = LaunchReq {
        command_id: CommandId(uuid::Uuid::new_v4()),
        thread: t,
        launch: l,
        kind: LaunchKind::Spawn {
            agent: Agent::Codex,
            cwd: rig.cwd.path().to_string_lossy().into_owned(),
            task: None,
            parent: None,
        },
        prompt: None,
        input_id: None,
        hook_token: mint_token(),
        caller: Actor::Human,
    };
    let after = rig.pauses.arm(Pause::AfterReserve);
    let runner = tokio::spawn(SpawnHandler(rig.sup.clone()).run(EffectRun::new(req)));
    after.wait();
    assert!(rig.kinds(t).iter().any(|k| k == "launch_reserved"));
    runner.abort();
    assert!(runner.await.unwrap_err().is_cancelled());
    after.go();
    rig.until("the op records launch_started", || {
        rig.kinds(t).iter().any(|k| k == "launch_started")
    })
    .await;
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = json(&r);
    assert_eq!(
        (v["outcome"].clone(), v["death_confirmed"].clone()),
        (json!("stopped"), json!(true))
    );
    assert_eq!(rig.pty.spawns().len(), 1);
    rig.srv.stop().await;
}
