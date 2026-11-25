//! The four handlers (design §2.2): prompt admission, filesystem refusals, codes, duplicate
//! requests, the caller, and the target scope.

use chaldea_core::ids::{LaunchId, StreamId, ThreadId};
use chaldea_pty_runtime::fake::{FakeEvent, FakeScript, KillBehavior};
use serde_json::json;

use super::support::http::{Reply, spawn_send};
use super::{Rig, cid, json};
use crate::store::TokenKind;

fn host_head(rig: &Rig) -> u64 {
    rig.srv
        .store
        .read(|r| Ok(r.head(StreamId::Host, 0)?.head_seq))
}

fn code(r: &Reply) -> String {
    json(r)["code"].as_str().unwrap_or_default().to_owned()
}

/// A Codex thread whose launch was stopped with its death unconfirmed.
async fn unconfirmed(rig: &Rig) -> (ThreadId, LaunchId) {
    rig.pty.script_next(FakeScript {
        kill: KillBehavior::Unconfirmed,
        ..FakeScript::default()
    });
    let (t, l) = rig.spawn_codex(None).await;
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(json(&r)["death_confirmed"], false);
    (t, l)
}

/// PR10a1-CL-1: each refused prompt is refused in `prepare`, before any receipt, event, or
/// spawn; an accepted one is the argv's last element after `--`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prompt_admission() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    rig.cmd("stop", &json!({ "thread_id": t })).await;
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let r = rig
        .cmd("spawn", &json!({ "agent": "claude", "cwd": cwd }))
        .await;
    let (ct, cl) = (json(&r)["thread_id"].clone(), json(&r)["launch_id"].clone());
    rig.cmd("stop", &json!({ "thread_id": ct })).await;
    let head = host_head(&rig);
    let thread_head = rig.kinds(t).len();
    let spawned = rig.pty.spawns().len();
    let big = "a".repeat(chaldea_core::input::MAX_PROMPT_BYTES + 1);
    let cases = [
        (
            "spawn",
            json!({ "agent": "codex", "cwd": cwd, "prompt": big }),
            400,
            "prompt_too_large",
        ),
        (
            "spawn",
            json!({ "agent": "codex", "cwd": cwd, "prompt": "a\u{1b}b" }),
            400,
            "prompt_rejected",
        ),
        (
            "spawn",
            json!({ "agent": "codex", "cwd": cwd, "prompt": "   " }),
            400,
            "prompt_rejected",
        ),
        (
            "spawn",
            json!({ "agent": "claude", "cwd": cwd, "prompt": "hi" }),
            400,
            "bad_args",
        ),
        (
            "resume",
            json!({ "thread_id": t, "expected_launch_id": l, "prompt": "a\u{7}" }),
            400,
            "prompt_rejected",
        ),
        (
            "resume",
            json!({ "thread_id": ct, "expected_launch_id": cl, "prompt": big }),
            400,
            "bad_args",
        ),
    ];
    for (name, args, status, want) in cases {
        let id = cid();
        let r = rig.cmd_as(&rig.owner, name, id, &args).await;
        assert_eq!(
            (r.status, code(&r).as_str()),
            (status, want),
            "{name} {args}"
        );
        assert!(rig.srv.receipt(id).is_none(), "{want}: no receipt");
        assert_eq!(host_head(&rig), head, "{want}");
        assert_eq!(rig.kinds(t).len(), thread_head, "{want}");
        assert_eq!(rig.pty.spawns().len(), spawned, "{want}");
    }
    // Accepted: one argument after `--`.
    let (_t, _l) = rig.spawn_codex(Some("two words\nand a line")).await;
    let spawns = rig.pty.spawns();
    let argv = &spawns.last().unwrap().argv;
    let n = argv.len();
    assert_eq!(argv[n - 2], "--");
    assert_eq!(argv[n - 1], "two words\nand a line");
    rig.srv.stop().await;
}

/// PR10a1-CL-1: a `cwd` that is a file, missing, or relative (even naming an existing directory)
/// is 400 `bad_args` from the op, with a receipt holding that reply and nothing else written;
/// the same `command_id` replays it after the directory appears.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_cwd() {
    let rig = Rig::new().await;
    let file = rig.cwd.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    let missing = rig.cwd.path().join("missing");
    // A relative path naming an existing directory (the daemon's own working directory).
    let relative = std::path::PathBuf::from(".");
    for path in [&file, &missing, &relative] {
        let head = host_head(&rig);
        let id = cid();
        let args = json!({ "agent": "codex", "cwd": path.to_string_lossy() });
        let r = rig.cmd_as(&rig.owner, "spawn", id, &args).await;
        assert_eq!((r.status, code(&r).as_str()), (400, "bad_args"));
        assert!(rig.srv.receipt(id).is_some(), "the reply is stored");
        assert_eq!(host_head(&rig), head, "no thread_registered");
        assert!(rig.pty.spawns().is_empty());
        let threads = rig.srv.store.read(|r| Ok(r.threads()?.len()));
        assert_eq!(threads, 0);
        if path == &missing {
            std::fs::create_dir(&missing).unwrap();
            let again = rig.cmd_as(&rig.owner, "spawn", id, &args).await;
            assert_eq!(again.status, 400);
            assert_eq!(again.body, r.body, "the stored reply, byte for byte");
            assert!(rig.pty.spawns().is_empty());
        }
    }
    rig.srv.stop().await;
}

/// PR10a1-CL-21: the launch codes at their statuses, from real refusals (the full table is
/// `http::tests::commands`'s `codes_and_statuses` of part 1; `launch_busy`,
/// `worktree_unavailable`, and `no_session` have their own tests).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codes_and_statuses() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let other = LaunchId(uuid::Uuid::new_v4());
    let check = |r: Reply, status: u16, want: &str| {
        assert_eq!((r.status, code(&r).as_str()), (status, want));
        assert!(r.header("retry-after").is_none());
    };
    check(
        rig.cmd(
            "resume",
            &json!({ "thread_id": t, "expected_launch_id": l }),
        )
        .await,
        409,
        "launch_live",
    );
    check(
        rig.cmd("close", &json!({ "thread_id": t, "expected_launch_id": l }))
            .await,
        409,
        "launch_live",
    );
    check(
        rig.cmd(
            "close",
            &json!({ "thread_id": t, "expected_launch_id": other }),
        )
        .await,
        409,
        "launch_changed",
    );
    check(
        rig.cmd(
            "stop",
            &json!({ "thread_id": t, "expected_launch_id": other }),
        )
        .await,
        409,
        "launch_changed",
    );
    let unknown = ThreadId(uuid::Uuid::new_v4());
    check(
        rig.cmd("stop", &json!({ "thread_id": unknown })).await,
        404,
        "not_found",
    );
    // Unconfirmed: stop, resume, and close are `death_unconfirmed`.
    let (u, ul) = unconfirmed(&rig).await;
    check(
        rig.cmd("stop", &json!({ "thread_id": u })).await,
        409,
        "death_unconfirmed",
    );
    check(
        rig.cmd(
            "resume",
            &json!({ "thread_id": u, "expected_launch_id": ul }),
        )
        .await,
        409,
        "death_unconfirmed",
    );
    check(
        rig.cmd(
            "close",
            &json!({ "thread_id": u, "expected_launch_id": ul }),
        )
        .await,
        409,
        "death_unconfirmed",
    );
    // Closed.
    rig.cmd("stop", &json!({ "thread_id": t })).await;
    let r = rig
        .cmd("close", &json!({ "thread_id": t, "expected_launch_id": l }))
        .await;
    assert_eq!(r.status, 200);
    check(
        rig.cmd("close", &json!({ "thread_id": t, "expected_launch_id": l }))
            .await,
        409,
        "thread_closed",
    );
    rig.srv.stop().await;
}

/// PR10a1-CL-21, CL-39: no success body has a top-level `code`, in the schemas and in the
/// replies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn success_schemas_have_no_code() {
    // The exported schemas (`packages/contracts/schema`).
    for name in ["StopResult", "CloseResult"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/contracts/schema")
            .join(format!("{name}.schema.json"));
        let schema: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(schema["properties"].is_object(), "{name}");
        assert!(schema["properties"].get("code").is_none(), "{name}");
    }
    let rig = Rig::new().await;
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let r = rig
        .cmd("spawn", &json!({ "agent": "codex", "cwd": cwd }))
        .await;
    let v = json(&r);
    let (t, l) = (v["thread_id"].clone(), v["launch_id"].clone());
    let stop = rig.cmd("stop", &json!({ "thread_id": t })).await;
    let close = rig
        .cmd(
            "close",
            &json!({ "thread_id": t, "expected_launch_id": l, "stop": true }),
        )
        .await;
    let (t2, l2) = rig.spawn_codex(None).await;
    rig.cmd("stop", &json!({ "thread_id": t2 })).await;
    let resume = rig
        .cmd(
            "resume",
            &json!({ "thread_id": t2, "expected_launch_id": l2 }),
        )
        .await;
    for reply in [&r, &stop, &close, &resume] {
        assert!(
            reply.status == 200 || reply.status == 409,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        if reply.status == 200 {
            assert!(
                json(reply).get("code").is_none(),
                "{}",
                String::from_utf8_lossy(&reply.body)
            );
        }
    }
    rig.srv.stop().await;
}

/// PR10a1-CL-5: two requests with one `command_id`, released together, cause one effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicates_write_once() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    rig.bind(t, l, "s1").await;
    let first = rig.pty.take_child().unwrap();
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let twice = |name: &'static str, args: serde_json::Value| {
        let rig = &rig;
        async move {
            let id = cid();
            let release = rig.srv.hold_writer().await;
            let req = rig.srv.command(name, &rig.owner, id, &args.to_string());
            let a = spawn_send(&rig.srv, &req);
            let b = spawn_send(&rig.srv, &req);
            drop(release);
            let (a, b) = (a.await.unwrap(), b.await.unwrap());
            for r in [&a, &b] {
                assert!(
                    r.status == 200 || (r.status == 409 && code(r) == "in_progress"),
                    "{name}: {} {}",
                    r.status,
                    String::from_utf8_lossy(&r.body)
                );
            }
            assert!(a.status == 200 || b.status == 200, "{name}");
        }
    };
    // spawn: one reservation, one spawn.
    let before = rig.pty.spawns().len();
    twice("spawn", json!({ "agent": "codex", "cwd": cwd })).await;
    assert_eq!(rig.pty.spawns().len(), before + 1);
    // stop: one `StopRequested` (one `launch_exited`), one kill.
    twice("stop", json!({ "thread_id": t })).await;
    let exits = rig
        .kinds(t)
        .iter()
        .filter(|k| *k == "launch_exited")
        .count();
    assert_eq!(exits, 1);
    let kills = first
        .log()
        .iter()
        .filter(|e| matches!(e, FakeEvent::Kill { .. }))
        .count();
    assert_eq!(kills, 1);
    // resume: one reservation.
    let before = rig.pty.spawns().len();
    twice("resume", json!({ "thread_id": t, "expected_launch_id": l })).await;
    assert_eq!(rig.pty.spawns().len(), before + 1);
    let reserved = rig
        .kinds(t)
        .iter()
        .filter(|k| *k == "launch_reserved")
        .count();
    assert_eq!(reserved, 2, "the spawn's and one resume's");
    // close --stop: one `thread_closed`.
    let l2 = rig.thread(t).current_launch_id.unwrap();
    twice(
        "close",
        json!({ "thread_id": t, "expected_launch_id": l2, "stop": true }),
    )
    .await;
    let closed = rig.srv.store.read(move |r| {
        Ok(r.events_after(StreamId::Host, 0, 10_000)?
            .events
            .into_iter()
            .filter(|e| serde_json::to_value(&e.kind).unwrap() == "thread_closed")
            .count())
    });
    assert_eq!(closed, 1);
    rig.srv.stop().await;
}

/// PR10a1-CL-34: an agent token spawns a child of its own thread, role `standard`; an
/// owner's spawn has no parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_caller() {
    let rig = Rig::new().await;
    let (parent, pl) = rig.spawn_codex(None).await;
    let agent = rig
        .srv
        .token(TokenKind::Agent, Some(parent), Some(pl))
        .await;
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let r = rig
        .cmd_as(
            &agent,
            "spawn",
            cid(),
            &json!({ "agent": "codex", "cwd": cwd }),
        )
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let child: ThreadId = serde_json::from_value(json(&r)["thread_id"].clone()).unwrap();
    let registered = |t: ThreadId| {
        rig.srv.store.read(move |r| {
            Ok(r.events_after(StreamId::Host, 0, 10_000)?
                .events
                .into_iter()
                .map(|e| serde_json::from_str::<serde_json::Value>(e.payload.get()).unwrap())
                .find(|p| p["thread_id"] == json!(t) && p.get("role").is_some())
                .unwrap())
        })
    };
    let c = registered(child);
    assert_eq!(c["parent_thread_id"], json!(parent));
    assert_eq!(c["role"], "standard");
    let p = registered(parent);
    assert_eq!(p["parent_thread_id"], serde_json::Value::Null);
    assert_eq!(p["role"], "standard");
    rig.srv.stop().await;
}

/// PR10a1-CL-35: an agent token acts on its own thread; against an unrelated thread each of
/// `stop`, `close`, `close --stop`, and `resume` is 404 before any read of its launch (the
/// store's reads of that launch's row, and of a stale `expected_launch_id`'s, counted: none),
/// with no receipt, no kill, and the thread unchanged; also with a stale
/// `expected_launch_id`, which a generation read before the scope check would answer 409.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn target_scope() {
    let rig = Rig::new().await;
    let (other, ol) = rig.spawn_codex(None).await;
    let other_child = rig.pty.take_child().unwrap();
    let (own, own_l) = rig.spawn_codex(None).await;
    let agent = rig
        .srv
        .token(TokenKind::Agent, Some(own), Some(own_l))
        .await;
    let head = rig.kinds(other).len();
    let stale = LaunchId(uuid::Uuid::new_v4());
    use crate::store::launch_reads;
    launch_reads::watch(ol);
    launch_reads::watch(stale);
    for (name, args) in [
        ("stop", json!({ "thread_id": other })),
        (
            "close",
            json!({ "thread_id": other, "expected_launch_id": ol }),
        ),
        (
            "close",
            json!({ "thread_id": other, "expected_launch_id": ol, "stop": true }),
        ),
        (
            "resume",
            json!({ "thread_id": other, "expected_launch_id": ol }),
        ),
        // A stale `expected_launch_id`: a generation read before the scope check would
        // answer 409 `launch_changed` and reveal the launch.
        (
            "stop",
            json!({ "thread_id": other, "expected_launch_id": stale }),
        ),
        (
            "close",
            json!({ "thread_id": other, "expected_launch_id": stale, "stop": true }),
        ),
        (
            "resume",
            json!({ "thread_id": other, "expected_launch_id": stale }),
        ),
    ] {
        let id = cid();
        let r = rig.cmd_as(&agent, name, id, &args).await;
        assert_eq!(
            (r.status, code(&r).as_str()),
            (404, "not_found"),
            "{name} {args}"
        );
        assert!(rig.srv.receipt(id).is_none(), "{name}");
        assert_eq!(
            (launch_reads::count(ol), launch_reads::count(stale)),
            (0, 0),
            "{name} {args}: a launch read before the scope check"
        );
    }
    assert!(
        !other_child
            .log()
            .iter()
            .any(|e| matches!(e, FakeEvent::Kill { .. })),
        "no kill of the unrelated thread"
    );
    assert_eq!(rig.kinds(other).len(), head);
    // Its own thread: the stop succeeds.
    let r = rig
        .cmd_as(&agent, "stop", cid(), &json!({ "thread_id": own }))
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    rig.srv.stop().await;
}
