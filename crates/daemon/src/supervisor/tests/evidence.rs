//! `spawn_failed` and `close` evidence (`docs/design/supervisor-recovery.md` §2.4;
//! PR10a2-CL-7 `recorded`).

use std::time::Duration;

use chaldea_core::event::EventEnvelope;
use chaldea_core::ids::{LaunchId, StreamId, ThreadId};
use chaldea_pty_runtime::fake::{FakeScript, KillBehavior};
use chaldea_pty_runtime::{Cleanup, KillAttempt, KillReport, ProcessIdentity, SpawnError};
use serde_json::{Value, json};

use super::{Rig, json as body};
use crate::lifecycle::ParticipantEnd;
use crate::supervisor::Pause;
use crate::supervisor::components::shutdown;
use crate::supervisor::ops::{CloseReceipt, close_evidence};

/// The thread stream's envelopes.
fn envelopes(rig: &Rig, t: ThreadId) -> Vec<EventEnvelope> {
    rig.srv
        .store
        .read(move |r| Ok(r.events_after(StreamId::Thread(t), 0, 10_000)?.events))
}

/// The one `launch_evidence` of `l`, checked against §2.4's envelope and order rules: right
/// after `launch_exited` (seq + 1), keyed `launch:<L>:evidence:<source>`, `command_id` null,
/// `source.kind` the exit job's, `unchecked` never empty. `None` when there is none.
fn checked(rig: &Rig, t: ThreadId, l: LaunchId, kind: &str) -> Option<Value> {
    let env = envelopes(rig, t);
    let ev: Vec<&EventEnvelope> = env
        .iter()
        .filter(|e| e.kind.as_str() == "launch_evidence" && e.launch_id == Some(l))
        .collect();
    assert!(ev.len() <= 1, "{ev:?}");
    let e = ev.first()?;
    let exited = env
        .iter()
        .find(|e| e.kind.as_str() == "launch_exited" && e.launch_id == Some(l))
        .unwrap();
    assert_eq!(e.seq, exited.seq + 1, "right after launch_exited");
    let p: Value = serde_json::from_str(e.payload.get()).unwrap();
    assert_eq!(
        e.idempotency_key.0,
        format!("launch:{l}:evidence:{}", p["source"].as_str().unwrap())
    );
    assert_eq!(e.command_id, None);
    assert_eq!(p["launch_id"], json!(l));
    assert_eq!(serde_json::to_value(&e.source.kind).unwrap(), json!(kind));
    assert!(!p["unchecked"].as_array().unwrap().is_empty());
    assert_eq!(p["probe_run"], Value::Null);
    Some(p)
}

fn id(start: &str) -> ProcessIdentity {
    ProcessIdentity {
        pid: 77,
        start_time: start.to_owned(),
        group: "g".to_owned(),
    }
}

fn report(dead: bool, survivors: Vec<ProcessIdentity>) -> KillReport {
    KillReport {
        method: "fake".to_owned(),
        tree_dead: dead,
        survivors,
        attempt: KillAttempt::Signalled,
        proved_at: dead.then(std::time::Instant::now),
    }
}

/// Spawns with `fail` scripted and returns the thread and launch.
async fn failed_spawn(rig: &Rig, script: FakeScript) -> (ThreadId, LaunchId) {
    rig.pty.script_next(script);
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let r = rig
        .cmd("spawn", &json!({ "agent": "codex", "cwd": cwd }))
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = body(&r);
    assert_eq!(v["outcome"], "spawn_failed");
    (
        serde_json::from_value(v["thread_id"].clone()).unwrap(),
        serde_json::from_value(v["launch_id"].clone()).unwrap(),
    )
}

fn started_then_failed(identity: Option<ProcessIdentity>, cleanup: Cleanup) -> FakeScript {
    FakeScript {
        fail: Some(SpawnError::StartedThenFailed {
            stage: "s",
            error: std::io::Error::other("x"),
            identity,
            cleanup,
        }),
        ..FakeScript::default()
    }
}

fn unresolved(kill: Option<KillReport>) -> Cleanup {
    Cleanup::Unresolved {
        kill,
        close: None,
        drain_abandoned: false,
    }
}

/// PR10a2-CL-7 `recorded`, the failed starts: each case's identities and unchecked scope,
/// and a confirmed cleanup records none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recorded_failed_starts() {
    let rig = Rig::new().await;
    let other = ProcessIdentity {
        pid: 78,
        start_time: "s78".to_owned(),
        group: "g".to_owned(),
    };
    let cases: Vec<(FakeScript, Value, Value)> = vec![
        (
            started_then_failed(None, unresolved(None)),
            json!([]),
            json!(["no_identity", "tree_unproven"]),
        ),
        // No leader identity, and a kill report naming a survivor: the survivor is kept.
        (
            started_then_failed(None, unresolved(Some(report(false, vec![other.clone()])))),
            json!([{ "pid": 78, "start_time": "s78", "observed": "unresolved" }]),
            json!(["no_identity", "tree_unproven"]),
        ),
        (
            started_then_failed(Some(id("st")), unresolved(Some(report(true, vec![])))),
            json!([{ "pid": 77, "start_time": "st", "observed": "gone" }]),
            json!(["cleanup_unreleased"]),
        ),
        (
            started_then_failed(
                Some(id("st")),
                unresolved(Some(report(false, vec![id("st"), other.clone()]))),
            ),
            json!([
                { "pid": 77, "start_time": "st", "observed": "unresolved" },
                { "pid": 78, "start_time": "s78", "observed": "unresolved" },
            ]),
            json!(["tree_unproven"]),
        ),
        (
            started_then_failed(Some(id("")), unresolved(Some(report(false, vec![id("")])))),
            json!([{ "pid": 77, "start_time": null, "observed": "unresolved" }]),
            json!(["tree_unproven"]),
        ),
        (
            FakeScript {
                panic_spawn: true,
                ..FakeScript::default()
            },
            json!([]),
            json!(["no_identity", "tree_unproven", "result_not_received"]),
        ),
    ];
    for (script, identities, unchecked) in cases {
        let (t, l) = failed_spawn(&rig, script).await;
        let p = checked(&rig, t, l, "process").expect("spawn_failed evidence");
        assert_eq!(p["source"], "spawn_failed");
        assert_eq!(p["identities"], identities, "{p}");
        assert_eq!(p["unchecked"], unchecked, "{p}");
    }
    // A confirmed cleanup: no evidence.
    let (t, l) = failed_spawn(
        &rig,
        started_then_failed(Some(id("st")), Cleanup::Confirmed),
    )
    .await;
    assert!(checked(&rig, t, l, "process").is_none());
    rig.srv.stop().await;
}

/// PR10a2-CL-7 `recorded`, the owned closes: a stop whose close reports `tree_dead: false`
/// with survivors (and the builder with none): `tree_unproven`; a confirmed stop: none; a
/// natural exit and the participant's close left unconfirmed: their survivors.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recorded_closes() {
    let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, |c| {
        c.kill_budget = Duration::from_millis(300);
    })
    .await;
    rig.clock.resume();
    // A stop the close did not prove.
    rig.pty.script_next(FakeScript {
        kill: KillBehavior::Unconfirmed,
        ..FakeScript::default()
    });
    let (t, l) = rig.spawn_codex(None).await;
    let leader = super::prober_leader(&rig, l);
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    assert_eq!(body(&r)["death_confirmed"], false);
    let p = checked(&rig, t, l, "process").expect("close evidence");
    assert_eq!(p["source"], "close");
    assert_eq!(
        p["identities"],
        json!([{ "pid": leader.pid, "start_time": leader.start_time, "observed": "unresolved" }])
    );
    assert_eq!(p["unchecked"], json!(["tree_unproven"]));
    // A confirmed stop.
    let (t, l) = rig.spawn_codex(None).await;
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    assert_eq!(body(&r)["death_confirmed"], true);
    assert!(checked(&rig, t, l, "process").is_none());
    // A natural exit left unconfirmed.
    rig.pty.script_next(FakeScript {
        kill: KillBehavior::Unconfirmed,
        ..FakeScript::default()
    });
    let (t, l) = rig.spawn_codex(None).await;
    rig.pty.take_child().unwrap().fail_wait();
    rig.until("the exit recorded", || rig.launch(l).exit_reason.is_some())
        .await;
    let p = checked(&rig, t, l, "process").expect("close evidence of an exit");
    assert_eq!(p["unchecked"], json!(["tree_unproven"]));
    assert_eq!(p["identities"].as_array().unwrap().len(), 1);
    // The participant's close left unconfirmed.
    rig.pty.script_next(FakeScript {
        kill: KillBehavior::Unconfirmed,
        ..FakeScript::default()
    });
    let (t, l) = rig.spawn_codex(None).await;
    let end = rig.sup.now() + Duration::from_secs(2);
    assert_eq!(
        shutdown(rig.sup.clone(), end).await,
        ParticipantEnd::LeftRunning
    );
    let p = checked(&rig, t, l, "process").expect("close evidence of the participant");
    assert_eq!(p["unchecked"], json!(["tree_unproven"]));
    assert_eq!(p["identities"].as_array().unwrap().len(), 1);
    rig.srv.stop().await;
    // A close report with no survivors still names the unproven tree.
    let p = serde_json::to_value(close_evidence(
        LaunchId::from_u128(1),
        &CloseReceipt::InTime(report(false, vec![])),
    ))
    .unwrap();
    assert_eq!(p["identities"], json!([]));
    assert_eq!(p["unchecked"], json!(["tree_unproven"]));
    let p = serde_json::to_value(close_evidence(
        LaunchId::from_u128(1),
        &CloseReceipt::NotReceived(None),
    ))
    .unwrap();
    assert_eq!(
        p["unchecked"],
        json!(["tree_unproven", "result_not_received"])
    );
}

/// PR10a2-CL-7 `recorded`, a close not received by the cutoff: a stop whose first `kill_by`
/// reported survivors and whose join the clock makes late records those survivors with
/// `result_not_received`; a report published only after the cutoff is not kept, a report
/// whose publication the cutoff passed between its report and its slot write included.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recorded_late_close() {
    for (hold_at, published_late) in [
        (Pause::KillTaskEnd, false),
        (Pause::KillTaskStart, true),
        (Pause::KillReportPublish, true),
    ] {
        let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, |c| {
            c.kill_budget = Duration::from_secs(60);
        })
        .await;
        rig.pty.script_next(FakeScript {
            kill: KillBehavior::Unconfirmed,
            ..FakeScript::default()
        });
        let (t, l) = rig.spawn_codex(None).await;
        let leader = super::prober_leader(&rig, l);
        let hold = rig.pauses.arm(hold_at);
        let reply = super::support::http::spawn_send(
            &rig.srv,
            &rig.srv.command(
                "stop",
                &rig.owner,
                super::cid(),
                &json!({ "thread_id": t }).to_string(),
            ),
        );
        hold.wait();
        rig.clock.advance(crate::supervisor::PROOF_CUTOFF);
        hold.go();
        let r = reply.await.unwrap();
        assert_eq!(body(&r)["death_confirmed"], false);
        let p = checked(&rig, t, l, "process").expect("close evidence");
        let want = if published_late {
            json!([])
        } else {
            json!([{ "pid": leader.pid, "start_time": leader.start_time, "observed": "unresolved" }])
        };
        assert_eq!(p["identities"], want, "{hold_at:?}");
        assert_eq!(
            p["unchecked"],
            json!(["tree_unproven", "result_not_received"])
        );
        rig.srv.stop().await;
    }
}
