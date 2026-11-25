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

/// Every kill and close gets the launch's full recorded list (PR10a1-CL-10): a stop's, a
/// natural exit's, the shutdown participant's, and a late creation's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_list_every_call() {
    use crate::supervisor::components::shutdown;
    let id = ProcessIdentity {
        pid: 4242,
        start_time: "s".to_owned(),
        group: "g".to_owned(),
    };
    let only_id = |path: &str, child: &chaldea_pty_runtime::fake::FakeChild| {
        let ks = kills(&child.log());
        assert!(!ks.is_empty(), "{path}");
        assert!(
            ks.iter().all(|k| k == std::slice::from_ref(&id)),
            "{path}: {ks:?}"
        );
    };
    // A stop.
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    rig.sup.entry(l).unwrap().record_extra(id.clone());
    let child = rig.pty.take_child().unwrap();
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    assert_eq!(r.status, 200);
    only_id("stop", &child);
    // A natural exit.
    let (_t, l) = rig.spawn_codex(None).await;
    rig.sup.entry(l).unwrap().record_extra(id.clone());
    let child = rig.pty.take_child().unwrap();
    child.exit(chaldea_pty_runtime::ChildExit {
        code: Some(0),
        signal: None,
    });
    rig.until("the exit recorded", || rig.launch(l).exit_reason.is_some())
        .await;
    only_id("natural exit", &child);
    // The participant.
    let (_t, l) = rig.spawn_codex(None).await;
    rig.sup.entry(l).unwrap().record_extra(id.clone());
    let child = rig.pty.take_child().unwrap();
    shutdown(rig.sup.clone(), rig.sup.now() + Duration::from_secs(2)).await;
    only_id("participant", &child);
    rig.srv.stop().await;
    // A late creation: admission closed while the op holds its session unpublished.
    let rig = Rig::new().await;
    let held = rig.pauses.arm(Pause::BeforePublish);
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let reply = spawn_send(
        &rig.srv,
        &rig.srv.command(
            "spawn",
            &rig.owner,
            super::cid(),
            &json!({ "agent": "codex", "cwd": cwd }).to_string(),
        ),
    );
    held.wait();
    let l = rig
        .srv
        .store
        .read(|r| Ok(r.threads()?.pop().unwrap().current_launch_id.unwrap()));
    rig.sup.entry(l).unwrap().record_extra(id.clone());
    let child = rig.pty.take_child().unwrap();
    rig.sup.tracker.close_admission();
    held.go();
    let _ = reply.await;
    rig.until("the late close recorded", || {
        rig.launch(l).exit_reason.is_some()
    })
    .await;
    only_id("late creation", &child);
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
/// reservation commit and `Session::spawn` waits for it, then stops it; released after the
/// clock passed `RequestStart` + 5 s, `confirmed_within_deadline` is false.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn while_starting() {
    for late in [false, true] {
        while_starting_with(late).await;
    }
}

async fn while_starting_with(late: bool) {
    let rig = Rig::new().await;
    let (spawn, stop, after, start) = stop_while_starting(&rig).await;
    // The stop's request is in its op: it goes on to wait on the op lock, and nothing is
    // killed while the spawn is held.
    start.go();
    assert!(rig.pty.spawns().is_empty());
    if late {
        rig.clock.skew(Duration::from_secs(6));
    }
    after.go();
    let s = spawn.await.unwrap();
    assert_eq!(json(&s)["outcome"], "started");
    let r = stop.await.unwrap();
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(json(&r)["outcome"], "stopped");
    assert_eq!(json(&r)["death_confirmed"], true);
    assert_eq!(json(&r)["confirmed_within_deadline"], !late);
    rig.srv.stop().await;
}

/// A spawn held after its reservation commit (op lock held, `starting`), and a stop of its
/// thread held at its start: returns the spawn's and the stop's replies and the pauses.
async fn stop_while_starting(
    rig: &Rig,
) -> (
    tokio::task::JoinHandle<super::support::http::Reply>,
    tokio::task::JoinHandle<super::support::http::Reply>,
    super::Armed,
    super::Armed,
) {
    let after = rig.pauses.arm(Pause::AfterReserve);
    let start = rig.pauses.arm(Pause::StopStart);
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
    start.wait();
    (spawn, stop, after, start)
}

/// PR10a1-CL-12 `starting_past_the_bound`: a stop whose op-lock wait for a `starting` launch
/// passes the op's start + 10 s: 504 `launch_busy`; the spawn then completes and nothing
/// stops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn starting_past_the_bound() {
    let rig = Rig::new().await;
    let (spawn, stop, after, start) = stop_while_starting(&rig).await;
    rig.clock.skew(Duration::from_secs(11));
    start.go();
    let r = stop.await.unwrap();
    assert_eq!(r.status, 504, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(json(&r)["code"], "launch_busy");
    after.go();
    let s = spawn.await.unwrap();
    assert_eq!(json(&s)["outcome"], "started");
    let l: chaldea_core::ids::LaunchId =
        serde_json::from_value(json(&s)["launch_id"].clone()).unwrap();
    assert_eq!(rig.launch(l).exit_reason, None, "nothing stops it later");
    let child = rig.pty.take_child().unwrap();
    assert!(kills(&child.log()).is_empty());
    rig.srv.stop().await;
}

/// PR10a1-CL-12 `failed_start_already_exited`: a stop that waits on the op lock of a
/// `starting` launch whose spawn then fails before any process exists (`NeverStarted`, its
/// death confirmed): `already_exited`, `death_confirmed: true`, and no kill.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_start_already_exited() {
    use chaldea_pty_runtime::fake::FakeScript;
    use chaldea_pty_runtime::{Cleanup, NeverStartedKind, SpawnError};
    let rig = Rig::new().await;
    rig.pty.script_next(FakeScript {
        fail: Some(SpawnError::NeverStarted {
            kind: NeverStartedKind::Setup,
            error: std::io::Error::other("x"),
            cleanup: Cleanup::Confirmed,
        }),
        ..FakeScript::default()
    });
    let (spawn, stop, after, start) = stop_while_starting(&rig).await;
    start.go();
    after.go();
    let s = spawn.await.unwrap();
    assert_eq!(json(&s)["outcome"], "spawn_failed");
    let r = stop.await.unwrap();
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = json(&r);
    assert_eq!(v["outcome"], "already_exited", "{v}");
    assert_eq!(v["death_confirmed"], true, "{v}");
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
/// while the close is held before it starts, the token stays counted; the stop confirms
/// death with the report's method, the close reuses the proof, and the token is released
/// when the close ends. A task that panics right after sending its report still confirms
/// death, and its token stays counted. A join that ends past the kill's submission + 12 s is
/// `ops::report_before_a_late_join`, at the op's waits.
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
    // The task panics right after sending its report: still confirmed, the token counted.
    let (u, ul) = rig.spawn_codex(None).await;
    let fault = rig.pauses.arm_panic(Pause::BeforeCloseBy);
    let reply = stop_with(&rig, u);
    fault.wait();
    fault.go();
    let r = reply.await.unwrap();
    assert_eq!(json(&r)["death_confirmed"], true, "{}", json(&r));
    assert!(rig.launch(ul).death_confirmed);
    assert_eq!(
        rig.sup.tracker.count(),
        1,
        "the panicked close released nothing"
    );
    rig.srv.stop().await;
}

fn exited(events: &[FakeEvent]) -> bool {
    events.iter().any(|e| matches!(e, FakeEvent::Exited))
}

/// PR10a1-CL-11 `kill_before_store_answer`: the `StopRequested` job held inside the writer,
/// past its deadline check: the kill is sent and the tree proven dead while the job is
/// held; released with an I/O error after its commit, its ticket answers `CommitUnknown`,
/// which reaches the sink.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_before_store_answer() {
    use crate::http::StoreFatal;
    use crate::store::test_faults::CommitFault;
    let rig = Rig::new().await;
    let (t, _l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let held = rig.pauses.arm(Pause::InStopRequested);
    let reply = stop_with(&rig, t);
    held.wait();
    rig.until("the kill proved while StopRequested is held", || {
        let log = child.log();
        !kills(&log).is_empty() && signals(&log) > 0 && exited(&log)
    })
    .await;
    assert!(rig.sink.0.lock().unwrap().is_empty());
    *rig.srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    held.go();
    rig.until("CommitUnknown reported", || {
        rig.sink
            .0
            .lock()
            .unwrap()
            .contains(&StoreFatal::CommitUnknown)
    })
    .await;
    let _ = reply.await;
}

/// PR10a1-CL-11 `kill_before_fatal`: the writer retired by a job submitted outside
/// `write_with` (the sink sees nothing of it); the stop's `Store::submit` then fails at
/// once, and when the sink is told, the kill was already submitted; the kill follows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_before_fatal() {
    use crate::http::StoreFatal;
    use crate::store::test_faults::CommitFault;
    let rig = Rig::new().await;
    let (t, _l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let (seen, pauses) = (seen.clone(), rig.pauses.clone());
        *rig.sink.1.lock().unwrap() = Some(Box::new(move |f: &StoreFatal| {
            seen.lock()
                .unwrap()
                .push((f.clone(), pauses.passed(Pause::KillSubmitted)));
        }));
    }
    let start = rig.pauses.arm(Pause::StopStart);
    let reply = stop_with(&rig, t);
    start.wait();
    *rig.srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    let job: crate::http::Job<()> = Box::new(|_| Ok(()));
    let r = tokio::task::block_in_place(|| rig.srv.store.run(job));
    assert!(r.is_err(), "the direct job's commit is unknown");
    assert!(
        seen.lock().unwrap().is_empty(),
        "the sink saw nothing of it"
    );
    start.go();
    rig.until("the sink is told", || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|(f, _)| *f == StoreFatal::Failed)
    })
    .await;
    let first = seen
        .lock()
        .unwrap()
        .iter()
        .find(|(f, _)| *f == StoreFatal::Failed)
        .cloned()
        .unwrap();
    assert!(first.1, "the kill was submitted before the sink was told");
    rig.until("the kill follows", || !kills(&child.log()).is_empty())
        .await;
    let _ = reply.await;
}

/// The thread's kinds from `stopping`'s `status_changed` on.
fn after_stopping(rig: &Rig, t: chaldea_core::ids::ThreadId) -> Vec<String> {
    let ev = rig.events(t);
    let stopping = |(k, p): &(String, serde_json::Value)| {
        k == "status_changed" && p["field"] == "process_state" && p["to"] == "stopping"
    };
    assert_eq!(ev.iter().filter(|e| stopping(e)).count(), 1, "{ev:?}");
    let i = ev.iter().position(stopping).unwrap();
    assert_eq!(ev[i].1["cause"], "stop_requested");
    ev[i..].iter().map(|(k, _)| k.clone()).collect()
}

/// PR10a1-CL-11 `uncommitted_stop_requested`: a `StopRequested` write that expired in the
/// writer's queue, and one rolled back: no second attempt, the kill still sent, and the
/// exit job applies `StopRequested` itself before `launch_exited`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uncommitted_stop_requested() {
    use crate::store::test_faults::CommitFault;
    let rig = Rig::new().await;
    // Expired: the writer held (after the intent committed) while the job waits, its clock
    // then past `not_after`.
    let (t, _l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let start = rig.pauses.arm(Pause::StopStart);
    let written = rig.pauses.arm(Pause::StopWritten);
    let reply = stop_with(&rig, t);
    start.wait();
    let release = rig.srv.hold_writer().await;
    start.go();
    rig.until("the kill is sent", || !kills(&child.log()).is_empty())
        .await;
    rig.srv
        .store
        .clock
        .lead(crate::http::COMPLETION_ATTEMPT + Duration::from_secs(1));
    release.send(()).unwrap();
    written.wait();
    rig.srv.store.clock.lead(Duration::ZERO);
    written.go();
    let r = reply.await.unwrap();
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let ks = after_stopping(&rig, t);
    assert_eq!(ks.first().map(String::as_str), Some("status_changed"));
    assert!(ks.iter().any(|k| k == "launch_exited"), "{ks:?}");
    // Rolled back: no second attempt.
    let (u, _ul) = rig.spawn_codex(None).await;
    let uchild = rig.pty.take_child().unwrap();
    let held = rig.pauses.arm(Pause::InStopRequested);
    let reply = stop_with(&rig, u);
    held.wait();
    *rig.srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::BusyBeforeCommit);
    let again = rig.pauses.arm(Pause::InStopRequested);
    held.go();
    let r = reply.await.unwrap();
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert!(again.reached.try_recv().is_err(), "no second attempt");
    assert!(!kills(&uchild.log()).is_empty());
    let ks = after_stopping(&rig, u);
    assert!(ks.iter().any(|k| k == "launch_exited"), "{ks:?}");
    rig.srv.stop().await;
}

/// PR10a1-CL-11 `proof_cutoff`: on a stopped clock, the seal, the kill's submission, and the
/// task's start at three instants (`S`, `S + 2 s`, `S + 5 s`); the report and the join both
/// read the clock at the kill's submission + 12 s less 1 ms (accepted) and at exactly it
/// (`Late`, unconfirmed). A cutoff from the seal or from the task's start classifies one of
/// them the other way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proof_cutoff() {
    let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, |c| {
        c.kill_budget = Duration::from_secs(60);
    })
    .await;
    for (before, want) in [(Duration::from_millis(1), true), (Duration::ZERO, false)] {
        let (t, l) = rig.spawn_codex(None).await;
        let seal = rig.pauses.arm(Pause::StopAfterSeal);
        let submit = rig.pauses.arm(Pause::KillSubmitted);
        let task = rig.pauses.arm(Pause::KillTaskStart);
        let reply = stop_with(&rig, t);
        seal.wait();
        rig.clock.stop();
        rig.clock.advance(Duration::from_secs(2));
        seal.go();
        submit.wait();
        rig.clock.advance(Duration::from_secs(3));
        submit.go();
        task.wait();
        rig.clock
            .advance(crate::supervisor::PROOF_CUTOFF - Duration::from_secs(3) - before);
        task.go();
        let r = reply.await.unwrap();
        let v = json(&r);
        assert_eq!(v["death_confirmed"], want, "{before:?}: {v}");
        assert_eq!(rig.launch(l).death_confirmed, want);
    }
    rig.srv.stop().await;
}

/// PR10a1-CL-11 `late_proof_confirms`: a kill that proves the tree dead after
/// `RequestStart` + 5 s and within its budget: `death_confirmed: true`,
/// `confirmed_within_deadline: false`, the token released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_proof_confirms() {
    let rig = Rig::new().await;
    rig.pty.script_next(FakeScript {
        kill: KillBehavior::ExitAfter(Duration::from_millis(1500)),
        ..FakeScript::default()
    });
    let (t, l) = rig.spawn_codex(None).await;
    // The op starts 4 s after `RequestStart` on the supervisor's clock; the proof comes
    // 1.5 s later.
    let start = rig.pauses.arm(Pause::StopStart);
    let reply = stop_with(&rig, t);
    start.wait();
    rig.clock.skew(Duration::from_secs(4));
    start.go();
    let r = reply.await.unwrap();
    let v = json(&r);
    assert_eq!(v["death_confirmed"], true, "{v}");
    assert_eq!(v["confirmed_within_deadline"], false, "{v}");
    assert!(rig.launch(l).death_confirmed);
    rig.until("the token released", || rig.sup.tracker.count() == 0)
        .await;
    rig.srv.stop().await;
}

/// PR10a1-CL-11 `reply_by` (the retry half): while the op runs after the intent committed,
/// the same `command_id` is 409 `in_progress`; once the result is stored, it replays the
/// stored `StopResult` byte for byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reply_by() {
    let rig = Rig::new().await;
    let (t, _l) = rig.spawn_codex(None).await;
    let id = super::cid();
    let args = json!({ "thread_id": t }).to_string();
    let held = rig.pauses.arm(Pause::StopWritten);
    let first = spawn_send(&rig.srv, &rig.srv.command("stop", &rig.owner, id, &args));
    held.wait();
    let r = rig
        .srv
        .send(&rig.srv.command("stop", &rig.owner, id, &args))
        .await;
    assert_eq!(
        (r.status, json(&r)["code"].clone()),
        (409, json!("in_progress"))
    );
    held.go();
    let r1 = first.await.unwrap();
    assert_eq!(r1.status, 200);
    let r2 = rig
        .srv
        .send(&rig.srv.command("stop", &rig.owner, id, &args))
        .await;
    assert_eq!((r2.status, &r2.body), (200, &r1.body));
    rig.srv.stop().await;
}

/// PR10a1-CL-29 `seal_before_stop_requested`: a unit held in its recheck when the stop
/// begins; with the `StopRequested` job held in the writer and the kill task held before
/// its first call, the unit, released, settles `NotStarted(Closed)` and no byte reaches the
/// child: the stop's seal came first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seal_before_stop_requested() {
    use chaldea_pty_runtime::{Refusal, WriteOutcome};
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let q = super::exit::queue_behind_recheck(&rig, l).await;
    let held = rig.pauses.arm(Pause::InStopRequested);
    // The kill task held before its first call, so its close cannot seal the queue first.
    let task = rig.pauses.arm(Pause::KillTaskStart);
    let reply = stop_with(&rig, t);
    held.wait();
    task.wait();
    q.gate.send(()).unwrap();
    let a =
        q.a.recv_deadline(std::time::Instant::now() + Duration::from_secs(20))
            .unwrap();
    assert_eq!(a.outcome, WriteOutcome::NotStarted(Refusal::Closed));
    assert!(child.writes().is_empty(), "{:?}", child.writes());
    task.go();
    held.go();
    assert_eq!(reply.await.unwrap().status, 200);
    rig.srv.stop().await;
}

/// PR10a1-CL-10 `drop_never_confirms`: a kill task that panics before its first call drops
/// its session; the drop's implicit close (empty list) kills the tree, yet confirms
/// nothing: `death_confirmed: false`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drop_never_confirms() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let fault = rig.pauses.arm_panic(Pause::KillTaskStart);
    let reply = stop_with(&rig, t);
    fault.wait();
    fault.go();
    let r = reply.await.unwrap();
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(json(&r)["death_confirmed"], false, "{}", json(&r));
    assert!(!rig.launch(l).death_confirmed);
    let ks = kills(&child.log());
    assert_eq!(
        ks,
        vec![Vec::<ProcessIdentity>::new()],
        "only the drop's close killed"
    );
    rig.srv.stop().await;
}
