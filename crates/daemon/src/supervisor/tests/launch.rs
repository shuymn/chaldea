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

fn argv(spec: &chaldea_pty_runtime::SpawnSpec) -> Vec<String> {
    spec.argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn env_of<'a>(spec: &'a chaldea_pty_runtime::SpawnSpec, key: &str) -> Vec<&'a std::ffi::OsStr> {
    spec.env
        .iter()
        .filter(|(k, _)| chaldea_pty_runtime::env_key_eq(k, std::ffi::OsStr::new(key)))
        .map(|(_, v)| v.as_os_str())
        .collect()
}

/// PR10a1-CL-2: the argv comes from the mapped `host_os`, whatever the runner's OS.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_row() {
    for (host, flag) in [
        (chaldea_core::adapter::HostOs::Windows, true),
        (chaldea_core::adapter::HostOs::Macos, false),
    ] {
        let rig = Rig::with(host, |_| {}).await;
        rig.spawn_codex(None).await;
        let a = argv(&rig.pty.spawns()[0]);
        let has = a
            .windows(2)
            .any(|w| w == ["-c", "tui.disable_paste_burst=true"]);
        assert_eq!(has, flag, "{host:?}: {a:?}");
        rig.srv.stop().await;
    }
}

/// PR10a1-CL-6: the launch's own hook token (one row per launch), the URLs and thread id,
/// the scrubbed names gone and the user's variables kept, and Claude's session flag: fresh,
/// then resumed after binding.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn env_and_session_args() {
    use std::ffi::OsString;
    let seeded: Vec<(OsString, OsString)> = [
        ("CLAUDECODE", "1"),
        ("CLAUDE_CODE_CHILD_SESSION", "x"),
        ("CLAUDE_CODE_SESSION_ID", "x"),
        ("CLAUDE_CODE_MESSAGING_TOKEN", "x"),
        ("CODEX_THREAD_ID", "x"),
        ("CODEX_SESSION_ID", "x"),
        ("CHALDEA_AGENT_TOKEN", "x"),
        ("CHALDEA_HOOK_TOKEN", "stale"),
        ("CODEX_HOME", "/work/codex"),
        ("CLAUDE_CODE_USE_BEDROCK", "1"),
        ("PATH", "/work/bin"),
    ]
    .into_iter()
    .map(|(k, v)| (OsString::from(k), OsString::from(v)))
    .collect();
    let mut env = seeded.clone();
    // Mixed case: the same variables on Windows, other variables on macOS.
    env.push((
        OsString::from("Chaldea_Hook_Token"),
        OsString::from("mixed"),
    ));
    env.push((OsString::from("claudecode"), OsString::from("mixed")));
    let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, move |c| {
        c.env = Some(env);
    })
    .await;
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let r = rig
        .cmd("spawn", &json!({ "agent": "claude", "cwd": cwd }))
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = json(&r);
    let t: chaldea_core::ids::ThreadId = serde_json::from_value(v["thread_id"].clone()).unwrap();
    let l1: chaldea_core::ids::LaunchId = serde_json::from_value(v["launch_id"].clone()).unwrap();
    rig.bind(t, l1, "the-session").await;
    rig.cmd("stop", &json!({ "thread_id": t })).await;
    let r = rig
        .cmd(
            "resume",
            &json!({ "thread_id": t, "expected_launch_id": l1 }),
        )
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let l2: chaldea_core::ids::LaunchId =
        serde_json::from_value(json(&r)["launch_id"].clone()).unwrap();
    let spawns = rig.pty.spawns();
    assert_eq!(spawns.len(), 2);
    let mut secrets = Vec::new();
    for (spec, l) in [(&spawns[0], l1), (&spawns[1], l2)] {
        let one = |k: &str| {
            let v = env_of(spec, k);
            assert_eq!(v.len(), 1, "{k}: {v:?}");
            v[0].to_string_lossy().into_owned()
        };
        assert!(one("CHALDEA_URL").starts_with("http://127.0.0.1:"));
        assert_eq!(
            one("CHALDEA_HOOK_URL"),
            format!("{}/hook", one("CHALDEA_URL"))
        );
        assert_eq!(one("CHALDEA_THREAD_ID"), t.to_string());
        let secret = one("CHALDEA_HOOK_TOKEN");
        let row = rig.srv.token_row(&secret).expect("the launch's token row");
        assert_eq!(row.kind, crate::store::TokenKind::Hook);
        assert_eq!((row.thread_id, row.launch_id), (Some(t), Some(l)));
        secrets.push(secret);
        for name in [
            "CLAUDECODE",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CODEX_THREAD_ID",
            "CODEX_SESSION_ID",
            "CHALDEA_AGENT_TOKEN",
        ] {
            assert!(env_of(spec, name).is_empty(), "{name} scrubbed");
        }
        assert_eq!(one("CODEX_HOME"), "/work/codex");
        assert_eq!(one("CLAUDE_CODE_USE_BEDROCK"), "1");
        assert_eq!(one("PATH"), "/work/bin");
    }
    assert_ne!(secrets[0], secrets[1], "a token per launch");
    let a1 = argv(&spawns[0]);
    assert!(
        a1.windows(2)
            .any(|w| w == ["--session-id".to_owned(), l1.to_string()]),
        "{a1:?}"
    );
    let a2 = argv(&spawns[1]);
    assert!(
        a2.windows(2).any(|w| w == ["--resume", "the-session"]),
        "{a2:?}"
    );
    assert!(
        !a2.iter().any(|a| *a == l2.to_string()),
        "the launch id is not a session"
    );
    // C1-1: both carry the daemon's settings file.
    for a in [&a1, &a2] {
        assert!(
            a.windows(2)
                .any(|w| w == ["--settings", "/work/state/claude-settings.json"]),
            "{a:?}"
        );
    }
    rig.srv.stop().await;
}

/// PR10a1-CL-7: the start job records the backend's identity, and the positional input's
/// `sent` (after `launch_started`'s seq) in the same commit, right after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn started_job() {
    let rig = Rig::new().await;
    // A second spawn, so the backend's pid is not the first one a fixed value could match.
    rig.spawn_codex(None).await;
    let (t, l) = rig.spawn_codex(Some("hello")).await;
    let child = rig.pty.take_child().unwrap();
    let id = child.identity();
    assert_ne!(id.pid, 1);
    let row = rig.launch(l);
    assert_eq!(row.pid, Some(id.pid));
    let events = rig.srv.store.read(move |r| {
        Ok(
            r.events_after(chaldea_core::ids::StreamId::Thread(t), 0, 10_000)?
                .events,
        )
    });
    let started = events
        .iter()
        .find(|e| serde_json::to_value(&e.kind).unwrap() == "launch_started")
        .unwrap();
    let p: serde_json::Value = serde_json::from_str(started.payload.get()).unwrap();
    assert_eq!(p["pid"], id.pid);
    assert_eq!(p["start_time"], id.start_time);
    assert_eq!(p["pgid_or_job"], id.group);
    let sent = events
        .iter()
        .find(|e| {
            serde_json::to_value(&e.kind).unwrap() == "input_state_changed"
                && serde_json::from_str::<serde_json::Value>(e.payload.get()).unwrap()["to"]
                    == "sent"
        })
        .unwrap();
    let sp: serde_json::Value = serde_json::from_str(sent.payload.get()).unwrap();
    assert_eq!(sp["sent_after_seq"], json!(started.seq));
    assert!(sent.seq > started.seq);
    // Derived from the start job's cause, and nothing else between them.
    assert_eq!(
        sent.idempotency_key.0,
        format!(
            "derived:launch:{l}:started:input:{}",
            sp["input_id"].as_str().unwrap()
        )
    );
    assert!(
        events
            .iter()
            .filter(|e| e.seq > started.seq && e.seq < sent.seq)
            .all(|e| e
                .idempotency_key
                .0
                .starts_with(&format!("derived:launch:{l}:started:"))),
        "{events:?}"
    );
    rig.srv.stop().await;
}

/// PR10a1-CL-9: each spawn error and cleanup.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_errors() {
    use chaldea_pty_runtime::fake::FakeScript;
    use chaldea_pty_runtime::{Cleanup, NeverStartedKind, ProcessIdentity, SpawnError};
    let unresolved = || Cleanup::Unresolved {
        kill: None,
        close: None,
        drain_abandoned: false,
    };
    let id = |start: &str, group: &str| ProcessIdentity {
        pid: 77,
        start_time: start.to_owned(),
        group: group.to_owned(),
    };
    let rig = Rig::new().await;
    type Case = (SpawnError, bool, &'static str, serde_json::Value);
    let cases: Vec<Case> = vec![
        (
            SpawnError::NeverStarted {
                kind: NeverStartedKind::Setup,
                error: std::io::Error::other("x"),
                cleanup: Cleanup::Confirmed,
            },
            true,
            "rejected",
            serde_json::Value::Null,
        ),
        (
            SpawnError::StartedThenFailed {
                stage: "s",
                error: std::io::Error::other("x"),
                identity: Some(id("st", "g")),
                cleanup: Cleanup::Confirmed,
            },
            true,
            "unconfirmed",
            serde_json::Value::Null,
        ),
        (
            SpawnError::StartedThenFailed {
                stage: "s",
                error: std::io::Error::other("x"),
                identity: Some(id("st", "g")),
                cleanup: unresolved(),
            },
            false,
            "unconfirmed",
            json!({ "pid": 77, "start_time": "st", "pgid_or_job": "g" }),
        ),
        (
            SpawnError::StartedThenFailed {
                stage: "s",
                error: std::io::Error::other("x"),
                identity: Some(id("", "")),
                cleanup: unresolved(),
            },
            false,
            "unconfirmed",
            json!({ "pid": 77 }),
        ),
        (
            SpawnError::StartedThenFailed {
                stage: "s",
                error: std::io::Error::other("x"),
                identity: None,
                cleanup: unresolved(),
            },
            false,
            "unconfirmed",
            serde_json::Value::Null,
        ),
    ];
    for (fail, confirmed, input, failed_start) in cases {
        let what = format!("{fail}");
        rig.pty.script_next(FakeScript {
            fail: Some(fail),
            ..FakeScript::default()
        });
        let cwd = rig.cwd.path().to_string_lossy().into_owned();
        let r = rig
            .cmd(
                "spawn",
                &json!({ "agent": "codex", "cwd": cwd, "prompt": "hi" }),
            )
            .await;
        assert_eq!(r.status, 200, "{what}");
        let v = json(&r);
        assert_eq!(v["outcome"], "spawn_failed", "{what}");
        let t: chaldea_core::ids::ThreadId =
            serde_json::from_value(v["thread_id"].clone()).unwrap();
        let events = rig.events(t);
        assert!(!events.iter().any(|(k, _)| k == "launch_started"), "{what}");
        let exited = &events.iter().find(|(k, _)| k == "launch_exited").unwrap().1;
        assert_eq!(exited["reason"], "spawn_failed", "{what}");
        assert_eq!(exited["death_confirmed"], confirmed, "{what}");
        match &failed_start {
            serde_json::Value::Null => {
                assert!(exited.get("failed_start").is_none(), "{what}: {exited}");
            }
            f => assert_eq!(&exited["failed_start"], f, "{what}"),
        }
        assert!(
            events
                .iter()
                .any(|(k, p)| k == "input_state_changed" && p["to"] == input),
            "{what}: {events:?}"
        );
    }
    rig.srv.stop().await;
}

/// PR10a1-CL-17 (start part): `startup_stall` only for an upgraded Codex launch; no launch
/// is upgraded before PR 14, so neither agent raises it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_stall_only_upgraded_codex() {
    let rig = Rig::new().await;
    let (codex, _) = rig.spawn_codex(None).await;
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let r = rig
        .cmd("spawn", &json!({ "agent": "claude", "cwd": cwd }))
        .await;
    let claude: chaldea_core::ids::ThreadId =
        serde_json::from_value(json(&r)["thread_id"].clone()).unwrap();
    for t in [codex, claude] {
        assert!(
            !rig.events(t)
                .iter()
                .any(|(_, p)| p.to_string().contains("startup_stall")),
            "{t}"
        );
    }
    rig.srv.stop().await;
}

/// PR10a1-CL-13 `wait_failed`: the waiter fails: the exit op kills and closes; death is
/// confirmed exactly when the kill proved it, and the status is null.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_failed() {
    use chaldea_pty_runtime::fake::{FakeScript, KillBehavior};
    for (kill, confirmed) in [
        (KillBehavior::ExitOnKill, true),
        (KillBehavior::Ignore, false),
    ] {
        let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, |c| {
            c.kill_budget = std::time::Duration::from_millis(300);
        })
        .await;
        rig.clock.resume();
        rig.pty.script_next(FakeScript {
            kill,
            ..FakeScript::default()
        });
        let (_t, l) = rig.spawn_codex(None).await;
        let child = rig.pty.take_child().unwrap();
        child.fail_wait();
        rig.until("the exit is recorded", || {
            rig.launch(l).exit_reason.is_some()
        })
        .await;
        let row = rig.launch(l);
        assert_eq!(row.exit_reason, Some(ExitReason::Exit));
        assert_eq!(row.death_confirmed, confirmed);
        assert_eq!((row.exit_code, row.exit_signal), (None, None));
        rig.srv.stop().await;
    }
}

/// PR10a1-CL-22 `unrecorded_start_job`: the start job's commit fails with `CommitUnknown`:
/// 503 `outcome_unknown` before the deadline with no completion attempt; after a restart
/// the reconcilers make the receipt `unknown_after_crash`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unrecorded_start_job() {
    use crate::store::test_faults::CommitFault;
    let rig = Rig::new().await;
    let id = super::cid();
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let held = rig.pauses.arm(crate::supervisor::Pause::InStartJob);
    let started = std::time::Instant::now();
    let reply = super::support::http::spawn_send(
        &rig.srv,
        &rig.srv.command(
            "spawn",
            &rig.owner,
            id,
            &json!({ "agent": "codex", "cwd": cwd }).to_string(),
        ),
    );
    held.wait();
    *rig.srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    held.go();
    let r = reply.await.unwrap();
    assert_eq!(
        (r.status, json(&r)["code"].clone()),
        (503, json!("outcome_unknown")),
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert!(started.elapsed() < crate::supervisor::COMMAND_DEADLINE);
    // No completion was attempted (an attempt on the retired writer would report
    // `Failed`): the commit's outcome, then the runner's uncompleted receipt.
    assert_eq!(
        *rig.sink.0.lock().unwrap(),
        [
            crate::http::StoreFatal::CommitUnknown,
            crate::http::StoreFatal::ReceiptUncompleted { command_id: id }
        ]
    );
    drop(rig.sup);
    let (_, mut store) = rig.srv.into_store(std::time::Duration::from_secs(10)).await;
    store.reopen();
    super::restart::restart_all(&store);
    let receipt = store.read(move |r| r.receipt(id)).unwrap();
    assert_eq!(
        receipt.status,
        chaldea_core::command::ReceiptStatus::UnknownAfterCrash
    );
}

/// The hook's job gate: reached, release.
type JobGate = (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
);

/// A spawn held before its start job, with a `SessionStart` of its launch sent to `/hook`
/// (the emitter's chain reaching the spawned identity); returns the rig, the held pause, the
/// hook's reply, the ids, and, with `hold_job`, the gate holding the hook before its job. The
/// hook is inside its pid wait when this returns.
async fn early_session_start(
    stop_clock: bool,
    hold_job: bool,
) -> (
    Rig,
    super::Armed,
    tokio::task::JoinHandle<super::support::http::Reply>,
    chaldea_core::ids::LaunchId,
    uuid::Uuid,
    Option<JobGate>,
) {
    use super::support::hook::{EMITTER_PID, Hook, Launch, chain};
    use crate::supervisor::Pause;
    use chaldea_core::ids::Agent;
    use chaldea_pty_runtime::process::AncestryEnd;
    let (rig, chains, ingest) = Rig::with_chains(|_| {}).await;
    if !stop_clock {
        rig.clock.resume();
    }
    let job = hold_job.then(|| ingest.points.arm_job(1));
    let held = rig.pauses.arm(Pause::BeforeStartJob);
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let _spawn = super::support::http::spawn_send(
        &rig.srv,
        &rig.srv.command(
            "spawn",
            &rig.owner,
            super::cid(),
            &json!({ "agent": "codex", "cwd": cwd }).to_string(),
        ),
    );
    let held = tokio::task::spawn_blocking(move || {
        held.wait();
        held
    })
    .await
    .unwrap();
    let spec = rig.pty.spawns().remove(0);
    let token = env_of(&spec, "CHALDEA_HOOK_TOKEN")[0]
        .to_string_lossy()
        .into_owned();
    let id = rig.pty.take_child().unwrap().identity();
    chains.set(
        EMITTER_PID,
        chain(
            &[
                (EMITTER_PID, "3000", "chaldea"),
                (id.pid, &id.start_time, "codex"),
                (1, "1", "launchd"),
            ],
            AncestryEnd::Root,
        ),
    );
    let row = rig.srv.store.read(|r| Ok(r.threads()?.pop().unwrap()));
    let l = row.current_launch_id.unwrap();
    let hook = Hook::of(
        &Launch {
            thread: row.thread_id,
            launch: l,
            agent: Agent::Codex,
            token,
        },
        "SessionStart",
        "s",
    );
    let reply = super::support::http::spawn_send(&rig.srv, &hook.req(&rig.srv));
    rig.until("the hook is in its pid wait", || {
        crate::http::tests::shared(&rig.srv)
            .faults
            .pid_wait_deadlines()
            .len()
            == 1
    })
    .await;
    (rig, held, reply, l, hook.emit, job)
}

/// PR10a1-CL-7: a `SessionStart` that arrives before the start job commits, with the
/// supervisor wired. Its pid wait ending inside its deadline (a stopped clock, so only the
/// start's commit can end it): the reply follows the start's commit and the launch is bound,
/// `session_bound` after `launch_started`. Its pid wait running out first, the hook then held
/// before its job while the start commits, then released: the PR 9 result, decided at the
/// wait's end, 200 with `raw_unknown` (`unbound`) and no binding, though the job sees the
/// start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn early_hook_pid_wait() {
    use chaldea_core::payload::RawUnknownReason;
    // Inside the deadline.
    let (rig, held, reply, l, _, _) = early_session_start(true, false).await;
    assert!(!reply.is_finished(), "decided only after the start commits");
    held.go();
    rig.until("the hook answered", || reply.is_finished()).await;
    let r = reply.await.unwrap();
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(rig.launch(l).session_id.as_deref(), Some("s"));
    let kinds = rig.kinds(rig.launch(l).thread_id);
    let at = |k: &str| kinds.iter().position(|x| x == k).unwrap();
    assert!(at("launch_started") < at("session_bound"), "{kinds:?}");
    rig.srv.stop().await;
    // Past it.
    let (rig, held, reply, l, emit, job) = early_session_start(false, true).await;
    let (reached, release) = job.unwrap();
    reached
        .await
        .expect("the hook reaches its job after the wait ran out");
    assert_eq!(rig.launch(l).pid, None, "the wait ran out before the start");
    held.go();
    rig.until("the start committed", || rig.launch(l).pid.is_some())
        .await;
    release.send(()).unwrap();
    let r = reply.await.unwrap();
    assert_eq!(r.status, 200, "{r:?}");
    let d = rig
        .srv
        .store
        .read(move |r| r.hook_delivery(l, emit))
        .expect("a delivery");
    let crate::store::Routing::RawUnknown { stream_id, seq } = d.routing else {
        panic!("not raw_unknown: {d:?}");
    };
    let e = rig
        .srv
        .store
        .read(move |r| Ok(r.events_after(stream_id, seq - 1, 1)?.events.remove(0)));
    let chaldea_core::event::TypedEvent::RawUnknown(raw) =
        chaldea_core::event::TypedEvent::decode(&e).unwrap()
    else {
        panic!("{e:?}");
    };
    assert_eq!(raw.reason, RawUnknownReason::Unbound);
    assert_eq!(rig.launch(l).session_id, None);
    rig.srv.stop().await;
}

/// C1-1: `sh` (Claude's hook shell on macOS) runs the settings file's quoted emitter path as
/// one word.
#[cfg(unix)]
#[test]
fn claude_hook_command_in_sh() {
    let exe = "/work/my tools/it's \"x\" $HOME `id`/chaldea";
    let cmd = chaldea_core::launch::claude_hook_command(exe, chaldea_core::adapter::HostOs::Macos);
    // `printf` in place of the emitter: each word on its own line.
    let script = format!("set -- {cmd}; printf '%s\\n' \"$@\"");
    let mut c = std::process::Command::new("/bin/sh");
    c.args(["-c", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped());
    let out = crate::lifecycle::spawn_locked(&mut c)
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{exe}\nhook-emit\nclaude\n")
    );
}
