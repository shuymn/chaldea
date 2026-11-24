//! Commands and receipts over HTTP (`docs/design/http-surface.md` §2.8; PR7-CL-18, CL-19,
//! CL-20, CL-21, and the 7c cases of CL-8 and CL-22), through the test-only handlers of
//! `support::commands`. Cases that need the server's fault points are the unit tests
//! `http::tests::commands` and `http::tests::registry`.

mod support;

use std::time::Duration;

use chaldea_core::auth::TokenKind;
use chaldea_core::command::{CommandResult, ReceiptStatus};
use chaldea_core::event::TypedEvent;
use chaldea_core::ids::{Actor, CommandId, LaunchId, StreamId, ThreadId, WorkspaceId, WorktreeId};
use chaldea_core::payload::{WorktreeChanged, WorktreeState};
use chaldea_daemon::http::Mounts;
use chaldea_daemon::store::TokenSelector;
use support::commands::{completed_body, slow_body};
use support::http::{
    Reply, TestServer, eventually, host_events_of, reached, reached_sync, settled, spawn_send,
};
use support::waits;
use support::{event, registered_under, reserved, soon, started};

fn cid(n: u128) -> CommandId {
    CommandId::from_u128(0xC0DE_0000 + n)
}

fn code(s: &str) -> Option<String> {
    Some(s.to_owned())
}

async fn joined(h: tokio::task::JoinHandle<Reply>) -> Reply {
    waits::bounded(Duration::from_secs(20), h)
        .await
        .expect("the reply within 20 s")
        .unwrap()
}

fn completed(r: &Reply) -> CommandResult {
    CommandResult {
        http_status: r.status,
        body: String::from_utf8(r.body.clone()).unwrap(),
    }
}

/// A repeat by its token with the same identity returns the stored status and body byte
/// for byte, also when the arguments are spelled differently; one intent ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_is_byte_equal() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let first = srv
        .send(&srv.command("test_completed", &owner, cid(1), r#"{"text":"hi"}"#))
        .await;
    assert_eq!(first.status, 201, "{first:?}");
    assert_eq!(first.body, completed_body("hi").as_bytes());
    assert_eq!(first.header("content-type"), Some("application/json"));
    let again = srv
        .send(&srv.command("test_completed", &owner, cid(1), "{ \"text\" : \"hi\" }"))
        .await;
    assert_eq!((again.status, &again.body), (first.status, &first.body));
    assert_eq!(
        srv.receipt(cid(1)).unwrap().status,
        ReceiptStatus::Completed(completed(&first))
    );

    let args = r#"{"n":3,"ratio":1,"env":{"b":"2","a":"1"}}"#;
    let respelled = r#"{"env":{"a":"1","b":"2"},"ratio":1.0,"n":3}"#;
    let first = srv
        .send(&srv.command("test_counter", &owner, cid(2), args))
        .await;
    assert_eq!(first.status, 200, "{first:?}");
    let again = srv
        .send(&srv.command("test_counter", &owner, cid(2), respelled))
        .await;
    assert_eq!((again.status, &again.body), (first.status, &first.body));
    assert_eq!(probe.intents(), 1);
    assert_eq!(host_events_of(&srv, cid(2)), 1);

    // A stored non-2xx result replays the same way.
    let refusal = srv.command(
        "test_completed",
        &owner,
        cid(3),
        r#"{"text":"no","status":404,"code":"not_found"}"#,
    );
    let first = srv.send(&refusal).await;
    assert_eq!(first.status, 404, "{first:?}");
    assert_eq!(first.body, br#"{"code":"not_found"}"#);
    let again = srv.send(&refusal).await;
    assert_eq!((again.status, &again.body), (first.status, &first.body));
    srv.stop().await;
}

/// Another token gets one `conflict` body whatever the receipt's state: completed,
/// `in_progress`, or `unknown_after_crash`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_token_conflicts() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let other = srv.token(TokenKind::Owner, None, None).await;
    let owner_id = srv.token_row(&owner).unwrap().token_id;
    // unknown_after_crash, set up through the store before any receipt is in progress.
    let crashed = cid(3);
    srv.store
        .store()
        .submit(soon(), move |tx| {
            tx.receipt_begin(crashed, owner_id, [7; 32])?;
            Ok(tx.receipts_mark_unknown_after_crash()?)
        })
        .map(settled)
        .unwrap()
        .await
        .unwrap();
    // Completed.
    let done = srv
        .send(&srv.command("test_completed", &owner, cid(1), r#"{"text":"a"}"#))
        .await;
    assert_eq!(done.status, 201);
    // In progress: the effect is held.
    let (effect_reached, release) = probe.hold_effect();
    let running = spawn_send(
        &srv,
        &srv.command("test_slow_effect", &owner, cid(2), r#"{"tag":"t"}"#),
    );
    reached(effect_reached).await;
    assert_eq!(
        srv.receipt(cid(2)).unwrap().status,
        ReceiptStatus::InProgress
    );

    let mut bodies = Vec::new();
    for (id, name, args) in [
        (cid(1), "test_completed", r#"{"text":"a"}"#),
        (cid(2), "test_slow_effect", r#"{"tag":"t"}"#),
        (crashed, "test_completed", r#"{"text":"a"}"#),
    ] {
        let r = srv.send(&srv.command(name, &other, id, args)).await;
        assert_eq!(r.outcome(), (409, code("conflict")), "{id}");
        bodies.push((r.body.clone(), r.header("retry-after").map(str::to_owned)));
    }
    assert_eq!(bodies[0], (br#"{"code":"conflict"}"#.to_vec(), None));
    assert!(bodies.iter().all(|b| *b == bodies[0]), "{bodies:?}");
    drop(release);
    assert_eq!(joined(running).await.status, 200);
    assert_eq!(probe.effects(), 1);
    srv.stop().await;
}

/// The same token with another identity (other arguments, or another command name) gets
/// `conflict`, whatever the receipt's state: completed, `in_progress`, or
/// `unknown_after_crash`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_identity_conflicts() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let r = srv
        .send(&srv.command("test_counter", &owner, cid(1), r#"{"n":1}"#))
        .await;
    assert_eq!(r.status, 200);
    for (name, args) in [
        ("test_counter", r#"{"n":2}"#),
        ("test_counter", r#"{"n":1,"label":"x"}"#),
        ("test_completed", r#"{"text":"1"}"#),
    ] {
        let r = srv.send(&srv.command(name, &owner, cid(1), args)).await;
        assert_eq!(r.outcome(), (409, code("conflict")), "{name} {args}");
        assert_eq!(r.body, br#"{"code":"conflict"}"#);
    }
    assert_eq!(probe.intents(), 1);
    // In progress: the effect is held; then marked as at a restart.
    let (effect_reached, release) = probe.hold_effect();
    let running = spawn_send(
        &srv,
        &srv.command("test_slow_effect", &owner, cid(2), r#"{"tag":"i"}"#),
    );
    reached(effect_reached).await;
    let other = srv.command("test_slow_effect", &owner, cid(2), r#"{"tag":"j"}"#);
    let r = srv.send(&other).await;
    assert_eq!(r.outcome(), (409, code("conflict")), "in progress");
    srv.store
        .store()
        .submit(soon(), |tx| Ok(tx.receipts_mark_unknown_after_crash()?))
        .map(settled)
        .unwrap()
        .await
        .unwrap();
    let r = srv.send(&other).await;
    assert_eq!(r.outcome(), (409, code("conflict")), "unknown after crash");
    assert_eq!(r.body, br#"{"code":"conflict"}"#);
    drop(release);
    joined(running).await;
    assert_eq!(probe.effects(), 1);
    srv.stop().await;
}

/// A retry while the effect runs gets 409 `in_progress` with `retry_after`; after the
/// effect, the stored result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_progress_while_effect_runs() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let req = srv.command("test_slow_effect", &owner, cid(1), r#"{"tag":"t"}"#);
    let (effect_reached, release) = probe.hold_effect();
    let first = spawn_send(&srv, &req);
    reached(effect_reached).await;
    let retry = srv.send(&req).await;
    assert_eq!(retry.outcome(), (409, code("in_progress")));
    assert_eq!(retry.body, br#"{"code":"in_progress","retry_after":1}"#);
    release.send(()).unwrap();
    let first = joined(first).await;
    assert_eq!(
        (first.status, first.body.as_slice()),
        (200, slow_body("t").as_bytes())
    );
    let replay = srv.send(&req).await;
    assert_eq!((replay.status, &replay.body), (200, &first.body));
    assert_eq!((probe.intents(), probe.effects()), (1, 1));
    srv.stop().await;
}

/// An effect held well past the request deadline: the request and every retry get 409
/// `in_progress` (the receipt stays `in_progress`); after the release the receipt
/// completes and a retry replays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effect_holds_receipt_in_progress() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let req = srv.command("test_slow_effect", &owner, cid(1), r#"{"tag":"slow"}"#);
    let (effect_reached, release) = probe.hold_effect();
    let first = spawn_send(&srv, &req);
    reached(effect_reached).await;
    // The first reply comes at its reply time, while the effect is still held.
    assert_eq!(joined(first).await.outcome(), (409, code("in_progress")));
    for _ in 0..2 {
        assert_eq!(srv.send(&req).await.outcome(), (409, code("in_progress")));
    }
    assert_eq!(
        srv.receipt(cid(1)).unwrap().status,
        ReceiptStatus::InProgress
    );
    release.send(()).unwrap();
    eventually(Duration::from_secs(10), "the receipt completes", || {
        srv.receipt(cid(1)).unwrap().status != ReceiptStatus::InProgress
    })
    .await;
    let replay = srv.send(&req).await;
    assert_eq!(
        (replay.status, replay.body.as_slice()),
        (200, slow_body("slow").as_bytes())
    );
    assert_eq!(probe.effects(), 1);
    assert!(srv.sink.0.lock().unwrap().is_empty());
    srv.stop().await;
}

/// A receipt `unknown_after_crash` answers 409 `unknown_after_crash` and nothing runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_after_crash() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let req = srv.command("test_slow_effect", &owner, cid(1), r#"{"tag":"u"}"#);
    // The receipt the request itself leaves `in_progress`, then marked as at a restart.
    let (effect_reached, release) = probe.hold_effect();
    let first = spawn_send(&srv, &req);
    reached(effect_reached).await;
    srv.store
        .store()
        .submit(soon(), |tx| Ok(tx.receipts_mark_unknown_after_crash()?))
        .map(settled)
        .unwrap()
        .await
        .unwrap();
    let r = srv.send(&req).await;
    assert_eq!(r.outcome(), (409, code("unknown_after_crash")));
    assert_eq!(r.body, br#"{"code":"unknown_after_crash"}"#);
    drop(release);
    joined(first).await;
    assert_eq!(probe.effects(), 1);
    srv.stop().await;
}

/// After a restart: a receipt completed before it replays, one `in_progress` at the
/// restart is `unknown_after_crash`, and a new `command_id` runs (PR7-CL-20).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_after_restart_outcomes() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let done = srv.command("test_completed", &owner, cid(1), r#"{"text":"r"}"#);
    let first = srv.send(&done).await;
    assert_eq!(first.status, 201);
    // An effect still running at the drain deadline: its runner gets an abort request.
    let (effect_reached, _release) = probe.hold_effect();
    let (port, bytes) = (
        srv.port,
        srv.command("test_slow_effect", &owner, cid(2), r#"{"tag":"x"}"#)
            .bytes(),
    );
    let running = tokio::spawn(async move {
        let mut s = support::http::connect(port).await;
        support::http::write(&mut s, &bytes).await;
        support::http::read_to_end(&mut s).await
    });
    reached(effect_reached).await;
    let (report, mut store) = srv.into_store(Duration::from_millis(200)).await;
    assert_eq!(report.abort_requested.runner, 1, "{report:?}");
    // Its connection ended at shutdown without a reply.
    let (bytes, _end) = waits::bounded(Duration::from_secs(20), running)
        .await
        .expect("the client ends within 20 s")
        .unwrap();
    assert!(bytes.is_empty(), "{:?}", String::from_utf8_lossy(&bytes));
    // The restart: the store reopens and marks what was in progress.
    let store = tokio::task::spawn_blocking(move || {
        store.reopen();
        store
            .run(|tx| Ok(tx.receipts_mark_unknown_after_crash()?))
            .unwrap();
        store
    })
    .await
    .unwrap();
    let (srv, probe2) = TestServer::with_commands_on(store, |_| {}).await;
    let again = srv
        .send(&srv.command("test_completed", &owner, cid(1), r#"{"text":"r"}"#))
        .await;
    assert_eq!((again.status, &again.body), (first.status, &first.body));
    let r = srv
        .send(&srv.command("test_slow_effect", &owner, cid(2), r#"{"tag":"x"}"#))
        .await;
    assert_eq!(r.outcome(), (409, code("unknown_after_crash")));
    let new = srv
        .send(&srv.command("test_completed", &owner, cid(3), r#"{"text":"n"}"#))
        .await;
    assert_eq!(new.status, 201);
    assert_eq!((probe.effects(), probe2.effects()), (1, 0));
    srv.stop().await;
}

/// A duplicate member at any depth is 400 `bad_args` with no receipt and nothing run,
/// also where the handler's own parser would keep the last value (a map field).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_member_is_400_without_receipt() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let cases = [
        r#"{"n":1,"n":2}"#,
        r#"{"n":1,"env":{"a":"x","a":"y"}}"#,
        r#"{"n":1,"layers":[{"k":1},{"k":1,"k":2}]}"#,
        // Equal only after JSON unescaping.
        r#"{"n":1,"env":{"a":"x","\u0061":"y"}}"#,
    ];
    for (i, args) in cases.iter().enumerate() {
        let id = cid(i as u128);
        let r = srv
            .send(&srv.command("test_counter", &owner, id, args))
            .await;
        assert_eq!(r.outcome(), (400, code("bad_args")), "{args}");
        assert_eq!(srv.receipt(id), None, "{args}");
    }
    assert_eq!(probe.intents(), 0);
    // The positive control: distinct members run.
    let r = srv
        .send(&srv.command(
            "test_counter",
            &owner,
            cid(9),
            r#"{"n":1,"env":{"a":"x","b":"y"}}"#,
        ))
        .await;
    assert_eq!(r.status, 200);
    srv.stop().await;
}

/// `args` that is not an object, or an envelope that does not decode or is not an object,
/// is 400 `bad_request` with no receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn args_must_be_an_object() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    for (i, args) in ["[]", "1", r#""x""#, "null", "[{\"n\":1}]"]
        .iter()
        .enumerate()
    {
        let id = cid(i as u128);
        let r = srv
            .send(&srv.command("test_counter", &owner, id, args))
            .await;
        assert_eq!(r.outcome(), (400, code("bad_request")), "{args}");
        assert_eq!(srv.receipt(id), None);
    }
    let path = "/api/commands/test_counter";
    for body in [
        format!(r#"{{"command_id":"{}"}}"#, cid(7)),
        r#"{"command_id":"not-a-uuid","args":{"n":1}}"#.to_owned(),
        format!(r#"{{"command_id":"{}","args":{{"n":1}}"#, cid(8)),
        String::new(),
        // The envelope's fields as an array.
        format!(r#"["{}",{{"n":1}}]"#, cid(10)),
    ] {
        let r = srv
            .send(&srv.req("POST", path).bearer(&owner).body(body.as_bytes()))
            .await;
        assert_eq!(r.outcome(), (400, code("bad_request")), "{body}");
    }
    assert_eq!(srv.receipt(cid(10)), None);
    assert_eq!(probe.intents(), 0);
    srv.stop().await;
}

/// A registered command without a handler is 501 with no receipt, under the test table and
/// under the daemon's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unimplemented_is_501_without_receipt() {
    let (srv, _probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let r = srv
        .send(&srv.command("send", &owner, cid(1), r#"{"text":"x"}"#))
        .await;
    assert_eq!(r.outcome(), (501, code("not_implemented")));
    assert_eq!(srv.receipt(cid(1)), None);
    srv.stop().await;

    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    for name in ["test_counter", "test_completed"] {
        let r = srv.send(&srv.command(name, &owner, cid(2), "{}")).await;
        assert_eq!(r.outcome(), (404, code("not_found")), "{name}");
    }
    let r = srv.send(&srv.command("spawn", &owner, cid(2), "{}")).await;
    assert_eq!(r.outcome(), (501, code("not_implemented")));
    assert_eq!(srv.receipt(cid(2)), None);
    srv.stop().await;
}

/// A token revoked after the guard's check and before the intent job runs: 401, nothing
/// written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_between_guard_and_job() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let id = srv.token_row(&owner).unwrap().token_id;
    let writer = srv.hold_writer().await;
    // The revocation is queued first; the guard's read does not see it.
    let revoke = srv
        .store
        .store()
        .submit(soon(), move |tx| {
            Ok(tx.token_revoke(TokenSelector::Token(id))?)
        })
        .unwrap();
    let base = srv.server.tasks();
    let r = spawn_send(
        &srv,
        &srv.command("test_counter", &owner, cid(1), r#"{"n":1}"#),
    );
    // Its connection and its runner are registered: the guard admitted it.
    eventually(Duration::from_secs(10), "the runner is registered", || {
        srv.server.tasks() >= base + 2
    })
    .await;
    writer.send(()).unwrap();
    assert_eq!(settled(revoke).await.unwrap(), 1);
    assert_eq!(joined(r).await.outcome(), (401, code("unauthorized")));
    assert_eq!(srv.receipt(cid(1)), None);
    assert_eq!((probe.intents(), host_events_of(&srv, cid(1))), (0, 0));
    srv.stop().await;
}

fn worktree(n: u8, owner: ThreadId) -> TypedEvent {
    TypedEvent::WorktreeChanged(WorktreeChanged {
        worktree_id: WorktreeId(format!("{n:08x}")),
        repo: "/work/repo".into(),
        workspace_id: WorkspaceId("ws".into()),
        path: format!("/work/wt{n}"),
        branch: format!("chaldea/{n}"),
        base_ref: "main".into(),
        base_commit: "0".repeat(40),
        owner_thread_id: Some(owner),
        created_by: Actor::Human,
        state: WorktreeState::Active,
    })
}

/// Object checks run in the intent job before anything is written: an object the token
/// cannot read is 404, one it can read but not act on 403; no receipt, nothing run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn object_denials() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let t = |n: u128| ThreadId::from_u128(0xE000 + n);
    let (me, child, grandchild, sibling) = (t(1), t(2), t(3), t(4));
    srv.append(vec![
        registered_under(me, None),
        registered_under(child, Some(me)),
        registered_under(grandchild, Some(child)),
        registered_under(sibling, None),
    ])
    .await;
    let l = LaunchId::from_u128(0xE100);
    srv.append(vec![reserved(me, l), started(me, l)]).await;
    let agent = srv.token(TokenKind::Agent, Some(me), Some(l)).await;
    srv.append(vec![
        event(StreamId::Host, "wt:me", worktree(1, me)),
        event(StreamId::Host, "wt:child", worktree(2, child)),
        event(StreamId::Host, "wt:sibling", worktree(3, sibling)),
    ])
    .await;
    let target = |th: ThreadId| format!(r#"{{"n":1,"target":"{th}"}}"#);
    let wt = |n: u8| format!(r#"{{"worktree_id":"{n:08x}"}}"#);
    let refused = [
        ("test_counter", target(sibling), (404, code("not_found"))),
        ("test_counter", target(t(9)), (404, code("not_found"))),
        // Readable (self+) but not a direct child: `task set` refuses.
        ("test_counter", target(grandchild), (403, code("forbidden"))),
        ("test_worktree_remove", wt(2), (403, code("forbidden"))),
        ("test_worktree_remove", wt(3), (404, code("not_found"))),
        ("test_worktree_remove", wt(8), (404, code("not_found"))),
    ];
    for (i, (name, args, want)) in refused.iter().enumerate() {
        let id = cid(i as u128);
        let r = srv.send(&srv.command(name, &agent, id, args)).await;
        assert_eq!(&r.outcome(), want, "{name} {args}");
        assert_eq!(srv.receipt(id), None, "{name} {args}");
        assert_eq!(host_events_of(&srv, id), 0);
    }
    assert_eq!(probe.intents(), 0);
    // Positive controls: a direct child, and the agent's own worktree.
    let r = srv
        .send(&srv.command("test_counter", &agent, cid(20), &target(child)))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = srv
        .send(&srv.command("test_worktree_remove", &agent, cid(21), &wt(1)))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    srv.stop().await;
}

/// The effect revokes the caller's token: the runner still completes the receipt (no
/// token check after the effect), and the request gets the result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_during_effect() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let req = srv.command(
        "test_slow_effect",
        &owner,
        cid(1),
        r#"{"tag":"r","revoke_self":true}"#,
    );
    let r = srv.send(&req).await;
    assert_eq!(
        (r.status, r.body.as_slice()),
        (200, slow_body("r").as_bytes())
    );
    assert!(srv.token_row(&owner).unwrap().revoked_at.is_some());
    assert_eq!(
        srv.receipt(cid(1)).unwrap().status,
        ReceiptStatus::Completed(completed(&r))
    );
    assert_eq!(srv.send(&req).await.outcome(), (401, code("unauthorized")));
    assert_eq!(probe.effects(), 1);
    assert!(srv.sink.0.lock().unwrap().is_empty());
    srv.stop().await;
}

/// The intent job runs before its deadline but commits only after the reply time: the
/// request gets 503 `outcome_unknown`; the runner then runs the effect, and a retry
/// replays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_successful_intent() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let req = srv.command("test_slow_effect", &owner, cid(1), r#"{"tag":"late"}"#);
    let (intent_reached, release) = probe.hold_intent();
    let first = spawn_send(&srv, &req);
    reached_sync(intent_reached).await;
    let first = joined(first).await;
    assert_eq!(first.outcome(), (503, code("outcome_unknown")));
    assert_eq!(first.body, br#"{"code":"outcome_unknown","retry_after":1}"#);
    release.send(()).unwrap();
    eventually(Duration::from_secs(10), "the receipt completes", || {
        srv.receipt(cid(1))
            .is_some_and(|r| r.status != ReceiptStatus::InProgress)
    })
    .await;
    let replay = srv.send(&req).await;
    assert_eq!(
        (replay.status, replay.body.as_slice()),
        (200, slow_body("late").as_bytes())
    );
    assert_eq!((probe.intents(), probe.effects()), (1, 1));
    srv.stop().await;
}

/// With the writer occupied well past `deadline + write_reply_grace`, the reply is 503
/// `outcome_unknown`, never 408; the intent job then expires, leaving no receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writer_held_is_outcome_unknown() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let writer = srv.hold_writer().await;
    let r = srv
        .send(&srv.command("test_counter", &owner, cid(1), r#"{"n":1}"#))
        .await;
    assert_eq!(r.outcome(), (503, code("outcome_unknown")));
    assert_eq!(r.body, br#"{"code":"outcome_unknown","retry_after":1}"#);
    writer.send(()).unwrap();
    // A job queued after the intent job runs after it.
    srv.store
        .store()
        .submit(soon(), |_tx| Ok(()))
        .map(settled)
        .unwrap()
        .await
        .unwrap();
    assert_eq!(srv.receipt(cid(1)), None);
    assert_eq!(probe.intents(), 0);
    srv.stop().await;
}

/// The client goes away while the intent job is queued: the runner still commits the
/// intent, runs the effect once, and completes the receipt; a retry replays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_before_outcome() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let req = srv.command("test_slow_effect", &owner, cid(1), r#"{"tag":"d"}"#);
    let writer = srv.hold_writer().await;
    let base = srv.server.tasks();
    let mut conn = support::http::connect(srv.port).await;
    support::http::write(&mut conn, &req.bytes()).await;
    eventually(Duration::from_secs(10), "the runner is registered", || {
        srv.server.tasks() >= base + 2
    })
    .await;
    drop(conn);
    writer.send(()).unwrap();
    eventually(Duration::from_secs(10), "the receipt completes", || {
        srv.receipt(cid(1))
            .is_some_and(|r| r.status != ReceiptStatus::InProgress)
    })
    .await;
    let replay = srv.send(&req).await;
    assert_eq!(
        (replay.status, replay.body.as_slice()),
        (200, slow_body("d").as_bytes())
    );
    assert_eq!(probe.effects(), 1);
    srv.stop().await;
}

/// The body of 500 `internal`.
const INTERNAL: &[u8] = br#"{"code":"internal"}"#;

/// An effect that returns 408 after its intent committed: the result is stored and answered
/// as 500 `internal`, on the first reply and on the replay; the effect ran once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_effect_cannot_answer_408() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let req = srv.command(
        "test_slow_effect",
        &owner,
        cid(1),
        r#"{"tag":"effect_408"}"#,
    );
    let first = srv.send(&req).await;
    let replay = srv.send(&req).await;
    assert_eq!(probe.effects(), 1);
    assert_eq!(
        (first.status, first.body.as_slice()),
        (500, INTERNAL),
        "{:?}",
        srv.receipt(cid(1))
    );
    assert_eq!((replay.status, &replay.body), (first.status, &first.body));
    assert_eq!(
        srv.receipt(cid(1)).unwrap().status,
        ReceiptStatus::Completed(completed(&first))
    );
    srv.stop().await;
}

/// A completed intent's or an effect's status outside the result set (408, 503, 302, 42,
/// and 204 and 205, which carry no body) is stored and answered as 500 `internal`, and
/// replays as such; a refusal outside the refusal set (408, 200) is 500 `internal` with no
/// receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statuses_outside_the_sets_are_500() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let mut stored = Vec::new();
    for (i, status) in [408, 503, 302, 42, 204, 205].into_iter().enumerate() {
        let args = format!(r#"{{"text":"s","status":{status}}}"#);
        stored.push((cid(i as u128), "test_completed", args));
    }
    stored.push((
        cid(9),
        "test_slow_effect",
        r#"{"tag":"effect_42"}"#.to_owned(),
    ));
    for (id, name, args) in &stored {
        let req = srv.command(name, &owner, *id, args);
        let first = srv.send(&req).await;
        assert_eq!(
            (first.status, first.body.as_slice()),
            (500, INTERNAL),
            "{args}"
        );
        assert_eq!(
            srv.receipt(*id).unwrap().status,
            ReceiptStatus::Completed(completed(&first)),
            "{args}"
        );
        let replay = srv.send(&req).await;
        assert_eq!(
            (replay.status, &replay.body),
            (first.status, &first.body),
            "{args}"
        );
    }
    assert_eq!(probe.effects(), 1);
    for (i, how) in ["timeout", "success"].into_iter().enumerate() {
        let id = cid(20 + i as u128);
        let args = format!(r#"{{"how":"{how}"}}"#);
        let r = srv
            .send(&srv.command("test_refuse", &owner, id, &args))
            .await;
        assert_eq!((r.status, r.body.as_slice()), (500, INTERNAL), "{how}");
        assert_eq!(srv.receipt(id), None, "{how}");
    }
    srv.stop().await;
}

/// An effect that panics: 503 `outcome_unknown`, `ReceiptUncompleted`, the receipt stays
/// `in_progress` and a retry gets 409 `in_progress`; after the restart marks it, 409
/// `unknown_after_crash`. The effect ran once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_effect_is_reconciled() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let id = cid(1);
    let args = r#"{"tag":"effect_panic"}"#;
    let req = srv.command("test_slow_effect", &owner, id, args);
    assert_eq!(
        srv.send(&req).await.outcome(),
        (503, code("outcome_unknown"))
    );
    assert_eq!(srv.receipt(id).unwrap().status, ReceiptStatus::InProgress);
    assert_eq!(srv.send(&req).await.outcome(), (409, code("in_progress")));
    assert_eq!((probe.intents(), probe.effects()), (1, 1));
    assert_eq!(
        *srv.sink.0.lock().unwrap(),
        [chaldea_daemon::http::StoreFatal::ReceiptUncompleted { command_id: id }]
    );
    // The restart: the store reopens and marks what was in progress.
    let (_, mut store) = srv.into_store(Duration::from_secs(10)).await;
    let reopened = tokio::task::spawn_blocking(move || {
        store.reopen();
        store
    });
    let store = waits::bounded(Duration::from_secs(20), reopened)
        .await
        .expect("the store reopens within 20 s")
        .unwrap();
    let marked = store
        .store()
        .submit(soon(), |tx| Ok(tx.receipts_mark_unknown_after_crash()?))
        .map(settled)
        .unwrap()
        .await
        .unwrap();
    assert_eq!(marked, 1);
    let (srv, probe2) = TestServer::with_commands_on(store, |_| {}).await;
    let r = srv
        .send(&srv.command("test_slow_effect", &owner, id, args))
        .await;
    assert_eq!(r.outcome(), (409, code("unknown_after_crash")));
    assert_eq!(
        (probe.effects(), probe2.intents(), probe2.effects()),
        (1, 0, 0)
    );
    srv.stop().await;
}

/// Replies reserved to the runner and the request path (408, and the codes
/// `request_timeout`, `outcome_unknown`, `in_progress`, `unknown_after_crash`, `conflict`)
/// are 500 `internal` from a handler: a `prepare` error and an intent's refusal with no
/// receipt, a completed result stored as such and replayed. A transient refusal whose
/// outcome is known (503 `busy`) is answered as is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reserved_handler_replies_are_500() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    for (id, name, args) in [
        (cid(1), "test_completed", r#"{"text":"prepare_timeout"}"#),
        (cid(2), "test_refuse", r#"{"how":"outcome_unknown"}"#),
    ] {
        let r = srv.send(&srv.command(name, &owner, id, args)).await;
        assert_eq!(srv.receipt(id), None, "{name} {args}");
        assert_eq!(
            (r.status, r.body.as_slice()),
            (500, INTERNAL),
            "{name} {args}"
        );
    }
    for (i, code) in [
        "in_progress",
        "outcome_unknown",
        "request_timeout",
        "conflict",
        "unknown_after_crash",
    ]
    .into_iter()
    .enumerate()
    {
        let id = cid(10 + i as u128);
        let args = format!(r#"{{"text":"r","status":409,"code":"{code}"}}"#);
        let req = srv.command("test_completed", &owner, id, &args);
        let first = srv.send(&req).await;
        assert_eq!(
            (first.status, first.body.as_slice()),
            (500, INTERNAL),
            "{code}"
        );
        assert_eq!(
            srv.receipt(id).unwrap().status,
            ReceiptStatus::Completed(completed(&first)),
            "{code}"
        );
        let replay = srv.send(&req).await;
        assert_eq!(
            (replay.status, &replay.body),
            (first.status, &first.body),
            "{code}"
        );
    }
    // A code that is not an `ErrorCode` (an owning PR adds its codes there first).
    let r = srv
        .send(&srv.command(
            "test_completed",
            &owner,
            cid(20),
            r#"{"text":"r","status":409,"code":"card_closed"}"#,
        ))
        .await;
    assert_eq!((r.status, r.body.as_slice()), (500, INTERNAL));
    let r = srv
        .send(&srv.command("test_refuse", &owner, cid(21), r#"{"how":"busy"}"#))
        .await;
    assert_eq!(r.outcome(), (503, code("busy")));
    assert_eq!(srv.receipt(cid(21)), None);
    assert_eq!(probe.intents(), 0);
    srv.stop().await;
}

/// Every `ErrorCode` with the status it is sent with, written out from §2.9.
const CODES: [(&str, u16); 22] = [
    ("misdirected_request", 421),
    ("origin_rejected", 403),
    ("unauthorized", 401),
    ("forbidden", 403),
    ("not_found", 404),
    ("method_not_allowed", 405),
    ("request_timeout", 408),
    ("length_required", 411),
    ("payload_too_large", 413),
    ("bad_request", 400),
    ("bad_query", 400),
    ("bad_args", 400),
    ("in_progress", 409),
    ("conflict", 409),
    ("unknown_after_crash", 409),
    ("not_implemented", 501),
    ("busy", 503),
    ("disk_full", 503),
    ("shutting_down", 503),
    ("store_failed", 503),
    ("outcome_unknown", 503),
    ("internal", 500),
];

/// The codes a handler may reply with (§2.8 "Handler replies"); every other code belongs to
/// the runner, the guard, or the store.
const HANDLER_CODES: [&str; 6] = [
    "bad_args",
    "forbidden",
    "not_found",
    "busy",
    "shutting_down",
    "internal",
];

/// The handler codes whose reply carries `retry_after` (every other one omits it).
const RETRY_CODES: [&str; 1] = ["busy"];

/// The code table over every `ErrorCode`, as a refusal and as a stored result: a code a
/// handler may use, at its status, with `retry_after` exactly when the code calls for it, is
/// answered as it is (a result only with a status of the result set, so not 503); any other
/// reply is 500 `internal`: a refusal with no receipt, a result stored as 500 and replayed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_replies_follow_the_code_table() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let mut n = 100;
    let mut mismatched = Vec::new();
    let mut cases: Vec<(&str, u16, Option<u32>)> =
        CODES.iter().map(|(c, s)| (*c, *s, None)).collect();
    cases.extend([
        // A handler code at another status.
        ("not_found", 400, None),
        ("forbidden", 404, None),
        ("internal", 503, None),
        // `retry_after` where the code calls for it, and where it does not.
        ("busy", 503, Some(1)),
        ("shutting_down", 503, Some(1)),
        ("forbidden", 403, Some(1)),
    ]);
    for (code, status, retry_after) in cases {
        let usable = HANDLER_CODES.contains(&code)
            && CODES.contains(&(code, status))
            && retry_after.is_some() == RETRY_CODES.contains(&code);
        let (body, retry) = match retry_after {
            Some(secs) => (
                format!(r#"{{"code":"{code}","retry_after":{secs}}}"#),
                format!(r#","retry_after":{secs}"#),
            ),
            None => (format!(r#"{{"code":"{code}"}}"#), String::new()),
        };
        let case = format!("{code} {status} {retry_after:?}");
        n += 1;
        let id = cid(n);
        let args = format!(r#"{{"how":"code","code":"{code}","status":{status}{retry}}}"#);
        let r = srv
            .send(&srv.command("test_refuse", &owner, id, &args))
            .await;
        let want = if usable {
            (status, body.as_bytes())
        } else {
            (500, INTERNAL)
        };
        if (r.status, r.body.as_slice()) != want {
            mismatched.push(format!(
                "refusal {case}: got {} {}",
                r.status,
                String::from_utf8_lossy(&r.body)
            ));
        }
        assert_eq!(srv.receipt(id), None, "refusal {case}");
        n += 1;
        let id = cid(n);
        let args = format!(r#"{{"text":"t","status":{status},"code":"{code}"{retry}}}"#);
        let req = srv.command("test_completed", &owner, id, &args);
        let first = srv.send(&req).await;
        let want = if usable && status != 503 {
            (status, body.as_bytes())
        } else {
            (500, INTERNAL)
        };
        if (first.status, first.body.as_slice()) != want {
            mismatched.push(format!(
                "result {case}: got {} {}",
                first.status,
                String::from_utf8_lossy(&first.body)
            ));
        }
        let replay = srv.send(&req).await;
        assert_eq!(
            (replay.status, &replay.body),
            (first.status, &first.body),
            "result {case}"
        );
        assert_eq!(
            srv.receipt(id).unwrap().status,
            ReceiptStatus::Completed(completed(&first)),
            "result {case}"
        );
    }
    assert!(mismatched.is_empty(), "{mismatched:#?}");
    assert_eq!(probe.intents(), 0);
    srv.stop().await;
}

/// A `busy` refusal tells the client when to retry: without `retry_after` it is 500
/// `internal`, with it 503 as it is; no receipt either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_refusal_has_retry_delay() {
    let (srv, _probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let mut n = 0;
    for (retry, want_status, want_body) in [
        (String::new(), 500, INTERNAL.to_vec()),
        (
            r#","retry_after":1"#.to_owned(),
            503,
            br#"{"code":"busy","retry_after":1}"#.to_vec(),
        ),
    ] {
        n += 1;
        let args = format!(r#"{{"how":"code","code":"busy","status":503{retry}}}"#);
        let r = srv
            .send(&srv.command("test_refuse", &owner, cid(n), &args))
            .await;
        assert_eq!((r.status, &r.body), (want_status, &want_body), "{args}");
        assert_eq!(srv.receipt(cid(n)), None, "{args}");
    }
    srv.stop().await;
}

/// A handler's `shutting_down` refusal has the shape of every other `shutting_down` reply
/// (no `retry_after`): the same body as the guard's once the store is closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutting_down_reply_shape_is_consistent() {
    let (mut srv, _) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let id = cid(94);
    let req = srv.command(
        "test_refuse",
        &owner,
        id,
        r#"{"how":"code","code":"shutting_down","status":503}"#,
    );
    let handler = srv.send(&req).await;
    assert_eq!(srv.receipt(id), None);
    srv.store.shutdown();
    let guard = srv.send(&req).await;
    srv.shutdown_http(std::time::Instant::now() + Duration::from_secs(10))
        .await;
    assert_eq!(handler.outcome(), (503, code("shutting_down")));
    assert_eq!(guard.outcome(), (503, code("shutting_down")));
    assert_eq!(handler.body, guard.body);
}

/// A result body that is not one a handler may store is 500 `internal`, stored and
/// replayed: an error body that `serde_json::Value` cannot hold (a number out of range
/// beside a runner code), a success body that is not JSON, and error bodies with a handler
/// code at its status but `"retry_after": null` (absent is omitted, never `null`) or a
/// member other than `code` and `retry_after`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unparsable_result_bodies_are_500() {
    let (srv, _probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    for (i, args) in [
        r#"{"text":"large_number","status":409}"#,
        r#"{"text":"not_json","status":200}"#,
        r#"{"text":"null_retry_after","status":403}"#,
        r#"{"text":"extra_member","status":403}"#,
    ]
    .into_iter()
    .enumerate()
    {
        let id = cid(i as u128);
        let req = srv.command("test_completed", &owner, id, args);
        let first = srv.send(&req).await;
        let replay = srv.send(&req).await;
        assert_eq!(
            (first.status, first.body.as_slice()),
            (500, INTERNAL),
            "{args}"
        );
        assert_eq!(
            (replay.status, &replay.body),
            (first.status, &first.body),
            "{args}"
        );
        assert_eq!(
            srv.receipt(id).unwrap().status,
            ReceiptStatus::Completed(completed(&first)),
            "{args}"
        );
    }
    srv.stop().await;
}

/// A handler's 405 `method_not_allowed` (which needs an `Allow` the runner does not send) is
/// 500 `internal`, as a result and as a refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_method_rejection_is_500() {
    let (srv, _probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let req = srv.command(
        "test_completed",
        &owner,
        cid(1),
        r#"{"text":"x","status":405,"code":"method_not_allowed"}"#,
    );
    for _ in 0..2 {
        let r = srv.send(&req).await;
        assert_eq!((r.status, r.body.as_slice()), (500, INTERNAL), "{r:?}");
    }
    let r = srv
        .send(&srv.command(
            "test_refuse",
            &owner,
            cid(2),
            r#"{"how":"code","code":"method_not_allowed","status":405}"#,
        ))
        .await;
    assert_eq!((r.status, r.body.as_slice()), (500, INTERNAL), "{r:?}");
    srv.stop().await;
}
