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

fn proving_report(method: &str) -> chaldea_pty_runtime::KillReport {
    chaldea_pty_runtime::KillReport {
        method: method.to_owned(),
        tree_dead: true,
        survivors: Vec::new(),
        attempt: chaldea_pty_runtime::KillAttempt::Signalled,
        proved_at: Some(std::time::Instant::now()),
    }
}

fn close_outcome(
    kill: chaldea_pty_runtime::KillReport,
    exit: Option<chaldea_pty_runtime::ChildExit>,
) -> chaldea_pty_runtime::CloseOutcome {
    chaldea_pty_runtime::CloseOutcome {
        kill,
        close: chaldea_pty_runtime::CloseReport {
            elapsed: Some(std::time::Duration::ZERO),
        },
        output: None,
        released: chaldea_pty_runtime::Released {
            input: true,
            control: true,
            output: true,
            tree_dead: true,
        },
        abandoned: Vec::new(),
        final_snapshot: None,
        final_after_end: false,
        elapsed: std::time::Duration::ZERO,
        exit,
    }
}

/// Polls `fut` once: true when it is still pending.
async fn pending_after_one_poll(fut: &mut std::pin::Pin<&mut impl std::future::Future>) -> bool {
    tokio::select! {
        biased;
        _ = fut => false,
        () = std::future::ready(()) => true,
    }
}

/// PR10a1-CL-11 `proof_before_slow_join`, at the op's waits (`await_kill`): a proving report
/// delivered at once, the join held until the clock passed the kill's submission + 12 s: the
/// report is accepted, the late join's exit and its own report are not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_before_a_late_join() {
    use crate::supervisor::ops::{KillTask, await_kill};
    let rig = Rig::new().await;
    let sup = rig.sup.clone();
    let (tx, first) = tokio::sync::oneshot::channel();
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let late = close_outcome(
        proving_report("from_the_join"),
        Some(chaldea_pty_runtime::ChildExit {
            code: Some(1),
            signal: None,
        }),
    );
    let join = tokio::spawn(async move {
        let _ = held.await;
        late
    });
    let report = proving_report("fake");
    tx.send(report.clone()).unwrap();
    let fut = await_kill(&sup, KillTask::of(sup.now(), join, first));
    tokio::pin!(fut);
    assert!(pending_after_one_poll(&mut fut).await, "the join is held");
    rig.clock
        .advance(crate::supervisor::PROOF_CUTOFF + std::time::Duration::from_secs(1));
    release.send(()).unwrap();
    let proof = fut.await;
    assert_eq!(proof.report, Some(report));
    assert_eq!(proof.exit, None, "the join is late");
    rig.srv.stop().await;
}

/// PR10a1-CL-11 `proof_cutoff` (join first), at the op's waits: the kill task panicked after
/// sending nothing yet, so its join is ready and failed while the report is withheld: the
/// waits do not end with the join; the report delivered later is accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_join_waits_for_the_report() {
    use crate::supervisor::ops::{KillTask, await_kill};
    let rig = Rig::new().await;
    let sup = rig.sup.clone();
    let (tx, first) = tokio::sync::oneshot::channel();
    let join: tokio::task::JoinHandle<chaldea_pty_runtime::CloseOutcome> =
        tokio::spawn(async { panic!("a kill task that panicked") });
    rig.until("the join is ready", || join.is_finished()).await;
    let fut = await_kill(&sup, KillTask::of(sup.now(), join, first));
    tokio::pin!(fut);
    assert!(
        pending_after_one_poll(&mut fut).await,
        "the waits end with both, not with the failed join"
    );
    let report = proving_report("fake");
    tx.send(report.clone()).unwrap();
    let proof = fut.await;
    assert_eq!(proof.report, Some(report));
    assert_eq!(proof.exit, None);
    rig.srv.stop().await;
}
