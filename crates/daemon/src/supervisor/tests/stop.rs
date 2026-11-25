//! Stop (design §2.3 "Stop"; PR10a1-CL-10, CL-11, CL-12).

use std::time::Duration;

use chaldea_pty_runtime::ProcessIdentity;
use chaldea_pty_runtime::fake::{FakeEvent, FakeScript, KillBehavior};
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

fn signals(events: &[FakeEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, FakeEvent::Signal))
        .count()
}

/// PR10a1-CL-11 `records_one_exit`: one `launch_exited`, from the stop, with the leader's
/// exit the close saw (the fake's kill exits it with signal 9).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn records_one_exit() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    let v = json(&r);
    assert_eq!(
        (v["exit_code"].clone(), v["signal"].clone()),
        (json!(null), json!(9))
    );
    let exits: Vec<_> = rig
        .events(t)
        .into_iter()
        .filter(|(k, _)| k == "launch_exited")
        .collect();
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0].1["reason"], "stopped");
    assert_eq!(exits[0].1["signal"], 9);
    assert_eq!(rig.launch(l).exit_signal, Some(9));
    rig.srv.stop().await;
}

/// PR10a1-CL-11 `reports_from_request_start`: one clock for the server and the supervisor;
/// the stop held at its start past `RequestStart` + 5 s still kills and confirms, late; held
/// 1 s, in time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reports_from_request_start() {
    let rig = Rig::new().await;
    for (skew, within) in [(6, false), (1, true)] {
        let (t, _l) = rig.spawn_codex(None).await;
        let child = rig.pty.take_child().unwrap();
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
        rig.clock.skew(Duration::from_secs(skew));
        start.go();
        let r = reply.await.unwrap();
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        let v = json(&r);
        assert_eq!(v["death_confirmed"], true, "skew {skew}");
        assert_eq!(v["confirmed_within_deadline"], within, "skew {skew}");
        assert!(signals(&child.log()) > 0);
    }
    rig.srv.stop().await;
}

/// PR10a1-CL-11 `not_attempted_unconfirmed`: a report that is neither busy nor proving is
/// not retried: one task call and the close's, no signal, death unconfirmed, the token
/// still counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn not_attempted_unconfirmed() {
    let rig = Rig::new().await;
    rig.pty.script_next(FakeScript {
        kill: KillBehavior::Unconfirmed,
        ..FakeScript::default()
    });
    let (t, l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    let v = json(&r);
    assert_eq!(v["death_confirmed"], false);
    assert_eq!(v["confirmed_within_deadline"], false);
    let log = child.log();
    assert_eq!(kills(&log).len(), 2, "{log:?}");
    assert_eq!(signals(&log), 0);
    assert!(!rig.launch(l).death_confirmed);
    assert!(rig.sup.tracker.count() > 0, "the token stays counted");
    rig.srv.stop().await;
}

/// PR10a1-CL-11 `busy_retry`: the kill-state lock held from before the stop; busy reports
/// are retried with the full list and no signal; released, a call confirms.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_retry() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let id = ProcessIdentity {
        pid: 4242,
        start_time: "s".to_owned(),
        group: "g".to_owned(),
    };
    rig.sup.entry(l).unwrap().record_extra(id.clone());
    let child = rig.pty.take_child().unwrap();
    let held = child.hold_kill_lock();
    let reply = spawn_send(
        &rig.srv,
        &rig.srv.command(
            "stop",
            &rig.owner,
            super::cid(),
            &json!({ "thread_id": t }).to_string(),
        ),
    );
    // A third call has begun only after two busy reports returned.
    rig.until("two busy calls", || kills(&child.log()).len() >= 3)
        .await;
    assert_eq!(signals(&child.log()), 0, "a busy call signals nothing");
    held.release();
    let r = reply.await.unwrap();
    let v = json(&r);
    assert_eq!(v["death_confirmed"], true, "{v}");
    let ks = kills(&child.log());
    assert!(ks.len() >= 3, "{ks:?}");
    assert!(ks.iter().all(|k| k == std::slice::from_ref(&id)), "{ks:?}");
    rig.srv.stop().await;
}

/// PR10a1-CL-11 `stalled_task_unconfirmed`: the task held past its `kill_end`: its calls and
/// the close's kill step are expired, with no signal; death unconfirmed, the token counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_task_unconfirmed() {
    let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, |c| {
        c.kill_budget = Duration::from_millis(500);
    })
    .await;
    let (t, _l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let task = rig.pauses.arm(Pause::KillTaskStart);
    let reply = spawn_send(
        &rig.srv,
        &rig.srv.command(
            "stop",
            &rig.owner,
            super::cid(),
            &json!({ "thread_id": t }).to_string(),
        ),
    );
    task.wait();
    rig.clock.skew(Duration::from_secs(1));
    task.go();
    let r = reply.await.unwrap();
    let v = json(&r);
    assert_eq!(v["death_confirmed"], false, "{v}");
    assert_eq!(signals(&child.log()), 0);
    assert!(rig.sup.tracker.count() > 0);
    rig.srv.stop().await;
}

/// PR10a1-CL-12 `while_starting`: a stop of a launch whose op holds its lock between the
/// reservation commit and `Session::spawn` waits for it, then stops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn while_starting() {
    let rig = Rig::new().await;
    let after = rig.pauses.arm(Pause::AfterReserve);
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let spawn = spawn_send(
        &rig.srv,
        &rig.srv.command(
            "spawn",
            &rig.owner,
            super::cid(),
            &json!({ "agent": "codex", "cwd": cwd }).to_string(),
        ),
    );
    after.wait();
    let t = rig
        .srv
        .store
        .read(|r| Ok(r.threads()?.pop().unwrap().thread_id));
    let stop = spawn_send(
        &rig.srv,
        &rig.srv.command(
            "stop",
            &rig.owner,
            super::cid(),
            &json!({ "thread_id": t }).to_string(),
        ),
    );
    // The stop waits on the op lock: nothing is killed while the spawn is held.
    rig.until("the stop's request is admitted", || {
        rig.srv.store.read(|r| {
            Ok(
                r.events_after(chaldea_core::ids::StreamId::Thread(t), 0, 100)?
                    .events
                    .len(),
            )
        }) > 0
    })
    .await;
    assert!(rig.pty.spawns().is_empty());
    after.go();
    let s = spawn.await.unwrap();
    assert_eq!(json(&s)["outcome"], "started");
    let r = stop.await.unwrap();
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(json(&r)["outcome"], "stopped");
    assert_eq!(json(&r)["death_confirmed"], true);
    rig.srv.stop().await;
}

/// PR10a1-CL-39 `already_exited_in_intent`: the intent finds the launch exited with death
/// confirmed: `already_exited`, completed in the intent's job; in time when the job started
/// by `RequestStart` + 5 s, late when the writer held it past that.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn already_exited_in_intent() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    rig.cmd("stop", &json!({ "thread_id": t })).await;
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    let v = json(&r);
    assert_eq!(v["outcome"], "already_exited");
    assert_eq!(v["launch_id"], json!(l));
    assert_eq!(
        (
            v["death_confirmed"].clone(),
            v["confirmed_within_deadline"].clone()
        ),
        (json!(true), json!(true))
    );
    assert_eq!(
        (v["exit_code"].clone(), v["signal"].clone()),
        (json!(null), json!(null))
    );
    // The runner held before its intent's job until past `RequestStart` + 5 s.
    let (reached, release) = rig.srv.server.shared().faults.arm_runner_gate();
    let reply = spawn_send(
        &rig.srv,
        &rig.srv.command(
            "stop",
            &rig.owner,
            super::cid(),
            &json!({ "thread_id": t }).to_string(),
        ),
    );
    super::support::http::reached(reached).await;
    rig.clock.skew(Duration::from_secs(6));
    drop(release);
    let v = json(&reply.await.unwrap());
    assert_eq!(v["outcome"], "already_exited");
    assert_eq!(v["confirmed_within_deadline"], false);
    rig.srv.stop().await;
}

fn stop_with(
    rig: &Rig,
    t: chaldea_core::ids::ThreadId,
) -> tokio::task::JoinHandle<super::support::http::Reply> {
    spawn_send(
        &rig.srv,
        &rig.srv.command(
            "stop",
            &rig.owner,
            super::cid(),
            &json!({ "thread_id": t }).to_string(),
        ),
    )
}

/// PR10a1-CL-11 `proof_in_close_by`: the task's call signals without proving and is not
/// busy, so it is not retried; the close's kill step is the second kill and the proof.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proof_in_close_by() {
    let rig = Rig::new().await;
    rig.pty.script_next(FakeScript {
        kill: KillBehavior::ProveOnCall(2),
        ..FakeScript::default()
    });
    let (t, l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let r = stop_with(&rig, t).await.unwrap();
    let v = json(&r);
    assert_eq!(v["death_confirmed"], true, "{v}");
    assert_eq!(v["confirmed_within_deadline"], true, "{v}");
    assert_eq!(kills(&child.log()).len(), 2, "{:?}", child.log());
    assert!(rig.launch(l).death_confirmed);
    rig.srv.stop().await;
}

/// PR10a1-CL-10 `late_join_unconfirmed`: no call proves before the close, whose kill step
/// proves the tree dead, and whose return is held until the clock passed the kill's
/// submission + 12 s: the join is `Late`, death unconfirmed despite `tree_dead`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_join_unconfirmed() {
    let rig = Rig::new().await;
    rig.pty.script_next(FakeScript {
        kill: KillBehavior::ProveOnCall(2),
        ..FakeScript::default()
    });
    let (t, l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let end = rig.pauses.arm(Pause::KillTaskEnd);
    let reply = stop_with(&rig, t);
    end.wait();
    assert_eq!(kills(&child.log()).len(), 2, "the close's kill step ran");
    rig.clock
        .skew(crate::supervisor::PROOF_CUTOFF + Duration::from_secs(1));
    end.go();
    let r = reply.await.unwrap();
    let v = json(&r);
    assert_eq!(v["death_confirmed"], false, "{v}");
    assert!(!rig.launch(l).death_confirmed);
    rig.srv.stop().await;
}

/// PR10a1-CL-11 `reused_proof`: the task's first call proves for the full, non-empty list;
/// the close reuses that report for the identical list and records no second kill.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reused_proof() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let id = ProcessIdentity {
        pid: 4242,
        start_time: "s".to_owned(),
        group: "g".to_owned(),
    };
    rig.sup.entry(l).unwrap().record_extra(id.clone());
    let child = rig.pty.take_child().unwrap();
    let r = stop_with(&rig, t).await.unwrap();
    let v = json(&r);
    assert_eq!(v["death_confirmed"], true, "{v}");
    assert_eq!(v["confirmed_within_deadline"], true, "{v}");
    assert_eq!(kills(&child.log()), vec![vec![id]]);
    rig.srv.stop().await;
}

/// PR10a1-CL-39 `replay_bytes`: a receipt row written directly, holding a `StopResult` whose
/// whitespace and key order differ from serde's: the replay returns those bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_bytes() {
    use chaldea_core::command::CommandResult;
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let args = json!({ "thread_id": t }).to_string();
    let first = super::cid();
    let r = spawn_send(&rig.srv, &rig.srv.command("stop", &rig.owner, first, &args))
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    // The same token and request identity under a new `command_id`, completed with bytes
    // serde would not write.
    let row = rig.srv.receipt(first).unwrap();
    let body = format!(
        "{{ \"signal\" : 9,\n  \"exit_code\":null, \"confirmed_within_deadline\" : true,\"death_confirmed\": true, \"outcome\" :\"stopped\", \"launch_id\": \"{l}\" }}"
    );
    let stored = CommandResult {
        http_status: 200,
        body: body.clone(),
    };
    let second = super::cid();
    let job: crate::http::Job<()> = Box::new(move |tx| {
        tx.receipt_begin(second, row.token, row.request_sha256)?;
        tx.receipt_complete(second, &stored)?;
        Ok(())
    });
    tokio::task::block_in_place(|| rig.srv.store.run(job).unwrap());
    let r = spawn_send(
        &rig.srv,
        &rig.srv.command("stop", &rig.owner, second, &args),
    )
    .await
    .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(String::from_utf8_lossy(&r.body), body);
    rig.srv.stop().await;
}

/// PR10a1-CL-11 `proof_before_slow_join`: the task's first call proves and sends its report;
/// the close is held before it starts until the clock passed the kill's submission + 12 s:
/// the join is late, the early report still confirms death with its method, and the token
/// stays counted until the close ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proof_before_slow_join() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let close = rig.pauses.arm(Pause::BeforeCloseBy);
    let reply = stop_with(&rig, t);
    close.wait();
    assert!(
        rig.sup.tracker.count() > 0,
        "counted while the close is held"
    );
    rig.clock
        .skew(crate::supervisor::PROOF_CUTOFF + Duration::from_secs(1));
    close.go();
    let r = reply.await.unwrap();
    assert_eq!(kills(&child.log()).len(), 1, "the close reused the proof");
    rig.until("the close released the token", || {
        rig.sup.tracker.count() == 0
    })
    .await;
    let v = json(&r);
    assert_eq!(v["death_confirmed"], true, "{v}");
    let row = rig.launch(l);
    assert!(row.death_confirmed);
    assert!(
        rig.events(t)
            .iter()
            .any(|(k, p)| k == "launch_death_confirmed" && p["method"] == "fake"),
        "{:?}",
        rig.events(t)
    );
    rig.srv.stop().await;
}
