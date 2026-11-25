//! Spawn, start, stop, exit, and close through the command route (design §2.3).

use chaldea_core::payload::ExitReason;
use chaldea_core::state::ProcessState;
use chaldea_pty_runtime::ChildExit;
use serde_json::json;

use super::{Rig, json};

/// A Codex spawn with a prompt: the reservation and the start each in one job, the prompt
/// the argv's last element after `--`, and the positional input `sent` after
/// `launch_started`'s seq.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_records_reservation_and_start() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(Some("hello world")).await;
    let spawns = rig.pty.spawns();
    assert_eq!(spawns.len(), 1);
    let argv: Vec<String> = spawns[0]
        .argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(argv[0], super::PROGRAM);
    assert_eq!(&argv[argv.len() - 2..], ["--", "hello world"]);
    let row = rig.thread(t);
    assert_eq!(row.state.process, ProcessState::Running);
    assert_eq!(row.current_launch_id, Some(l));
    let events = rig.events(t);
    let kinds: Vec<&str> = events.iter().map(|(k, _)| k.as_str()).collect();
    let started = kinds.iter().position(|k| *k == "launch_started").unwrap();
    assert!(kinds.iter().position(|k| *k == "launch_reserved").unwrap() < started);
    let sent = events
        .iter()
        .find(|(k, p)| k == "input_state_changed" && p["to"] == "sent")
        .expect("the positional input is sent");
    let started_seq = rig.srv.store.read(move |r| {
        Ok(
            r.events_after(chaldea_core::ids::StreamId::Thread(t), 0, 10_000)?
                .events
                .into_iter()
                .find(|e| serde_json::to_value(&e.kind).unwrap() == "launch_started")
                .unwrap()
                .seq,
        )
    });
    assert_eq!(sent.1["sent_after_seq"], json!(started_seq));
    rig.srv.stop().await;
}

/// A stop of a running launch: one `launch_exited` (`stopped`) with death confirmed by the
/// kill's proving report, `launch_death_confirmed` with its method, and the reply's fields.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_confirms_and_records_one_exit() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = json(&r);
    assert_eq!(v["outcome"], "stopped");
    assert_eq!(v["death_confirmed"], true);
    assert_eq!(v["confirmed_within_deadline"], true);
    let row = rig.launch(l);
    assert_eq!(row.exit_reason, Some(ExitReason::Stopped));
    assert!(row.death_confirmed);
    assert_eq!(row.death_method.as_deref(), Some("fake"));
    let kinds = rig.kinds(t);
    assert_eq!(kinds.iter().filter(|k| *k == "launch_exited").count(), 1);
    assert!(kinds.iter().any(|k| k == "launch_death_confirmed"));
    assert_eq!(rig.thread(t).state.process, ProcessState::Exited);
    // A second stop finds it exited, in the intent.
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    assert_eq!(json(&r)["outcome"], "already_exited");
    assert!(rig.sink.0.lock().unwrap().is_empty(), "no fatal");
    rig.srv.stop().await;
}

/// The child exits by itself: one `launch_exited` (`exit`) with its code.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn natural_exit_records_its_code() {
    let rig = Rig::new().await;
    let (_t, l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    child.exit(ChildExit {
        code: Some(3),
        signal: None,
    });
    rig.until("the exit is recorded", || {
        rig.launch(l).exit_reason.is_some()
    })
    .await;
    let row = rig.launch(l);
    assert_eq!(row.exit_reason, Some(ExitReason::Exit));
    assert_eq!(row.exit_code, Some(3));
    assert!(row.death_confirmed);
    rig.srv.stop().await;
}

/// No program: `spawn_failed` with death confirmed, and the positional input `rejected`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_program_is_spawn_failed() {
    let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, |c| {
        c.program = Some(std::sync::Arc::new(|_| None));
    })
    .await;
    let r = rig
        .cmd(
            "spawn",
            &json!({ "agent": "codex", "cwd": rig.cwd.path().to_string_lossy(), "prompt": "hi" }),
        )
        .await;
    assert_eq!(r.status, 200);
    let v = json(&r);
    assert_eq!(v["outcome"], "spawn_failed");
    let l = serde_json::from_value(v["launch_id"].clone()).unwrap();
    let row = rig.launch(l);
    assert_eq!(row.exit_reason, Some(ExitReason::SpawnFailed));
    assert!(row.death_confirmed);
    let t = serde_json::from_value(v["thread_id"].clone()).unwrap();
    assert!(
        rig.events(t)
            .iter()
            .any(|(k, p)| k == "input_state_changed" && p["to"] == "rejected")
    );
    assert!(rig.pty.spawns().is_empty());
    rig.srv.stop().await;
}

/// Close after a confirmed stop: the thread is closed, its tokens revoked, and a resume is
/// refused `thread_closed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_after_stop() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let hook = rig
        .srv
        .token(crate::store::TokenKind::Hook, Some(t), Some(l))
        .await;
    assert_eq!(
        rig.cmd("stop", &json!({ "thread_id": t })).await.status,
        200
    );
    let r = rig
        .cmd("close", &json!({ "thread_id": t, "expected_launch_id": l }))
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(json(&r)["stop"], serde_json::Value::Null);
    assert!(rig.thread(t).closed);
    assert!(rig.srv.token_row(&hook).unwrap().revoked_at.is_some());
    let r = rig
        .cmd(
            "resume",
            &json!({ "thread_id": t, "expected_launch_id": l }),
        )
        .await;
    assert_eq!(r.status, 409);
    assert_eq!(json(&r)["code"], "thread_closed");
    rig.srv.stop().await;
}

/// `close --stop` of a live launch: the inner stop's result, then the thread closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_with_stop() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let r = rig
        .cmd(
            "close",
            &json!({ "thread_id": t, "expected_launch_id": l, "stop": true }),
        )
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = json(&r);
    assert_eq!(v["stop"]["outcome"], "stopped");
    assert_eq!(v["stop"]["death_confirmed"], true);
    assert!(rig.thread(t).closed);
    rig.srv.stop().await;
}
