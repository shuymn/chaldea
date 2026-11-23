//! Hook ingest against the test server with the hook mount (`docs/design/hook-ingest.md`
//! §6.2, §7.2): deduplication, binding, selection, buffering and promotion, the buffer's
//! bounds, `raw_unknown`, and the token checks.

mod support;

use std::time::Duration;

use chaldea_core::ids::{Agent, StreamId};
use chaldea_core::payload::{Content, RawUnknownReason as R};
use chaldea_daemon::hook::{HookConfig, resolve_unbindable};
use chaldea_daemon::store::Routing;
use chaldea_pty_runtime::process::AncestryEnd;
use support::hook::{
    EMITTER_PID, Hook, HookServer, L_PID, L_START, Launch, Mode, chain, direct, exit, payload,
    reserve_and_start,
};
use support::{launch, soon, thread};
use uuid::Uuid;

/// A hook server on a stopped clock ([`support::http::SkewClock::stop`]): no request
/// deadline passes, so a reply here depends on the server's work and not on how fast the
/// host runs it. None of these tests is about a deadline (`hook_b8` and the unit tests
/// cover those).
async fn server() -> HookServer {
    stopped(HookServer::start().await)
}

fn stopped(h: HookServer) -> HookServer {
    h.srv.clock.stop();
    h
}

fn ok(r: &support::http::Reply) {
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.body.is_empty(), "{r:?}");
    assert_eq!(r.header("content-length"), Some("0"), "{r:?}");
}

/// The kinds of the hook-sourced events of `l`'s thread.
fn kinds(h: &HookServer, l: &Launch) -> Vec<String> {
    h.hook_events(l.thread)
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

fn session_of(l: &Launch) -> String {
    l.launch.to_string()
}

async fn bind_codex(h: &HookServer, l: &Launch, session: &str) -> Uuid {
    let start = Hook::of(l, "SessionStart", session);
    ok(&h.post(&start).await);
    assert_eq!(h.launch_row(l.launch).session_id.as_deref(), Some(session));
    start.emit
}

// ---- PR9-CL-1 to CL-4: one delivery per emit id ------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_replays_first_reply() {
    let h = server().await;
    let l = h.codex(1).await;
    bind_codex(&h, &l, "s").await;
    let hook = Hook::of(&l, "Stop", "s");
    ok(&h.post(&hook).await);
    let events = h.stream(l.thread).len();
    let calls = h.adapter.calls().len();
    let first = h.delivery(l.launch, hook.emit).unwrap();
    ok(&h.post(&hook.clone().attempt(Some("2"))).await);
    assert_eq!(h.stream(l.thread).len(), events, "the retry added an event");
    assert_eq!(h.adapter.calls().len(), calls, "the retry was interpreted");
    assert_eq!(h.delivery(l.launch, hook.emit).unwrap(), first);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_retries_dedup_in_the_job() {
    let h = server().await;
    let l = h.codex(1).await;
    bind_codex(&h, &l, "s").await;
    let events = h.stream(l.thread).len();
    let calls = h.adapter.calls().len();
    let hook = Hook::of(&l, "Stop", "s");
    // The writer held: both posts pass the pre-read (no delivery yet) and queue a job.
    let release = h.srv.hold_writer().await;
    let a = h.spawn_post(&hook);
    let b = h.spawn_post(&hook.clone().attempt(Some("2")));
    // The binding's walk and both posts' walks: each post's walk follows its pre-read, so
    // both passed the pre-read before either job could commit, and the second job finds
    // the first's delivery only in the job, whenever it was submitted.
    support::http::eventually(Duration::from_secs(10), "both walked", || {
        h.chains.walked() >= 3
    })
    .await;
    release.send(()).unwrap();
    ok(&a.await.unwrap());
    ok(&b.await.unwrap());
    assert_eq!(h.stream(l.thread).len(), events + 1, "one event");
    assert_eq!(h.adapter.calls().len(), calls + 1, "one adapter call");
    assert!(h.delivery(l.launch, hook.emit).is_some());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_waits_for_commit() {
    let h = server().await;
    let l = h.codex(1).await;
    bind_codex(&h, &l, "s").await;
    let hook = Hook::of(&l, "Stop", "s");
    let release = h.srv.hold_writer().await;
    let reply = h.spawn_post(&hook);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!reply.is_finished(), "a reply before the job committed");
    assert!(h.delivery(l.launch, hook.emit).is_none());
    release.send(()).unwrap();
    ok(&reply.await.unwrap());
    assert!(h.delivery(l.launch, hook.emit).is_some());
    h.stop().await;
}

/// The stopped clock of [`server`]: a hook whose job waits in the writer's queue past the
/// test deadline and its 100 ms reply grace is still answered by the job.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_clock_cuts_no_request() {
    let h = server().await;
    let l = h.codex(1).await;
    let hook = Hook::of(&l, "Stop", "s");
    let release = h.srv.hold_writer().await;
    let reply = h.spawn_post(&hook);
    tokio::time::sleep(support::http::DEADLINE + Duration::from_millis(500)).await;
    assert!(!reply.is_finished(), "a reply while the writer was held");
    release.send(()).unwrap();
    ok(&reply.await.unwrap());
    assert!(h.delivery(l.launch, hook.emit).is_some());
    h.stop().await;
}

/// Both replay paths answer the stored first reply, whatever it is: the pre-read's (a
/// delivery already stored) and the job's (stored after the pre-read, while the walk ran).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replays_answer_the_stored_reply() {
    use chaldea_daemon::store::{BufferedHook, HookReply, NewHookDelivery, NewRouting};
    let h = server().await;
    let l = h.codex(1).await;
    bind_codex(&h, &l, "s").await;
    let store = |emit: Uuid, ordinal: u64, body: &'static str| {
        let lid = l.launch;
        h.run(move |tx| {
            tx.hook_delivery_record(NewHookDelivery {
                launch_id: lid,
                emit_id: emit,
                receipt_ordinal: ordinal,
                first_reply: HookReply {
                    http_status: 202,
                    body: body.into(),
                },
                routing: NewRouting::Buffered(BufferedHook {
                    body: b"{}".to_vec(),
                    meta: "{}".into(),
                }),
            })?;
            Ok(())
        })
    };
    // The pre-read's replay.
    let pre = Hook::of(&l, "Stop", "s");
    store(pre.emit, 100, "{\"stored\":1}").await;
    let r = h.post(&pre).await;
    assert_eq!(
        (r.status, r.body.as_slice()),
        (202, &b"{\"stored\":1}"[..]),
        "{r:?}"
    );
    // The job's replay: the delivery is stored while the request walks.
    h.chains.set(7001, direct(7001, "codex"));
    let (walking, release) = h.chains.hold(7001);
    let job = Hook::of(&l, "Stop", "s").pid(Some(7001));
    let reply = h.spawn_post(&job);
    tokio::task::spawn_blocking(move || walking.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .unwrap();
    store(job.emit, 101, "{\"stored\":2}").await;
    release.send(()).unwrap();
    let r = reply.await.unwrap();
    assert_eq!(
        (r.status, r.body.as_slice()),
        (202, &b"{\"stored\":2}"[..]),
        "{r:?}"
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_after_binding_keeps_first_reply() {
    let h = server().await;
    let l = h.codex(1).await;
    let early = Hook::of(&l, "UserPromptSubmit", "s");
    ok(&h.post(&early).await);
    let d = h.delivery(l.launch, early.emit).unwrap();
    assert_eq!(d.routing, Routing::Buffered);
    bind_codex(&h, &l, "s").await;
    let events = h.stream(l.thread).len();
    ok(&h.post(&early.clone().attempt(Some("2"))).await);
    let after = h.delivery(l.launch, early.emit).unwrap();
    assert_eq!(after.first_reply, d.first_reply, "the first reply");
    assert!(
        matches!(after.routing, Routing::Appended { .. }),
        "the promoted routing: {after:?}"
    );
    assert_eq!(h.stream(l.thread).len(), events, "no new event");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_bodies_are_two_deliveries() {
    let h = server().await;
    let l = h.codex(1).await;
    bind_codex(&h, &l, "s").await;
    let events = h.stream(l.thread).len();
    let a = Hook::of(&l, "Stop", "s");
    let b = Hook::of(&l, "Stop", "s");
    ok(&h.post(&a).await);
    ok(&h.post(&b).await);
    assert!(h.delivery(l.launch, a.emit).is_some());
    assert!(h.delivery(l.launch, b.emit).is_some());
    assert_eq!(h.stream(l.thread).len(), events + 2);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffered_durable_before_200() {
    let h = server().await;
    let l = h.codex(1).await;
    let hook = Hook::of(&l, "UserPromptSubmit", "s");
    let release = h.srv.hold_writer().await;
    let reply = h.spawn_post(&hook);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !reply.is_finished(),
        "200 before the buffered row committed"
    );
    release.send(()).unwrap();
    ok(&reply.await.unwrap());
    assert_eq!(
        h.delivery(l.launch, hook.emit).unwrap().routing,
        Routing::Buffered
    );
    h.stop().await;
}

// ---- PR9-CL-5: ordinals -----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_ordinals_unique() {
    let h = stopped(
        HookServer::start_with(
            HookConfig {
                buffer_max_count: 100,
                ..HookConfig::default()
            },
            |_| {},
        )
        .await,
    );
    let l = h.codex(1).await;
    bind_codex(&h, &l, "s").await;
    let hooks: Vec<Hook> = (0..64).map(|_| Hook::of(&l, "Stop", "s")).collect();
    let tasks: Vec<_> = hooks.iter().map(|x| h.spawn_post(x)).collect();
    for t in tasks {
        ok(&t.await.unwrap());
    }
    let mut ordinals: Vec<u64> = hooks
        .iter()
        .map(|x| h.delivery(l.launch, x.emit).unwrap().receipt_ordinal)
        .collect();
    ordinals.sort_unstable();
    ordinals.dedup();
    assert_eq!(ordinals.len(), 64, "distinct ordinals");
    // Facts were taken in ordinal order: the k-th probe call got ordinal k (the binding
    // SessionStart was ordinal 1, probe call 1).
    for x in &hooks {
        let d = h.delivery(l.launch, x.emit).unwrap();
        let call = h
            .adapter
            .calls()
            .into_iter()
            .find(|c| c.emit_id == x.emit)
            .unwrap();
        assert_eq!(
            call.intake.unwrap().0["n"].as_u64(),
            Some(d.receipt_ordinal),
            "facts and ordinal in one order"
        );
    }
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinals_continue_after_restart() {
    let h = server().await;
    let l = h.codex(1).await;
    bind_codex(&h, &l, "s").await;
    for _ in 0..2 {
        ok(&h.post(&Hook::of(&l, "Stop", "s")).await);
    }
    let HookServer { srv, .. } = h;
    let (_, store) = srv.into_store(Duration::from_secs(10)).await;
    let h = stopped(HookServer::start_on(store, HookConfig::default(), |_| {}).await);
    h.chains.set(EMITTER_PID, direct(EMITTER_PID, "codex"));
    let next = Hook::of(&l, "Stop", "s");
    ok(&h.post(&next).await);
    assert_eq!(h.delivery(l.launch, next.emit).unwrap().receipt_ordinal, 4);
    h.stop().await;
}

// ---- PR9-CL-6: binding order -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binding_waits_for_earlier_hook() {
    let h = server().await;
    let l = h.codex(1).await;
    // Hook 1 walks a chain of its own pid, held.
    h.chains.set(7001, direct(7001, "codex"));
    let (walking, release) = h.chains.hold(7001);
    let first = Hook::of(&l, "UserPromptSubmit", "s").pid(Some(7001));
    let first_reply = h.spawn_post(&first);
    tokio::task::spawn_blocking(move || walking.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .unwrap();
    let start = Hook::of(&l, "SessionStart", "s");
    let start_reply = h.spawn_post(&start);
    let stop = Hook::of(&l, "Stop", "s");
    let stop_reply = h.spawn_post(&stop);
    // Stop 3 is buffered (the launch is unbound); the SessionStart waits for ordinal 1.
    ok(&stop_reply.await.unwrap());
    assert_eq!(
        h.launch_row(l.launch).session_id,
        None,
        "bound before hook 1"
    );
    release.send(()).unwrap();
    ok(&first_reply.await.unwrap());
    ok(&start_reply.await.unwrap());
    let order: Vec<Uuid> = h
        .adapter
        .calls()
        .into_iter()
        .filter(|c| c.promoted || c.event_name != "SessionStart")
        .map(|c| c.emit_id)
        .collect();
    assert_eq!(
        order,
        vec![first.emit, stop.emit],
        "promotion in receipt order"
    );
    assert_eq!(
        kinds(&h, &l),
        vec![
            "session_bound",
            "raw_unknown",
            "user_message",
            "assistant_message"
        ],
        "session_bound, the binding's raw, UserPromptSubmit 1, Stop 3"
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn earlier_session_start_wins() {
    let h = server().await;
    let l = h.codex(1).await;
    h.chains.set(7001, direct(7001, "codex"));
    let (walking, release) = h.chains.hold(7001);
    let earlier = Hook::of(&l, "SessionStart", "first").pid(Some(7001));
    let earlier_reply = h.spawn_post(&earlier);
    tokio::task::spawn_blocking(move || walking.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .unwrap();
    let later = Hook::of(&l, "SessionStart", "second");
    let later_reply = h.spawn_post(&later);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !later_reply.is_finished(),
        "the later SessionStart overtook"
    );
    release.send(()).unwrap();
    ok(&earlier_reply.await.unwrap());
    ok(&later_reply.await.unwrap());
    assert_eq!(h.launch_row(l.launch).session_id.as_deref(), Some("first"));
    h.assert_raw(&l, later.emit, R::ForeignSession);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn released_gap_does_not_block() {
    let h = server().await;
    let l = h.codex(1).await;
    h.chains.set(7001, direct(7001, "codex"));
    let (walking, _release) = h.chains.hold(7001);
    // Ordinal 1: its client disconnects during the walk.
    let first = Hook::of(&l, "UserPromptSubmit", "s").pid(Some(7001));
    let mut conn = support::http::connect(h.srv.port).await;
    support::http::write(&mut conn, &first.req(&h.srv).bytes()).await;
    tokio::task::spawn_blocking(move || walking.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .unwrap();
    drop(conn);
    // Ordinal 2 binds. Ordinal 1's walk stays held and its deadline never passes on the
    // stopped clock, so only the disconnect can have released it.
    bind_codex(&h, &l, "s").await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_earlier_job_releases_at_submit() {
    let h = server().await;
    let l = h.codex(1).await;
    let release = h.srv.hold_writer().await;
    // Hook 1 is submitted behind the held writer; its client then disconnects.
    let first = Hook::of(&l, "UserPromptSubmit", "s");
    let mut conn = support::http::connect(h.srv.port).await;
    support::http::write(&mut conn, &first.req(&h.srv).bytes()).await;
    support::http::eventually(Duration::from_secs(10), "hook 1 walked", || {
        h.chains.walked() > 0
    })
    .await;
    // The binding SessionStart 2 is submitted too: hook 1's ordinal was released when
    // its job was queued, not when it ran.
    let start = Hook::of(&l, "SessionStart", "s");
    let start_reply = h.spawn_post(&start);
    support::http::eventually(Duration::from_secs(10), "the binding walked", || {
        h.chains.walked() >= 2
    })
    .await;
    drop(conn);
    release.send(()).unwrap();
    ok(&start_reply.await.unwrap());
    let d1 = h.delivery(l.launch, first.emit).expect("hook 1 committed");
    let d2 = h.delivery(l.launch, start.emit).unwrap();
    assert!(
        d1.commit_ordinal < d2.commit_ordinal,
        "hook 1 then the binding"
    );
    assert!(
        matches!(d1.routing, Routing::Appended { .. }),
        "promoted: {d1:?}"
    );
    h.stop().await;
}

// ---- PR9-CL-7 to CL-10: binding rules and selection --------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_binds_by_expected_session() {
    let h = server().await;
    let l = h.claude(1).await;
    // Another session: foreign, no binding. Claude ignores the chain (pid absent).
    let other = Hook::of(&l, "SessionStart", "other").pid(None);
    ok(&h.post(&other).await);
    h.assert_raw(&l, other.emit, R::ForeignSession);
    assert_eq!(h.launch_row(l.launch).session_id, None);
    let own = Hook::of(&l, "SessionStart", &session_of(&l)).pid(None);
    ok(&h.post(&own).await);
    assert_eq!(
        h.launch_row(l.launch).session_id.as_deref(),
        Some(session_of(&l).as_str())
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_binds_by_ancestry() {
    let h = server().await;
    let l = h.codex(1).await;
    // No chain: foreign.
    let none = Hook::of(&l, "SessionStart", "s").pid(None);
    ok(&h.post(&none).await);
    h.assert_raw(&l, none.emit, R::ForeignSession);
    bind_codex(&h, &l, "s").await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escaped_windows_transcript_path() {
    let h = server().await;
    let l = h.codex(1).await;
    let body = br#"{"hook_event_name":"SessionStart","session_id":"s","transcript_path":"C:\\work\\t.jsonl"}"#;
    ok(&h.post(&Hook::new(&l, body)).await);
    assert_eq!(
        h.thread_row(l.thread).transcript_path.as_deref(),
        Some(r"C:\work\t.jsonl")
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_chain_does_not_bind() {
    let h = server().await;
    let l = h.codex(1).await;
    h.chains.set(
        7001,
        chain(
            &[
                (7001, "3000", "chaldea"),
                (7002, "2500", "codex"),
                (L_PID, L_START, "codex"),
            ],
            AncestryEnd::Root,
        ),
    );
    let nested = Hook::of(&l, "SessionStart", "nested").pid(Some(7001));
    ok(&h.post(&nested).await);
    h.assert_raw(&l, nested.emit, R::ForeignSession);
    assert_eq!(h.launch_row(l.launch).session_id, None);
    bind_codex(&h, &l, "s").await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_emitter_first_link_is_not_eligible() {
    let h = server().await;
    let l = h.codex(1).await;
    h.chains.set(
        7001,
        chain(
            &[(7001, "3000", "sh"), (L_PID, L_START, "codex")],
            AncestryEnd::Root,
        ),
    );
    let x = Hook::of(&l, "SessionStart", "s").pid(Some(7001));
    ok(&h.post(&x).await);
    h.assert_raw(&l, x.emit, R::ForeignSession);
    assert_eq!(h.launch_row(l.launch).session_id, None);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_claude_new_session_is_foreign() {
    let h = server().await;
    let l = h.claude(1).await;
    ok(&h.post(&Hook::of(&l, "SessionStart", &session_of(&l))).await);
    let nested = Hook::of(&l, "SessionStart", "nested-session");
    ok(&h.post(&nested).await);
    h.assert_raw(&l, nested.emit, R::ForeignSession);
    let nested_stop = Hook::of(&l, "Stop", "nested-session");
    ok(&h.post(&nested_stop).await);
    h.assert_raw(&l, nested_stop.emit, R::ForeignSession);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_claude_same_session_is_selected() {
    let h = server().await;
    let l = h.claude(1).await;
    ok(&h.post(&Hook::of(&l, "SessionStart", &session_of(&l))).await);
    // A nested Claude resuming the launch's session id, with another chain entirely:
    // the documented limit, selected and interpreted as the launch's own.
    let nested = Hook::of(&l, "Stop", &session_of(&l)).pid(Some(7777));
    ok(&h.post(&nested).await);
    let d = h.delivery(l.launch, nested.emit).unwrap();
    assert!(matches!(d.routing, Routing::Appended { .. }), "{d:?}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn differently_named_nested_codex_binds_first() {
    let h = server().await;
    let l = h.codex(1).await;
    h.chains.set(
        7001,
        chain(
            &[
                (7001, "3000", "chaldea"),
                (7002, "2500", "codex-dev"),
                (L_PID, L_START, "codex"),
            ],
            AncestryEnd::Root,
        ),
    );
    let nested = Hook::of(&l, "SessionStart", "nested").pid(Some(7001));
    ok(&h.post(&nested).await);
    assert_eq!(h.launch_row(l.launch).session_id.as_deref(), Some("nested"));
    let own = Hook::of(&l, "SessionStart", "own");
    ok(&h.post(&own).await);
    h.assert_raw(&l, own.emit, R::ForeignSession);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_link_missing_from_prefix_is_eligible() {
    let h = server().await;
    let l = h.codex(1).await;
    // A nested `codex` created the emitter, but its link reads as L directly.
    h.chains.set(7001, direct(7001, "codex"));
    let x = Hook::of(&l, "SessionStart", "nested").pid(Some(7001));
    ok(&h.post(&x).await);
    assert_eq!(h.launch_row(l.launch).session_id.as_deref(), Some("nested"));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrecognised_link_below_another_cli_is_not_eligible() {
    let h = server().await;
    let l = h.codex(1).await;
    h.chains.set(
        7001,
        chain(
            &[
                (7001, "3000", "chaldea"),
                (7002, "2500", "codex-dev"),
                (7003, "2400", "codex"),
                (L_PID, L_START, "codex"),
            ],
            AncestryEnd::Root,
        ),
    );
    let x = Hook::of(&l, "SessionStart", "s").pid(Some(7001));
    ok(&h.post(&x).await);
    h.assert_raw(&l, x.emit, R::ForeignSession);
    assert_eq!(h.launch_row(l.launch).session_id, None);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chain_cut_before_l_stays_unbound() {
    let h = server().await;
    let l = h.codex(1).await;
    for (pid, end) in [
        (7001, AncestryEnd::Limit),
        (7002, AncestryEnd::Unreadable(L_PID)),
        (7003, AncestryEnd::ParentNewer(L_PID)),
    ] {
        h.chains.set(
            pid,
            chain(&[(pid, "3000", "chaldea"), (7100, "2500", "sh")], end),
        );
        let x = Hook::of(&l, "SessionStart", "s").pid(Some(pid));
        ok(&h.post(&x).await);
        h.assert_raw(&l, x.emit, R::ForeignSession);
        assert_eq!(h.launch_row(l.launch).session_id, None, "{end:?}");
    }
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn later_hooks_revalidate_ancestry() {
    let h = server().await;
    let l = h.codex(1).await;
    bind_codex(&h, &l, "s").await;
    h.chains.set(
        7001,
        chain(
            &[
                (7001, "3000", "chaldea"),
                (7002, "2500", "codex"),
                (L_PID, L_START, "codex"),
            ],
            AncestryEnd::Root,
        ),
    );
    let nested = Hook::of(&l, "Stop", "s").pid(Some(7001));
    ok(&h.post(&nested).await);
    h.assert_raw(&l, nested.emit, R::ForeignSession);
    // The control: the same hook from an eligible chain is interpreted.
    let own = Hook::of(&l, "Stop", "s");
    ok(&h.post(&own).await);
    let d = h.delivery(l.launch, own.emit).unwrap();
    assert!(matches!(d.routing, Routing::Appended { .. }), "{d:?}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conflicting_session_start_is_foreign() {
    let h = server().await;
    let l = h.codex(1).await;
    bind_codex(&h, &l, "s").await;
    let other = Hook::of(&l, "SessionStart", "other");
    ok(&h.post(&other).await);
    h.assert_raw(&l, other.emit, R::ForeignSession);
    assert_eq!(h.launch_row(l.launch).session_id.as_deref(), Some("s"));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_session_never_interpreted() {
    let h = server().await;
    let l = h.codex(1).await;
    // Unbound: an ineligible chain.
    let unbound = Hook::of(&l, "Stop", "s").pid(None);
    ok(&h.post(&unbound).await);
    h.assert_raw(&l, unbound.emit, R::ForeignSession);
    bind_codex(&h, &l, "s").await;
    let calls = h.adapter.calls().len();
    // Bound: another session id.
    let other = Hook::of(&l, "Stop", "other");
    ok(&h.post(&other).await);
    h.assert_raw(&l, other.emit, R::ForeignSession);
    // Absent or other thread header (row 4).
    let no_header = Hook::of(&l, "Stop", "s").thread(None);
    ok(&h.post(&no_header).await);
    h.assert_raw(&l, no_header.emit, R::ForeignSession);
    let other_thread = Hook::of(&l, "Stop", "s").thread(Some(&thread(9).to_string()));
    ok(&h.post(&other_thread).await);
    h.assert_raw(&l, other_thread.emit, R::ForeignSession);
    assert_eq!(
        h.adapter.calls().len(),
        calls,
        "a foreign hook reached interpret"
    );
    // Claude, unbound: not the expected session.
    let c = h.claude(2).await;
    let foreign = Hook::of(&c, "Stop", "nope");
    ok(&h.post(&foreign).await);
    h.assert_raw(&c, foreign.emit, R::ForeignSession);
    assert_eq!(h.adapter.calls().len(), calls);
    h.stop().await;
}

// ---- PR9-CL-11, CL-12: buffering and promotion -------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unbound_intake_changes_nothing() {
    let h = server().await;
    h.adapter.set_mode(Mode::Readiness);
    let l = h.codex(1).await;
    let before = h.snapshot(&l);
    let early = Hook::of(&l, "UserPromptSubmit", "s");
    ok(&h.post(&early).await);
    let after = h.snapshot(&l);
    assert_eq!(
        after.heads, before.heads,
        "a buffered hook appended an event"
    );
    assert_eq!(
        after.tables, before.tables,
        "a buffered hook changed a projection"
    );
    assert_eq!(after.deliveries, before.deliveries + 1);
    bind_codex(&h, &l, "s").await;
    let basis = h.launch_row(l.launch).write_basis.expect("a write basis");
    // The early hook's intake was probe call 1.
    assert_eq!(basis.position, 1, "the intake basis, not the binding's");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promotion_in_receipt_order() {
    let h = server().await;
    let l = h.codex(1).await;
    // Ordinal 1 walks held, so ordinal 2 commits first.
    h.chains.set(7001, direct(7001, "codex"));
    let (walking, release) = h.chains.hold(7001);
    let one = Hook::of(&l, "UserPromptSubmit", "s").pid(Some(7001));
    let one_reply = h.spawn_post(&one);
    tokio::task::spawn_blocking(move || walking.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .unwrap();
    let two = Hook::of(&l, "Stop", "s");
    ok(&h.post(&two).await);
    release.send(()).unwrap();
    ok(&one_reply.await.unwrap());
    let (d1, d2) = (
        h.delivery(l.launch, one.emit).unwrap(),
        h.delivery(l.launch, two.emit).unwrap(),
    );
    assert!(d1.receipt_ordinal < d2.receipt_ordinal);
    assert!(d1.commit_ordinal > d2.commit_ordinal, "committed 2, 1");
    bind_codex(&h, &l, "s").await;
    let promoted: Vec<Uuid> = h
        .adapter
        .calls()
        .into_iter()
        .filter(|c| c.promoted)
        .map(|c| c.emit_id)
        .collect();
    assert_eq!(promoted, vec![one.emit, two.emit]);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promotion_carries_intake_facts() {
    let h = server().await;
    let l = h.codex(1).await;
    let early = Hook::of(&l, "UserPromptSubmit", "s").attempt(Some("2"));
    ok(&h.post(&early).await);
    bind_codex(&h, &l, "s").await;
    let call = h
        .adapter
        .calls()
        .into_iter()
        .find(|c| c.emit_id == early.emit)
        .unwrap();
    assert!(call.promoted);
    assert_eq!(
        call.intake.unwrap().0["n"].as_u64(),
        Some(1),
        "its own intake"
    );
    assert_eq!(call.attempt, Some(2));
    assert!(!call.intake_reliable);
    h.stop().await;
}

/// Facts the probe returned as JSON `null` are present facts: the promoted hook gets
/// `Some(null)`, as a direct delivery after the binding does, not `None`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promotion_keeps_present_null_facts() {
    let h = server().await;
    let l = h.codex(1).await;
    h.facts
        .null
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let early = Hook::of(&l, "UserPromptSubmit", "s");
    ok(&h.post(&early).await);
    bind_codex(&h, &l, "s").await;
    let direct_hook = Hook::of(&l, "UserPromptSubmit", "s");
    ok(&h.post(&direct_hook).await);
    let calls = h.adapter.calls();
    let facts = |emit| {
        calls
            .iter()
            .find(|c| c.emit_id == emit)
            .unwrap()
            .intake
            .clone()
    };
    let null = Some(chaldea_daemon::hook::IntakeFacts(serde_json::Value::Null));
    assert_eq!(facts(early.emit), null, "promoted");
    assert_eq!(facts(direct_hook.emit), null, "direct");
    h.stop().await;
}

/// A buffered hook of another session than the one bound is `foreign_session` at
/// promotion, never interpreted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promotion_refuses_another_session() {
    let h = server().await;
    let l = h.codex(1).await;
    let other = Hook::of(&l, "UserPromptSubmit", "other");
    ok(&h.post(&other).await);
    assert_eq!(
        h.delivery(l.launch, other.emit).unwrap().routing,
        Routing::Buffered
    );
    bind_codex(&h, &l, "s").await;
    h.assert_raw(&l, other.emit, R::ForeignSession);
    assert!(h.adapter.calls().iter().all(|c| c.emit_id != other.emit));
    h.stop().await;
}

/// Writes a buffered delivery directly, with `meta` and `body`.
async fn buffer_directly(
    h: &HookServer,
    l: &Launch,
    ordinal: u64,
    body: Vec<u8>,
    meta: String,
) -> Uuid {
    use chaldea_daemon::store::{BufferedHook, HookReply, NewHookDelivery, NewRouting};
    let emit = Uuid::new_v4();
    let launch = l.launch;
    h.run(move |tx| {
        Ok(tx.hook_delivery_record(NewHookDelivery {
            launch_id: launch,
            emit_id: emit,
            receipt_ordinal: ordinal,
            first_reply: HookReply {
                http_status: 200,
                body: String::new(),
            },
            routing: NewRouting::Buffered(BufferedHook { body, meta }),
        })?)
    })
    .await;
    emit
}

fn meta(l: &Launch, start: &str) -> String {
    serde_json::json!({
        "v": 1,
        "thread_header": l.thread.to_string(),
        "agent_header": "codex",
        "emitter_version": "0.1.0",
        "attempt": 1,
        "chain": [
            {"pid": 7001, "start_time": "3000", "name": "chaldea"},
            {"pid": L_PID, "start_time": start, "name": "codex"}
        ],
        "chain_end": "root"
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promotion_rechecks_ancestry() {
    let h = server().await;
    let l = h.codex(1).await;
    let wrong = buffer_directly(&h, &l, 100, payload("Stop", "s"), meta(&l, "999")).await;
    let right = buffer_directly(&h, &l, 101, payload("Stop", "s"), meta(&l, L_START)).await;
    bind_codex(&h, &l, "s").await;
    h.assert_raw(&l, wrong, R::ForeignSession);
    let d = h.delivery(l.launch, right).unwrap();
    assert!(matches!(d.routing, Routing::Appended { .. }), "{d:?}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binding_ordinal_on_promoted_and_direct() {
    let h = server().await;
    let l = h.codex(1).await;
    ok(&h.post(&Hook::of(&l, "UserPromptSubmit", "s")).await);
    let start = Hook::of(&l, "SessionStart", "s");
    ok(&h.post(&start).await);
    let binding = h.delivery(l.launch, start.emit).unwrap().receipt_ordinal;
    ok(&h.post(&Hook::of(&l, "Stop", "s")).await);
    let calls = h.adapter.calls();
    assert_eq!(calls.len(), 3);
    for c in &calls {
        assert_eq!(c.binding_ordinal, binding, "{c:?}");
    }
    assert_eq!(calls.iter().filter(|c| c.promoted).count(), 1);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binding_interpret_sees_bound_state() {
    let h = server().await;
    let l = h.codex(1).await;
    bind_codex(&h, &l, "s").await;
    let c = &h.adapter.calls()[0];
    assert_eq!(c.event_name, "SessionStart");
    assert_eq!(c.launch_session.as_deref(), Some("s"));
    assert_eq!(c.thread_session.as_deref(), Some("s"));
    assert!(!c.promoted);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_promotion_rolls_back_binding() {
    for mode in [Mode::RepeatName, Mode::ReservedName] {
        let h = server().await;
        let l = h.codex(1).await;
        let early = Hook::of(&l, "UserPromptSubmit", "s");
        ok(&h.post(&early).await);
        h.adapter.set_mode(mode);
        let start = Hook::of(&l, "SessionStart", "s");
        let r = h.post(&start).await;
        assert_eq!(r.outcome(), (500, Some("internal".into())), "{mode:?}");
        assert_eq!(h.launch_row(l.launch).session_id, None, "{mode:?}: bound");
        assert!(h.delivery(l.launch, start.emit).is_none());
        assert_eq!(
            h.delivery(l.launch, early.emit).unwrap().routing,
            Routing::Buffered,
            "{mode:?}: the buffer changed"
        );
        h.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binding_raw_is_kept() {
    let srv = support::http::TestServer::start(chaldea_daemon::http::Mounts::mvp().with(
        chaldea_daemon::hook::mount(
            HookConfig::default(),
            chaldea_daemon::hook::HookSeams {
                procs: std::sync::Arc::new({
                    let c = support::hook::Chains::default();
                    c.set(EMITTER_PID, direct(EMITTER_PID, "codex"));
                    c
                }),
                ..chaldea_daemon::hook::HookSeams::mvp()
            },
        ),
    ))
    .await;
    srv.clock.stop();
    let (t, lid) = (thread(1), launch(1));
    srv.append(vec![support::hook::register(t, Agent::Codex)])
        .await;
    let state = srv.store.read(move |tx| tx.thread(t)).unwrap().state;
    srv.append(reserve_and_start(t, lid, Agent::Codex, &state, false, true))
        .await;
    let token = srv
        .token(chaldea_core::auth::TokenKind::Hook, Some(t), Some(lid))
        .await;
    let l = Launch {
        thread: t,
        launch: lid,
        agent: Agent::Codex,
        token,
    };
    let start = Hook::of(&l, "SessionStart", "s");
    ok(&srv.send(&start.req(&srv)).await);
    let events = srv
        .store
        .read(move |tx| tx.events_after(StreamId::Thread(t), 0, 100))
        .events;
    let tail: Vec<(String, String)> = events
        .iter()
        .rev()
        .take(2)
        .rev()
        .map(|e| (e.kind.0.clone(), e.idempotency_key.0.clone()))
        .collect();
    assert_eq!(
        tail,
        vec![
            ("session_bound".into(), format!("hook:{}", start.emit)),
            (
                "raw_unknown".into(),
                format!("derived:hook:{}:binding_raw", start.emit)
            ),
        ]
    );
    srv.stop().await;
}

// ---- PR9-CL-13: buffer bounds ------------------------------------------------------------

async fn buffered(h: &HookServer, l: &Launch, n: usize) -> Vec<Uuid> {
    let mut out = Vec::new();
    for _ in 0..n {
        let x = Hook::of(l, "Stop", "s");
        ok(&h.post(&x).await);
        out.push(x.emit);
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffer_count_overflow() {
    let h = server().await;
    let l = h.codex(1).await;
    let first = buffered(&h, &l, 32).await;
    for e in &first {
        assert_eq!(h.delivery(l.launch, *e).unwrap().routing, Routing::Buffered);
    }
    let over = Hook::of(&l, "Stop", "s");
    ok(&h.post(&over).await);
    let mut seqs = Vec::new();
    for e in &first {
        h.assert_raw(&l, *e, R::Unbound);
        seqs.push(h.routed_event(&h.delivery(l.launch, *e).unwrap()).seq);
    }
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "receipt order");
    h.assert_raw(&l, over.emit, R::Unbound);
    // The buffer starts again.
    let again = Hook::of(&l, "Stop", "s");
    ok(&h.post(&again).await);
    assert_eq!(
        h.delivery(l.launch, again.emit).unwrap().routing,
        Routing::Buffered
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffer_bytes_overflow() {
    let h = server().await;
    let l = h.codex(1).await;
    // Bodies padded to exactly 4 MiB: two fill the 8 MiB exactly; a third byte over.
    let body = |len: usize| {
        let mut v = serde_json::to_vec(&serde_json::json!({
            "hook_event_name": "Stop", "session_id": "s", "pad": ""
        }))
        .unwrap();
        let pad = len - v.len();
        v.truncate(v.len() - 2);
        v.extend(std::iter::repeat_n(b'x', pad));
        v.extend_from_slice(b"\"}");
        assert_eq!(v.len(), len);
        v
    };
    let a = Hook::new(&l, &body(4 * 1024 * 1024));
    let b = Hook::new(&l, &body(4 * 1024 * 1024));
    ok(&h.post(&a).await);
    ok(&h.post(&b).await);
    assert_eq!(
        h.delivery(l.launch, b.emit).unwrap().routing,
        Routing::Buffered,
        "exactly 8 MiB"
    );
    let c = Hook::new(&l, &body(64));
    ok(&h.post(&c).await);
    h.assert_raw(&l, a.emit, R::Unbound);
    h.assert_raw(&l, b.emit, R::Unbound);
    h.assert_raw(&l, c.emit, R::Unbound);
    h.stop().await;
}

fn set_clock(h: &HookServer, at: &str) {
    h.srv.store.clock.set(at);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffer_ttl_expires() {
    let h = server().await;
    let l = h.codex(1).await;
    set_clock(&h, "2026-11-22T00:00:00.000Z");
    let old = buffered(&h, &l, 1).await[0];
    set_clock(&h, "2026-11-22T00:00:30.000Z");
    let at_ttl = buffered(&h, &l, 1).await[0];
    assert_eq!(
        h.delivery(l.launch, old).unwrap().routing,
        Routing::Buffered,
        "at exactly 30 s"
    );
    set_clock(&h, "2026-11-22T00:00:30.001Z");
    let late = buffered(&h, &l, 1).await[0];
    h.assert_raw(&l, old, R::Unbound);
    h.assert_raw(&l, at_ttl, R::Unbound);
    assert_eq!(
        h.delivery(l.launch, late).unwrap().routing,
        Routing::Buffered
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ttl_uses_earliest_received_at() {
    let h = server().await;
    let l = h.codex(1).await;
    // Ordinal 1 commits after ordinal 2, with a later stamp.
    h.chains.set(7001, direct(7001, "codex"));
    let (walking, release) = h.chains.hold(7001);
    let one = Hook::of(&l, "Stop", "s").pid(Some(7001));
    let one_reply = h.spawn_post(&one);
    tokio::task::spawn_blocking(move || walking.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .unwrap();
    set_clock(&h, "2026-11-22T00:00:00.000Z");
    let two = buffered(&h, &l, 1).await[0];
    set_clock(&h, "2026-11-22T00:00:20.000Z");
    release.send(()).unwrap();
    ok(&one_reply.await.unwrap());
    // 31 s after ordinal 2 (the earliest stamp), 11 s after ordinal 1.
    set_clock(&h, "2026-11-22T00:00:31.000Z");
    buffered(&h, &l, 1).await;
    h.assert_raw(&l, one.emit, R::Unbound);
    h.assert_raw(&l, two, R::Unbound);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_back_expires_nothing() {
    let h = server().await;
    let l = h.codex(1).await;
    set_clock(&h, "2026-11-22T01:00:00.000Z");
    let e = buffered(&h, &l, 1).await[0];
    set_clock(&h, "2026-11-22T00:00:00.000Z");
    buffered(&h, &l, 1).await;
    assert_eq!(h.delivery(l.launch, e).unwrap().routing, Routing::Buffered);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_overflow() {
    let h = server().await;
    let l = h.codex(1).await;
    let hooks: Vec<Hook> = (0..33).map(|_| Hook::of(&l, "Stop", "s")).collect();
    let tasks: Vec<_> = hooks.iter().map(|x| h.spawn_post(x)).collect();
    for t in tasks {
        ok(&t.await.unwrap());
    }
    let mut unbound = 0;
    for x in &hooks {
        match h.delivery(l.launch, x.emit).unwrap().routing {
            Routing::RawUnknown { .. } => {
                h.assert_raw(&l, x.emit, R::Unbound);
                unbound += 1;
            }
            Routing::Buffered => {}
            other @ Routing::Appended { .. } => panic!("{other:?}"),
        }
    }
    assert_eq!(unbound, 33, "one overflow decided every delivery once");
    h.stop().await;
}

/// An overflow drains in receipt order: receipt 1, held in its walk while receipts 2–33
/// fill the buffer, overflows on release, and the 33 `raw_unknown` events follow receipt
/// order, receipt 1 first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overflow_keeps_receipt_order() {
    let h = server().await;
    let l = h.codex(1).await;
    h.chains.set(7001, direct(7001, "codex"));
    let (walking, release) = h.chains.hold(7001);
    let first = Hook::of(&l, "Stop", "s").pid(Some(7001));
    let first_reply = h.spawn_post(&first);
    tokio::task::spawn_blocking(move || walking.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .unwrap();
    let mut later = Vec::new();
    for _ in 0..32 {
        let x = Hook::of(&l, "Stop", "s");
        ok(&h.post(&x).await);
        later.push(x.emit);
    }
    release.send(()).unwrap();
    ok(&first_reply.await.unwrap());
    let mut want = vec![format!("hook:{}", first.emit)];
    want.extend(later.iter().map(|e| format!("hook:{e}")));
    let got: Vec<String> = h
        .hook_events(l.thread)
        .into_iter()
        .map(|(_, k)| k)
        .collect();
    assert_eq!(got, want);
    h.stop().await;
}

// ---- PR9-CL-14: unbound launches ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_current_launch_is_unbound() {
    let h = server().await;
    let l = h.codex(1).await;
    let state = h.thread_row(l.thread).state;
    h.srv.append(exit(l.thread, l.launch, &state)).await;
    let state = h.thread_row(l.thread).state;
    h.srv
        .append(reserve_and_start(
            l.thread,
            launch(2),
            Agent::Codex,
            &state,
            true,
            false,
        ))
        .await;
    let x = Hook::of(&l, "SessionStart", "s");
    ok(&h.post(&x).await);
    h.assert_raw(&l, x.emit, R::Unbound);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_before_bind_then_after_bind() {
    // Exit first: the buffer is resolved, a later SessionStart is unbound.
    let h = server().await;
    let l = h.codex(1).await;
    let early = buffered(&h, &l, 1).await[0];
    let (t, lid) = (l.thread, l.launch);
    let exit_events = exit(t, lid, &h.thread_row(t).state);
    h.run(move |tx| {
        tx.append(exit_events)?;
        Ok(resolve_unbindable(tx, lid)?)
    })
    .await;
    h.assert_raw(&l, early, R::Unbound);
    let late = Hook::of(&l, "SessionStart", "s");
    ok(&h.post(&late).await);
    h.assert_raw(&l, late.emit, R::Unbound);
    // Bind first: promotion once; a hook after the exit is unbound and moves nothing.
    let l2 = h.codex(2).await;
    buffered(&h, &l2, 1).await;
    bind_codex(&h, &l2, "s").await;
    let (t2, lid2) = (l2.thread, l2.launch);
    let exit_events = exit(t2, lid2, &h.thread_row(t2).state);
    let n = h
        .run(move |tx| {
            tx.append(exit_events)?;
            Ok(resolve_unbindable(tx, lid2)?)
        })
        .await;
    assert_eq!(n, 0, "the promoted buffer was resolved again");
    let turn = h.launch_row(lid2).last_turn_start_seq;
    let after = Hook::of(&l2, "UserPromptSubmit", "s");
    ok(&h.post(&after).await);
    h.assert_raw(&l2, after.emit, R::Unbound);
    assert_eq!(h.launch_row(lid2).last_turn_start_seq, turn);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_unbindable_clears_buffer() {
    let h = server().await;
    let l = h.codex(1).await;
    let ids = buffered(&h, &l, 3).await;
    let lid = l.launch;
    let n = h.run(move |tx| Ok(resolve_unbindable(tx, lid)?)).await;
    assert_eq!(n, 3);
    for e in ids {
        h.assert_raw(&l, e, R::Unbound);
    }
    assert!(
        h.srv
            .store
            .read(move |tx| tx.buffered_deliveries(lid))
            .is_empty()
    );
    h.stop().await;
}

// ---- PR9-CL-16: bodies kept verbatim -----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unparseable_kept_verbatim() {
    let h = server().await;
    let l = h.codex(1).await;
    let text = Hook::new(&l, b"not json");
    ok(&h.post(&text).await);
    let r = h.assert_raw(&l, text.emit, R::Unparseable);
    assert_eq!(r.content_type, "application/octet-stream");
    assert_eq!(
        r.raw,
        Content::Inline {
            inline: "not json".into(),
            encoding: chaldea_core::payload::Encoding::Utf8
        }
    );
    let bytes = Hook::new(&l, b"\xff\xfe{");
    ok(&h.post(&bytes).await);
    let r = h.assert_raw(&l, bytes.emit, R::Unparseable);
    assert_eq!(
        r.raw,
        Content::Inline {
            inline: "//57".into(),
            encoding: chaldea_core::payload::Encoding::Base64
        }
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schema_mismatch_kept_verbatim() {
    let h = server().await;
    let l = h.codex(1).await;
    let x = Hook::new(&l, br#"{"x":1}"#);
    ok(&h.post(&x).await);
    let r = h.assert_raw(&l, x.emit, R::SchemaMismatch);
    assert_eq!(r.content_type, "application/json");
    assert_eq!(
        r.raw,
        Content::Inline {
            inline: r#"{"x":1}"#.into(),
            encoding: chaldea_core::payload::Encoding::Utf8
        }
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_raw_is_a_blob() {
    let h = server().await;
    let l = h.codex(1).await;
    let body = vec![b'z'; 200 * 1024];
    let x = Hook::new(&l, &body);
    ok(&h.post(&x).await);
    let r = h.assert_raw(&l, x.emit, R::Unparseable);
    let Content::Blob { blob, .. } = &r.raw else {
        panic!("{:?}", r.raw);
    };
    assert_eq!(blob.size, 200 * 1024);
    let (stream, sha) = (StreamId::Thread(l.thread), blob.sha256.clone());
    let stored = h.srv.store.read(move |tx| tx.blob(stream, &sha)).unwrap();
    assert_eq!(stored.data, body);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lone_surrogate_is_unparseable() {
    let h = server().await;
    let l = h.codex(1).await;
    let body = br#"{"hook_event_name":"Stop","session_id":"s","x":"\uD800"}"#;
    let unbound = Hook::new(&l, body);
    ok(&h.post(&unbound).await);
    h.assert_raw(&l, unbound.emit, R::Unparseable);
    bind_codex(&h, &l, "s").await;
    let bound = Hook::new(&l, body);
    ok(&h.post(&bound).await);
    h.assert_raw(&l, bound.emit, R::Unparseable);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_buffer_then_valid_bind() {
    let h = server().await;
    let l = h.codex(1).await;
    let bad_body = buffer_directly(
        &h,
        &l,
        100,
        br#"{"hook_event_name":"Stop","session_id":"s","x":"\uD800"}"#.to_vec(),
        meta(&l, L_START),
    )
    .await;
    let bad_meta = buffer_directly(&h, &l, 101, payload("Stop", "s"), r#"{"v":2}"#.into()).await;
    let good = buffer_directly(&h, &l, 102, payload("Stop", "s"), meta(&l, L_START)).await;
    bind_codex(&h, &l, "s").await;
    h.assert_raw(&l, bad_body, R::Unparseable);
    h.assert_raw(&l, bad_meta, R::SchemaMismatch);
    assert!(matches!(
        h.delivery(l.launch, good).unwrap().routing,
        Routing::Appended { .. }
    ));
    h.stop().await;
}

// ---- PR9-CL-27, CL-28: tokens and keys ---------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_token_checks() {
    let h = server().await;
    let l = h.codex(1).await;
    let other = h.codex(2).await;
    let srv = &h.srv;
    let snapshot = h.snapshot(&l);
    let no_launch = srv
        .token(chaldea_core::auth::TokenKind::Hook, Some(l.thread), None)
        .await;
    let wrong_thread = srv
        .token(
            chaldea_core::auth::TokenKind::Hook,
            Some(l.thread),
            Some(other.launch),
        )
        .await;
    let revoked = srv
        .token(
            chaldea_core::auth::TokenKind::Hook,
            Some(l.thread),
            Some(l.launch),
        )
        .await;
    srv.revoke_quietly(&revoked).await;
    for token in [no_launch, wrong_thread, revoked] {
        let mut x = Hook::of(&l, "SessionStart", "s");
        x.token = token;
        assert_eq!(
            h.post(&x).await.outcome(),
            (401, Some("unauthorized".into()))
        );
    }
    assert_eq!(h.snapshot(&l), snapshot);
    // The control: the scoped token passes.
    bind_codex(&h, &l, "s").await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emit_id_reused_by_another_launch() {
    let h = server().await;
    let l = h.codex(1).await;
    // An unparseable body: an event under `hook:<emit>` in the thread at once.
    let first = Hook::new(&l, b"not json");
    ok(&h.post(&first).await);
    // The thread's next launch, its own token, the same emit id.
    let (t, l2) = (l.thread, launch(2));
    h.srv
        .append(exit(t, l.launch, &h.thread_row(t).state))
        .await;
    let state = h.thread_row(t).state;
    h.srv
        .append(reserve_and_start(t, l2, Agent::Codex, &state, true, true))
        .await;
    let token = h
        .srv
        .token(chaldea_core::auth::TokenKind::Hook, Some(t), Some(l2))
        .await;
    let next = Launch {
        launch: l2,
        token,
        ..l.clone()
    };
    let before = h.snapshot(&next);
    let reuse = Hook::of(&next, "Stop", "s").emit(first.emit);
    assert_eq!(
        h.post(&reuse).await.outcome(),
        (400, Some("bad_request".into()))
    );
    assert_eq!(h.snapshot(&next), before);
    assert!(h.delivery(l2, first.emit).is_none());
    h.stop().await;
}

/// `l`'s thread gets launch `n` (Codex, started) after `l` exits; `l`'s token stays valid.
async fn next_launch(h: &HookServer, l: &Launch, n: u128) -> Launch {
    let t = l.thread;
    h.srv
        .append(exit(t, l.launch, &h.thread_row(t).state))
        .await;
    let state = h.thread_row(t).state;
    h.srv
        .append(reserve_and_start(
            t,
            launch(n),
            Agent::Codex,
            &state,
            true,
            true,
        ))
        .await;
    let token = h
        .srv
        .token(
            chaldea_core::auth::TokenKind::Hook,
            Some(t),
            Some(launch(n)),
        )
        .await;
    Launch {
        launch: launch(n),
        token,
        ..l.clone()
    }
}

/// An emit id buffered by the thread's current launch (no event under its key yet) is
/// still taken: the earlier launch reusing it gets 400 and stores nothing, and the current
/// launch's buffer then binds, or resolves at its exit, without a key conflict.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emit_id_reused_while_other_launch_buffered() {
    let h = server().await;
    for (n, resolve) in [(1, false), (3, true)] {
        let l1 = h.codex(n).await;
        let l2 = next_launch(&h, &l1, n + 1).await;
        let buffered = Hook::of(&l2, "UserPromptSubmit", "s");
        ok(&h.post(&buffered).await);
        assert_eq!(
            h.delivery(l2.launch, buffered.emit).unwrap().routing,
            Routing::Buffered
        );
        let before = h.snapshot(&l1);
        let reuse = Hook::of(&l1, "Stop", "s").emit(buffered.emit);
        assert_eq!(
            h.post(&reuse).await.outcome(),
            (400, Some("bad_request".into()))
        );
        assert_eq!(h.snapshot(&l1), before);
        assert!(h.delivery(l1.launch, buffered.emit).is_none());
        if resolve {
            let lid = l2.launch;
            let n = h.run(move |tx| Ok(resolve_unbindable(tx, lid)?)).await;
            assert_eq!(n, 1);
            h.assert_raw(&l2, buffered.emit, R::Unbound);
        } else {
            bind_codex(&h, &l2, "s").await;
            assert!(matches!(
                h.delivery(l2.launch, buffered.emit).unwrap().routing,
                Routing::Appended { .. } | Routing::RawUnknown { .. }
            ));
        }
    }
    h.stop().await;
}

// ---- PR9-CL-30: retries ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_after_uncommitted_first_is_unreliable() {
    let h = server().await;
    let l = h.codex(1).await;
    bind_codex(&h, &l, "s").await;
    // Attempt 1 is cut before its job: its client leaves during a held walk.
    h.chains.set(7001, direct(7001, "codex"));
    let (walking, release) = h.chains.hold(7001);
    let first = Hook::of(&l, "Stop", "s").pid(Some(7001));
    let mut conn = support::http::connect(h.srv.port).await;
    support::http::write(&mut conn, &first.req(&h.srv).bytes()).await;
    tokio::task::spawn_blocking(move || walking.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .unwrap();
    drop(conn);
    let retry = first.clone().attempt(Some("2")).pid(Some(EMITTER_PID));
    ok(&h.post(&retry).await);
    drop(release);
    let calls: Vec<_> = h
        .adapter
        .calls()
        .into_iter()
        .filter(|c| c.emit_id == first.emit)
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].attempt, Some(2));
    assert!(!calls[0].intake_reliable);
    // Probe calls: binding (1), attempt 1 (2), the retry (3).
    assert_eq!(calls[0].intake.as_ref().unwrap().0["n"].as_u64(), Some(3));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_after_lost_reply_replays() {
    let h = server().await;
    let l = h.codex(1).await;
    bind_codex(&h, &l, "s").await;
    let first = Hook::of(&l, "Stop", "s");
    // The client never reads its reply: it closes once attempt 1 committed.
    let mut conn = support::http::connect(h.srv.port).await;
    support::http::write(&mut conn, &first.req(&h.srv).bytes()).await;
    let lid = l.launch;
    let emit = first.emit;
    let store = h.srv.store.store().clone();
    support::http::eventually(Duration::from_secs(10), "attempt 1 committed", || {
        store
            .read(soon(), move |tx| tx.hook_delivery(lid, emit))
            .unwrap()
            .is_some()
    })
    .await;
    drop(conn);
    ok(&h.post(&first.clone().attempt(Some("2"))).await);
    let calls: Vec<_> = h
        .adapter
        .calls()
        .into_iter()
        .filter(|c| c.emit_id == first.emit)
        .collect();
    assert_eq!(calls.len(), 1, "one adapter call");
    assert_eq!(calls[0].attempt, Some(1));
    assert!(calls[0].intake_reliable);
    h.stop().await;
}
