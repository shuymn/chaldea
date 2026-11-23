//! The body boundary and the deadline origin (design §2.2; PR7-CL-6, CL-8's guard case,
//! CL-22's before-body cases): the poll-counting request body records no poll by the
//! application for any rejection.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Response};
use chaldea_core::auth::TokenKind;
use chaldea_core::ids::{LaunchId, ThreadId};
use tokio::io::AsyncWriteExt;

use super::shared;
use super::support::http::{DEADLINE, Req, TestServer, connect, parse, read_to_end};
use crate::http::{Mount, Mounts, WriteCtx, spec};

/// A named request and its expected status and code.
type Case = (&'static str, Req, (u16, Option<String>));

fn code(s: &str) -> Option<String> {
    Some(s.to_owned())
}

/// `/hook` with a handler that reads the body (the positive control).
fn reading_hook() -> Mounts {
    Mounts::mvp().with(Mount::write(
        spec(&Method::POST, "/hook").unwrap(),
        |ctx: WriteCtx, req: axum::http::Request<Body>| async move {
            ctx.body_within(req.into_body(), 1024).await?;
            Ok(Response::new(Body::from("read")))
        },
    ))
}

/// One request per rejection step, each with a 10-byte body, and its decided reply.
async fn cases(srv: &TestServer) -> Vec<Case> {
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let hook = srv.token_of(TokenKind::Hook, 1).await;
    let agent = srv
        .agent(ThreadId::from_u128(1), LaunchId::from_u128(2))
        .await;
    let body = b"0123456789";
    let post = |path: &str| srv.req("POST", path).body(body);
    vec![
        (
            "host",
            post("/hook").bearer(&hook).host(Some("evil:1")),
            (421, code("misdirected_request")),
        ),
        (
            "method",
            srv.req("PUT", "/hook")
                .bearer(&hook)
                .header("Content-Length", "10")
                .body(body),
            (405, code("method_not_allowed")),
        ),
        (
            "origin",
            post("/hook").bearer(&hook).header("Origin", "http://evil"),
            (403, code("origin_rejected")),
        ),
        ("credential", post("/hook"), (401, code("unauthorized"))),
        (
            "kind",
            post("/hook").bearer(&owner),
            (403, code("forbidden")),
        ),
        (
            "action",
            post("/api/commands/key").bearer(&agent),
            (403, code("forbidden")),
        ),
        (
            "unknown command",
            post("/api/commands/bogus").bearer(&owner),
            (404, code("not_found")),
        ),
        (
            "framing",
            srv.req("POST", "/hook")
                .bearer(&hook)
                .header("Transfer-Encoding", "chunked")
                .no_length()
                .body(b"a\r\n0123456789\r\n0\r\n\r\n"),
            (400, code("bad_request")),
        ),
        (
            "size",
            srv.req("POST", "/hook")
                .bearer(&hook)
                .no_length()
                .header("Content-Length", "5000000")
                .body(body),
            (413, code("payload_too_large")),
        ),
        (
            "unimplemented",
            post("/api/commands/spawn").bearer(&owner),
            (501, code("not_implemented")),
        ),
    ]
}

async fn run(withhold_body: bool) {
    let srv = TestServer::start(reading_hook()).await;
    let s = shared(&srv);
    for (step, req, want) in cases(&srv).await {
        let before = s.faults.polls();
        let mut c = connect(srv.port).await;
        let bytes = if withhold_body {
            req.head()
        } else {
            req.bytes()
        };
        c.write_all(&bytes).await.unwrap();
        let (out, end) = read_to_end(&mut c).await;
        let r = parse(&out, end);
        assert_eq!(r.outcome(), want, "{step}: {r:?}");
        assert_eq!(
            s.faults.polls(),
            before,
            "{step}: the application polled the body"
        );
    }
    // Positive control: a handler that reads the body polls it.
    let hook = srv.token_of(TokenKind::Hook, 1).await;
    let before = s.faults.polls();
    let r = srv
        .send(&srv.req("POST", "/hook").bearer(&hook).body(b"{}"))
        .await;
    assert_eq!(r.status, 200);
    assert!(s.faults.polls() > before, "the counter saw no poll");
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejections_precede_body() {
    run(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejections_with_body_in_flight() {
    run(false).await;
}

/// A head trickled over 1.4 s, then the token lookup held at a fault point: the reply comes
/// by the deadline from the first head byte, never later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_head_then_slow_lookup() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    s.faults.arm_before_read(0, move || {
        let _ = release_rx.recv_timeout(Duration::from_secs(20));
    });
    let head = srv.req("GET", "/api/threads").bearer(&owner).head();
    let mut c = connect(srv.port).await;
    let first_byte = Instant::now();
    for chunk in head.chunks(head.len().div_ceil(7)) {
        c.write_all(chunk).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let (out, end) = read_to_end(&mut c).await;
    let took = first_byte.elapsed();
    drop(release_tx);
    assert_eq!(parse(&out, end).outcome(), (408, code("request_timeout")));
    assert!(
        took >= DEADLINE - Duration::from_millis(50)
            && took < DEADLINE + Duration::from_millis(900),
        "answered {took:?} after the first head byte; deadline {DEADLINE:?}"
    );
    srv.stop().await;
}
