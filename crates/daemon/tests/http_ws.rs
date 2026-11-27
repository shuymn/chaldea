//! Tickets and the `GET /api/ws` loop (`docs/design/http-surface.md` PR7-CL-14, CL-16,
//! CL-17, and CL-12's subscription case). The cases that need fault points are unit tests
//! in `src/http/tests/ws.rs`.

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::Method;
use chaldea_core::auth::TokenKind;
use chaldea_core::ids::{LaunchId, StreamId, ThreadId};
use chaldea_core::payload::Content;
use chaldea_core::sync::WsServerMessage;
use chaldea_daemon::http::{HttpConfig, Mount, Mounts, UpgradedSocket, spec};
use chaldea_daemon::store::{NewEvent, TokenSelector};
use futures_util::StreamExt;
use proptest::prelude::*;
use support::http::{
    DEADLINE, HEADS_INTERVAL, REQUEST_BUDGET, TestServer, WS_WAIT, eventually, head_of,
};
use support::waits;
use support::{raw_unknown, registered, registered_under, reserved, started};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocket};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn code(s: &str) -> Option<String> {
    Some(s.to_owned())
}

/// A server whose heads refresh is far off, so every frame a test sees follows from what it
/// did.
async fn quiet_with(mounts: Mounts, tune: impl FnOnce(&mut HttpConfig)) -> TestServer {
    TestServer::start_with(mounts, |c: &mut HttpConfig| {
        c.ws.heads_interval = Duration::from_secs(600);
        tune(c);
    })
    .await
}

async fn quiet() -> TestServer {
    quiet_with(Mounts::mvp(), |_| {}).await
}

async fn thread(srv: &TestServer, n: u128) -> StreamId {
    let t = ThreadId::from_u128(0x5000 + n);
    srv.append(vec![registered(t, false)]).await;
    StreamId::Thread(t)
}

fn one(stream: StreamId, key: &str) -> NewEvent {
    raw_unknown(stream, key, Content::text("x"))
}

fn head(srv: &TestServer, s: StreamId) -> u64 {
    srv.store.read(move |tx| tx.head(s, 0)).head_seq
}

/// Opens `path` with `ticket` through the client library: the socket, or the refusal's
/// HTTP status.
async fn open(srv: &TestServer, path: &str, ticket: &str) -> Result<Socket, u16> {
    let url = format!("ws://127.0.0.1:{}{path}?ticket={ticket}", srv.port);
    match tokio_tungstenite::connect_async(url).await {
        Ok((ws, resp)) => {
            assert_eq!(resp.status(), 101);
            Ok(ws)
        }
        Err(WsError::Http(r)) => Err(r.status().as_u16()),
        Err(e) => panic!("not an HTTP refusal: {e}"),
    }
}

async fn refused(srv: &TestServer, ticket: &str) -> u16 {
    open(srv, "/api/ws", ticket).await.expect_err("refused")
}

/// The first text frame of `ws`, within the wait.
async fn first_text(ws: &mut Socket) -> String {
    let m = waits::bounded(WS_WAIT, ws.next())
        .await
        .expect("a frame within the wait")
        .expect("open")
        .unwrap();
    m.to_text().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_then_heads() {
    let srv = quiet().await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let t = thread(&srv, 1).await;
    srv.append(vec![one(t, "a"), one(t, "b")]).await;
    // `ws` checks that `hello` comes first and carries the store epoch.
    let mut ws = srv.ws(&owner).await;
    ws.subscribe(&[StreamId::Host, t]).await;
    let heads = ws.heads_for(&[StreamId::Host, t], |_| {}).await;
    assert_eq!(head_of(&heads, t), Some(head(&srv, t)));
    assert_eq!(
        head_of(&heads, StreamId::Host),
        Some(head(&srv, StreamId::Host))
    );
    // A resubscribe is answered with a `heads` frame for the new set.
    ws.subscribe(&[t]).await;
    match ws.next().await.expect("open") {
        WsServerMessage::Heads { heads } => {
            assert_eq!(heads.len(), 1);
            assert_eq!(head_of(&heads, t), Some(head(&srv, t)));
        }
        other => panic!("unexpected {other:?}"),
    }
    srv.stop().await;
}

/// A `heads` frame omits streams that do not exist: the subscribe's frame and every
/// periodic one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heads_omit_nonexistent_streams() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let t = thread(&srv, 1).await;
    let missing = StreamId::Thread(ThreadId::from_u128(0x5FFF));
    let mut ws = srv.ws(&owner).await;
    ws.subscribe(&[StreamId::Host, t, missing]).await;
    let mut frames = 0;
    while frames < 3 {
        match ws.next().await.expect("open") {
            WsServerMessage::Heads { heads } => {
                let mut got: Vec<StreamId> = heads.iter().map(|h| h.stream_id).collect();
                got.sort();
                assert_eq!(got, [StreamId::Host, t], "{heads:?}");
                frames += 1;
            }
            WsServerMessage::StreamAdvanced { stream_id, .. } => {
                assert_ne!(stream_id, missing);
            }
            WsServerMessage::Hello { .. } => panic!("a second hello"),
        }
    }
    srv.stop().await;
}

/// A commit on a subscribed stream is announced; no frame names an unsubscribed stream or a
/// head not newer than one announced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_is_announced() {
    let srv = quiet().await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let t = thread(&srv, 1).await;
    let other = thread(&srv, 2).await;
    let mut ws = srv.ws(&owner).await;
    ws.subscribe(&[t]).await;
    let heads = ws.heads_for(&[t], |_| {}).await;
    let mut announced = head_of(&heads, t).unwrap();
    for i in 0..5 {
        srv.append(vec![one(other, &format!("o{i}"))]).await;
        srv.append(vec![one(t, &format!("t{i}"))]).await;
        let want = head(&srv, t);
        while announced < want {
            match ws.next().await.expect("open") {
                WsServerMessage::StreamAdvanced {
                    stream_id,
                    head_seq,
                } => {
                    assert_eq!(stream_id, t, "only the subscribed stream");
                    assert!(head_seq > announced, "{head_seq} after {announced}");
                    announced = head_seq;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(announced, want);
    }
    srv.stop().await;
}

/// Every periodic `heads` lists every subscribed stream, and carries the current heads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn periodic_heads_list_every_stream() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let a = thread(&srv, 1).await;
    let b = thread(&srv, 2).await;
    srv.append(vec![one(a, "a1")]).await;
    let set = [StreamId::Host, a, b];
    let mut ws = srv.ws(&owner).await;
    ws.subscribe(&set).await;
    ws.heads_for(&set, |_| {}).await;
    for i in 0..3 {
        srv.append(vec![one(b, &format!("b{i}"))]).await;
        let want: Vec<u64> = set.iter().map(|s| head(&srv, *s)).collect();
        // Every `heads` frame lists the whole set (`next` panics past the wait).
        loop {
            match ws.next().await.expect("open") {
                WsServerMessage::Heads { heads } => {
                    assert_eq!(heads.len(), set.len(), "{heads:?}");
                    let got: Vec<u64> = set.iter().map(|s| head_of(&heads, *s).unwrap()).collect();
                    if got == want {
                        break;
                    }
                }
                WsServerMessage::StreamAdvanced { .. } => {}
                WsServerMessage::Hello { .. } => panic!("a second hello"),
            }
        }
    }
    srv.stop().await;
}

/// An agent's subscription keeps only the streams it may read: not the host stream, a
/// parent, or a sibling; commits on those are never announced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_drops_unreadable() {
    let srv = quiet().await;
    let id = |n: u128| ThreadId::from_u128(0x5100 + n);
    let (parent, me, child, sibling) = (id(1), id(2), id(3), id(4));
    srv.append(vec![
        registered_under(parent, None),
        registered_under(me, Some(parent)),
        registered_under(child, Some(me)),
        registered_under(sibling, Some(parent)),
    ])
    .await;
    let l = LaunchId::from_u128(0x5200);
    srv.append(vec![reserved(me, l), started(me, l)]).await;
    let agent = srv.token(TokenKind::Agent, Some(me), Some(l)).await;
    let mut ws = srv.ws(&agent).await;
    let all: Vec<StreamId> = std::iter::once(StreamId::Host)
        .chain([parent, me, child, sibling].map(StreamId::Thread))
        .collect();
    ws.subscribe(&all).await;
    match ws.next().await.expect("open") {
        WsServerMessage::Heads { heads } => {
            let mut got: Vec<StreamId> = heads.iter().map(|h| h.stream_id).collect();
            got.sort();
            assert_eq!(got, [StreamId::Thread(me), StreamId::Thread(child)]);
        }
        other => panic!("unexpected {other:?}"),
    }
    srv.append(vec![one(StreamId::Thread(sibling), "s")]).await;
    srv.append(vec![one(StreamId::Thread(parent), "p")]).await;
    srv.append(vec![one(StreamId::Thread(me), "m")]).await;
    match ws.next().await.expect("open") {
        WsServerMessage::StreamAdvanced { stream_id, .. } => {
            assert_eq!(stream_id, StreamId::Thread(me));
        }
        other => panic!("unexpected {other:?}"),
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_single_use() {
    let srv = quiet().await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = srv.ticket(&owner, "events").await.unwrap();
    let mut ws = srv.ws_with(&ticket).await.unwrap();
    assert!(matches!(
        ws.next().await,
        Some(WsServerMessage::Hello { .. })
    ));
    assert_eq!(refused(&srv, &ticket).await, 401);
    // A ticket of the right form that was never minted.
    assert_eq!(
        refused(&srv, &format!("chdt_{}", "n".repeat(43))).await,
        401
    );
    srv.stop().await;
}

/// Redeemable before its expiry (29.999 s after minting), not at it (30 s).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_expiry_boundary() {
    let srv = quiet().await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let f = Instant::now();
    srv.clock.freeze(f);
    let early = srv.ticket(&owner, "events").await.unwrap();
    let late = srv.ticket(&owner, "events").await.unwrap();
    srv.clock.freeze(f + Duration::from_millis(29_999));
    let mut ws = srv.ws_with(&early).await.unwrap();
    assert!(matches!(
        ws.next().await,
        Some(WsServerMessage::Hello { .. })
    ));
    srv.clock.freeze(f + Duration::from_secs(30));
    assert_eq!(refused(&srv, &late).await, 401);
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_dies_with_token() {
    let srv = quiet().await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = srv.ticket(&owner, "events").await.unwrap();
    srv.revoke(&owner).await;
    assert_eq!(refused(&srv, &ticket).await, 401);
    srv.stop().await;
}

/// A test-only attach mount: the shared admission, then one text frame, then holds the
/// socket.
fn attach_mounts() -> Mounts {
    Mounts::mvp().with(Mount::upgrade(
        spec(&Method::GET, "/api/attach/{thread}").unwrap(),
        |mut s: UpgradedSocket| async move {
            let attached = axum::extract::ws::Message::Text(r#"{"type":"attached"}"#.into());
            if s.socket.send(attached).await.is_err() {
                return;
            }
            loop {
                tokio::select! {
                    () = s.shutdown.wait() => return,
                    m = s.socket.recv() => if !matches!(m, Some(Ok(_))) { return },
                }
            }
        },
    ))
}

async fn attached(srv: &TestServer, t: ThreadId, ticket: &str) {
    let mut ws = open(srv, &format!("/api/attach/{t}"), ticket)
        .await
        .expect("a 101");
    assert_eq!(first_text(&mut ws).await, r#"{"type":"attached"}"#);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_purpose_mismatch() {
    let srv = quiet_with(attach_mounts(), |_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let StreamId::Thread(t) = thread(&srv, 1).await else {
        unreachable!()
    };
    let StreamId::Thread(other) = thread(&srv, 2).await else {
        unreachable!()
    };
    let events = srv.ticket(&owner, "events").await.unwrap();
    let attach = srv.ticket(&owner, &format!("attach:{t}")).await.unwrap();
    for (path, ticket) in [
        (format!("/api/attach/{t}"), &events),
        ("/api/ws".to_owned(), &attach),
        (format!("/api/attach/{other}"), &attach),
    ] {
        assert_eq!(open(&srv, &path, ticket).await.err(), Some(401), "{path}");
    }
    // Neither was spent.
    attached(&srv, t, &attach).await;
    let mut ws = srv.ws_with(&events).await.unwrap();
    assert!(matches!(
        ws.next().await,
        Some(WsServerMessage::Hello { .. })
    ));
    // Purposes other than `events` and `attach:<existing thread>`.
    for (purpose, want) in [
        ("attach:nope", (400, code("bad_request"))),
        ("attach:", (400, code("bad_request"))),
        ("pair", (400, code("bad_request"))),
        ("Events", (400, code("bad_request"))),
        (
            "attach:00000000-0000-0000-0000-00000000dead",
            (404, code("not_found")),
        ),
    ] {
        let r = srv.ticket(&owner, purpose).await.expect_err("refused");
        assert_eq!(r.outcome(), want, "{purpose}");
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_ticket_owner_only() {
    let srv = quiet_with(attach_mounts(), |_| {}).await;
    let me = ThreadId::from_u128(0x5301);
    let agent = srv.agent(me, LaunchId::from_u128(0x5302)).await;
    let rescue = srv.token(TokenKind::Rescue, None, None).await;
    for tok in [&agent, &rescue] {
        let r = srv
            .ticket(tok, &format!("attach:{me}"))
            .await
            .expect_err("refused");
        assert_eq!(r.outcome(), (403, code("forbidden")));
    }
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let attach = srv.ticket(&owner, &format!("attach:{me}")).await.unwrap();
    attached(&srv, me, &attach).await;
    srv.stop().await;
}

/// A request refused for its credential lookup, its handshake, its framing, or a full
/// socket cap does not spend a valid ticket (the final admission lookup's case is the unit
/// test `ws::final_admission_failure_keeps_ticket`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_upgrade_keeps_ticket() {
    // The pool's wait ends at the short budget.
    let srv = quiet_with(Mounts::mvp(), |c| {
        c.ws.max_sockets_per_token = 1;
        c.short_deadline = DEADLINE;
    })
    .await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = srv.ticket(&owner, "events").await.unwrap();
    let q = format!("?ticket={ticket}");
    // The credential lookup with the reader pool exhausted through the cutoff.
    let release = srv.hold_readers(4);
    // The pool's wait and the request's cutoff end at the same deadline: 503 or 408.
    let r = srv.send(&srv.ws_handshake(&format!("/api/ws{q}"))).await;
    assert!(
        [(503, code("busy")), (408, code("request_timeout"))].contains(&r.outcome()),
        "{r:?}"
    );
    drop(release);
    // No `Upgrade` header; a body on the GET.
    let no_upgrade = srv
        .req("GET", &format!("/api/ws{q}"))
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", &support::http::ws_key());
    assert_eq!(
        srv.send(&no_upgrade).await.outcome(),
        (400, code("bad_request"))
    );
    let with_body = srv
        .ws_handshake(&format!("/api/ws{q}"))
        .header("Content-Length", "2")
        .body(b"{}");
    assert_eq!(
        srv.send(&with_body).await.outcome(),
        (400, code("bad_request"))
    );
    // The token's one socket taken: 503 `busy`.
    let mut first = srv.ws(&owner).await;
    assert_eq!(
        srv.send(&srv.ws_handshake(&format!("/api/ws{q}")))
            .await
            .outcome(),
        (503, code("busy"))
    );
    // The slot frees when the first socket's task ends.
    first.ws.close(None).await.unwrap();
    while first.next().await.is_some() {}
    let until = Instant::now() + Duration::from_secs(10);
    let mut ws = loop {
        match open(&srv, "/api/ws", &ticket).await {
            Ok(ws) => break ws,
            Err(status) => {
                assert_eq!(status, 503);
                assert!(Instant::now() < until, "the slot never freed");
                tokio::task::yield_now().await;
            }
        }
    };
    assert!(first_text(&mut ws).await.contains("\"hello\""));
    srv.stop().await;
}

/// Of 16 concurrent valid redemptions of one ticket, exactly one succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_redemption_once() {
    let srv = Arc::new(quiet().await);
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = srv.ticket(&owner, "events").await.unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(16));
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let (srv, barrier, ticket) = (srv.clone(), barrier.clone(), ticket.clone());
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            open(&srv, "/api/ws", &ticket).await
        }));
    }
    let mut ok = Vec::new();
    for t in tasks {
        match t.await.unwrap() {
            Ok(ws) => ok.push(ws),
            Err(status) => assert_eq!(status, 401),
        }
    }
    assert_eq!(ok.len(), 1);
    drop(ok);
    Arc::into_inner(srv).unwrap().stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_query_rules() {
    let srv = quiet().await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = srv.ticket(&owner, "events").await.unwrap();
    for (q, want) in [
        (String::new(), (401, code("unauthorized"))),
        ("?ticket=".to_owned(), (401, code("unauthorized"))),
        (
            format!("?ticket={ticket}&ticket={ticket}"),
            (401, code("unauthorized")),
        ),
        (
            format!("?ticket={ticket}&stream_id=host"),
            (400, code("bad_query")),
        ),
        (format!("?x=1&ticket={ticket}"), (400, code("bad_query"))),
    ] {
        let r = srv.send(&srv.ws_handshake(&format!("/api/ws{q}"))).await;
        assert_eq!(r.outcome(), want, "{q}");
    }
    // None of those spent the ticket.
    let mut ws = srv.ws_with(&ticket).await.unwrap();
    assert!(matches!(
        ws.next().await,
        Some(WsServerMessage::Hello { .. })
    ));
    srv.stop().await;
}

/// Only head numbers come before the close: every frame names heads, never content.
fn only_heads(before: &[WsServerMessage]) {
    for m in before {
        assert!(
            matches!(
                m,
                WsServerMessage::Heads { .. } | WsServerMessage::StreamAdvanced { .. }
            ),
            "{m:?}"
        );
    }
}

/// A revoked token's socket closes with 1008: on the nudge, and without it within the
/// heads interval plus a read deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocation_closes_socket() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let t = thread(&srv, 1).await;
    // With the nudge.
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let mut ws = srv.ws(&owner).await;
    ws.subscribe(&[t]).await;
    ws.heads_for(&[t], |_| {}).await;
    srv.revoke(&owner).await;
    let (code, before) = ws.until_closed().await;
    assert_eq!(code, Some(1008));
    only_heads(&before);
    // Without it: the next refresh read finds the token revoked; a commit in the window is
    // announced at most as a head number.
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let mut ws = srv.ws(&owner).await;
    ws.subscribe(&[t]).await;
    ws.heads_for(&[t], |_| {}).await;
    let at = Instant::now();
    srv.revoke_quietly(&owner).await;
    srv.append(vec![one(t, "window")]).await;
    let (code, before) = ws.until_closed().await;
    assert_eq!(code, Some(1008));
    only_heads(&before);
    let bound = HEADS_INTERVAL + REQUEST_BUDGET;
    assert!(at.elapsed() < bound, "{:?} past {bound:?}", at.elapsed());
    srv.stop().await;
}

/// The revoking caller leaves after its job is submitted and before it commits; the commit
/// still nudges, and the socket closes long before any refresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoke_after_waiter_left_still_nudges() {
    let srv = Arc::new(quiet().await);
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let mut ws = srv.ws(&owner).await;
    let id = srv.token_row(&owner).unwrap().token_id;
    let hold = srv.hold_writer().await;
    // A connection task can outlive its reply: the count is read once only the accept loop
    // and the socket remain (2), so no task leaves while the observer registers.
    eventually(
        WS_WAIT,
        "only the accept loop and the socket remain",
        || srv.server.tasks() == 2,
    )
    .await;
    let revoking = {
        let srv = srv.clone();
        tokio::spawn(async move {
            srv.server
                .tokens()
                .revoke(
                    TokenSelector::Token(id),
                    Instant::now() + Duration::from_secs(10),
                )
                .await
        })
    };
    // The write observer is registered once the job is submitted; the caller then leaves.
    eventually(WS_WAIT, "the write observer registered", || {
        srv.server.tasks() > 2
    })
    .await;
    revoking.abort();
    assert!(revoking.await.unwrap_err().is_cancelled());
    drop(hold);
    let (code, _) = ws.until_closed().await;
    assert_eq!(
        code,
        Some(1008),
        "closed on the nudge, not by a refresh 600 s away"
    );
    Arc::into_inner(srv).unwrap().stop().await;
}

/// A socket with no inbound frame (not even a pong) for `idle_timeout` is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_socket_dropped() {
    let srv = TestServer::start_with(Mounts::mvp(), |c| {
        c.ws.idle_timeout = Duration::from_millis(500);
    })
    .await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = srv.ticket(&owner, "events").await.unwrap();
    // A raw client reads the connection to its end and answers no ping.
    let r = srv
        .send(&srv.ws_handshake(&format!("/api/ws?ticket={ticket}")))
        .await;
    assert_eq!(r.status, 101, "{r:?}");
    // Dropped, not closed: the frames sent before it, then the end without a close frame.
    let frames = server_frames(&r.body);
    assert!(
        matches!(
            frames.first(),
            Some(WsMessage::Text(t))
                if matches!(serde_json::from_str(t.as_str()), Ok(WsServerMessage::Hello { .. }))
        ),
        "{frames:?}"
    );
    assert!(
        !frames.iter().any(|m| matches!(m, WsMessage::Close(_))),
        "{frames:?}"
    );
    srv.stop().await;
}

/// The messages of the server's frames in `bytes`, up to their end.
fn server_frames(bytes: &[u8]) -> Vec<WsMessage> {
    /// Reads `bytes`; drops what the client would write (the pongs).
    struct Replay<'a>(&'a [u8]);
    impl std::io::Read for Replay<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::io::Read::read(&mut self.0, buf)
        }
    }
    impl std::io::Write for Replay<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut ws = WebSocket::from_raw_socket(Replay(bytes), Role::Client, None);
    std::iter::from_fn(|| ws.read().ok()).collect()
}

// ---- subscribes racing commits ------------------------------------------------------------

#[derive(Debug, Clone)]
enum Step {
    Subscribe(Vec<usize>),
    Commit(usize, usize),
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    prop::collection::vec(
        prop_oneof![
            prop::collection::vec(0usize..3, 0..4).prop_map(Step::Subscribe),
            (0usize..3, 1usize..3).prop_map(|(s, n)| Step::Commit(s, n)),
        ],
        1..12,
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

    /// Subscribes and commits interleaved with the socket's work: the heads announced for a
    /// stream never decrease, a `stream_advanced` is newer than every head announced for its
    /// stream, and once commits stop and the client subscribes to every stream its heads
    /// become the store's.
    #[test]
    fn subscribe_racing_commits(steps in steps()) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let srv = quiet().await;
            let owner = srv.token(TokenKind::Owner, None, None).await;
            let mut streams = Vec::new();
            for n in 0..3 {
                streams.push(thread(&srv, n).await);
            }
            let mut ws = srv.ws(&owner).await;
            for (i, s) in steps.iter().enumerate() {
                match s {
                    Step::Subscribe(ix) => {
                        let ids: Vec<StreamId> = ix.iter().map(|i| streams[*i]).collect();
                        ws.subscribe(&ids).await;
                    }
                    Step::Commit(si, n) => {
                        for k in 0..*n {
                            srv.append(vec![one(streams[*si], &format!("c{i}:{k}"))]).await;
                        }
                    }
                }
            }
            ws.subscribe(&streams).await;
            let want: BTreeMap<StreamId, u64> =
                streams.iter().map(|s| (*s, head(&srv, *s))).collect();
            let mut seen: BTreeMap<StreamId, u64> = BTreeMap::new();
            let until = Instant::now() + Duration::from_secs(20);
            while seen != want {
                assert!(Instant::now() < until, "{seen:?} never became {want:?}");
                match ws.next().await.expect("open") {
                    WsServerMessage::Heads { heads } => {
                        for h in heads {
                            let last = seen.entry(h.stream_id).or_insert(0);
                            assert!(h.head_seq >= *last, "{} fell to {}", *last, h.head_seq);
                            *last = h.head_seq;
                        }
                    }
                    WsServerMessage::StreamAdvanced { stream_id, head_seq } => {
                        let last = seen.entry(stream_id).or_insert(0);
                        assert!(head_seq > *last, "advanced {head_seq} after {}", *last);
                        *last = head_seq;
                    }
                    WsServerMessage::Hello { .. } => panic!("a second hello"),
                }
            }
            srv.stop().await;
        });
    }
}
