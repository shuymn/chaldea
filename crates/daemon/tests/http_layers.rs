//! The outer layer, the guard order, framing, one request per connection, and deadlines
//! (`docs/design/http-surface.md` PR7-CL-1, CL-3, CL-6 `layer_order`, CL-7, CL-8).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Response};
use chaldea_core::auth::TokenKind;
use chaldea_core::ids::{LaunchId, ThreadId};
use chaldea_daemon::http::{ApiError, Mount, Mounts, ReadCtx, WriteCtx, spec};
use support::http::{DEADLINE, HEAD_TIMEOUT, Req, TestServer, connect, dump, parse, read_to_end};
use support::{exited, reserved};
use tokio::io::AsyncWriteExt;

/// A named request and its expected status and code.
type Case = (&'static str, Req, (u16, Option<String>));

fn code(s: &str) -> Option<String> {
    Some(s.to_owned())
}

fn ok_response() -> Response<Body> {
    Response::new(Body::from("ok"))
}

/// `GET /` mounted with a handler that counts its calls.
fn counting_root() -> (Mounts, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let mounts = Mounts::mvp().with(Mount::read(
        spec(&Method::GET, "/").unwrap(),
        move |_ctx: ReadCtx, _req| {
            c.fetch_add(1, Ordering::SeqCst);
            async { Ok(ok_response()) }
        },
    ));
    (mounts, calls)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_rejections() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let p = srv.port;
    let token = srv.token(TokenKind::Owner, None, None).await;
    let before = dump(&srv.store.path());
    for path in ["/api/threads", "/no/such/path"] {
        let base = || srv.req("GET", path).bearer(&token);
        let cases: Vec<(&str, Req)> = vec![
            ("missing", base().host(None)),
            ("repeated", base().header("Host", &format!("127.0.0.1:{p}"))),
            ("other host", base().host(Some(&format!("evil:{p}")))),
            (
                "other port",
                base().host(Some(&format!("127.0.0.1:{}", p + 1))),
            ),
            ("no port", base().host(Some("127.0.0.1"))),
            ("ipv6", base().host(Some(&format!("[::1]:{p}")))),
            (
                "absolute other host",
                Req::new(p, "GET", &format!("http://evil:{p}{path}")).bearer(&token),
            ),
            (
                "absolute other port",
                Req::new(p, "GET", &format!("http://127.0.0.1:{}{path}", p + 1)).bearer(&token),
            ),
            (
                "absolute userinfo",
                Req::new(p, "GET", &format!("http://user@127.0.0.1:{p}{path}")).bearer(&token),
            ),
            (
                "absolute localhost, Host 127.0.0.1",
                Req::new(p, "GET", &format!("http://localhost:{p}{path}")).bearer(&token),
            ),
            (
                "absolute 127.0.0.1, Host localhost",
                Req::new(p, "GET", &format!("http://127.0.0.1:{p}{path}"))
                    .host(Some(&format!("localhost:{p}")))
                    .bearer(&token),
            ),
        ];
        for (name, req) in cases {
            let r = srv.send(&req).await;
            assert_eq!(
                r.outcome(),
                (421, code("misdirected_request")),
                "{name} on {path}: {r:?}"
            );
        }
        // Positive controls: `localhost` (any case) and the absolute form with the own
        // authority pass the outer layer.
        let want = if path == "/api/threads" {
            (200, None)
        } else {
            (404, code("not_found"))
        };
        for req in [
            base().host(Some(&format!("LocalHost:{p}"))),
            Req::new(p, "GET", &format!("http://127.0.0.1:{p}{path}")).bearer(&token),
            Req::new(p, "GET", &format!("http://LOCALHOST:{p}{path}"))
                .host(Some(&format!("localhost:{p}")))
                .bearer(&token),
        ] {
            assert_eq!(srv.send(&req).await.outcome(), want, "{path}");
        }
    }
    assert!(
        dump(&srv.store.path()) == before,
        "a rejected request changed the store"
    );
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bearer_rejections() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let revoked = srv.token(TokenKind::Owner, None, None).await;
    srv.revoke(&revoked).await;
    // An agent token whose launch exited (the launch is recorded exited, the token is not
    // revoked), and one whose launch is not its thread's current launch.
    let (t1, l1) = (ThreadId::from_u128(1), LaunchId::from_u128(11));
    let exited_agent = srv.agent(t1, l1).await;
    srv.append(vec![exited(t1, l1)]).await;
    let (t2, l2) = (ThreadId::from_u128(2), LaunchId::from_u128(21));
    let _ = srv.agent(t2, l2).await;
    let stale_agent = srv
        .token(TokenKind::Agent, Some(t2), Some(LaunchId::from_u128(22)))
        .await;
    // An agent token whose launch exists and has not exited, but is another thread's
    // current launch: only the current-launch comparison refuses it.
    let (t4, l4) = (ThreadId::from_u128(4), LaunchId::from_u128(41));
    let _ = srv.agent(t4, l4).await;
    let foreign_launch_agent = srv.token(TokenKind::Agent, Some(t2), Some(l4)).await;
    let unknown = format!("chd_{}", "Q".repeat(43));
    let agent = srv
        .agent(ThreadId::from_u128(3), LaunchId::from_u128(31))
        .await;
    let before = dump(&srv.store.path());
    let get = || srv.req("GET", "/api/threads");
    let cases: Vec<(&str, Req)> = vec![
        ("none", get()),
        ("two headers", get().bearer(&owner).bearer(&owner)),
        (
            "other scheme",
            get().header("Authorization", &format!("Basic {owner}")),
        ),
        (
            "two spaces",
            get().header("Authorization", &format!("Bearer  {owner}")),
        ),
        (
            "tab",
            get().header("Authorization", &format!("Bearer\t{owner}")),
        ),
        (
            "trailing",
            get().header("Authorization", &format!("Bearer {owner} x")),
        ),
        (
            "extra char",
            get().header("Authorization", &format!("Bearer {owner}x")),
        ),
        ("scheme only", get().header("Authorization", "Bearer")),
        ("malformed", get().bearer("chd_short")),
        (
            "ticket form",
            get().bearer(&format!("chdt_{}", "Q".repeat(43))),
        ),
        ("unknown", get().bearer(&unknown)),
        ("revoked", get().bearer(&revoked)),
        ("agent_token_after_launch_exit", get().bearer(&exited_agent)),
        ("agent launch not current", get().bearer(&stale_agent)),
        (
            "agent launch live but not its thread's",
            get().bearer(&foreign_launch_agent),
        ),
    ];
    for (name, req) in cases {
        let r = srv.send(&req).await;
        assert_eq!(r.outcome(), (401, code("unauthorized")), "{name}: {r:?}");
        assert_eq!(r.header("www-authenticate"), Some("Bearer"), "{name}");
    }
    // Positive controls: the owner, `bearer` in any case, and a current agent reach the
    // handler.
    for req in [
        get().bearer(&owner),
        get().header("Authorization", &format!("bEaReR {owner}")),
        get().bearer(&agent),
    ] {
        assert_eq!(srv.send(&req).await.outcome(), (200, None));
    }
    // A rejected token changed nothing (the controls only read).
    assert!(
        dump(&srv.store.path()) == before,
        "a rejected request changed the store"
    );
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn layer_order() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let p = srv.port;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let hook = srv.token(TokenKind::Hook, None, None).await;
    let agent = srv
        .agent(ThreadId::from_u128(1), LaunchId::from_u128(2))
        .await;
    let big = (5 * 1024 * 1024).to_string();
    let cases: Vec<Case> = vec![
        (
            "host before method",
            Req::new(p, "PUT", "/hook").host(Some("evil:1")),
            (421, code("misdirected_request")),
        ),
        (
            "host before routing",
            Req::new(p, "GET", "/nope").host(Some("evil:1")),
            (421, code("misdirected_request")),
        ),
        (
            "method before origin",
            Req::new(p, "PUT", "/hook").header("Origin", "http://evil"),
            (405, code("method_not_allowed")),
        ),
        (
            "HEAD before credential",
            Req::new(p, "HEAD", "/api/threads"),
            (405, None),
        ),
        (
            "origin before credential",
            Req::new(p, "POST", "/hook").header("Origin", "http://evil"),
            (403, code("origin_rejected")),
        ),
        (
            "credential before kind",
            Req::new(p, "POST", "/hook").bearer("chd_bad"),
            (401, code("unauthorized")),
        ),
        (
            "credential before framing",
            Req::new(p, "POST", "/hook")
                .header("Transfer-Encoding", "chunked")
                .no_length(),
            (401, code("unauthorized")),
        ),
        (
            "kind before framing",
            Req::new(p, "POST", "/hook").bearer(&owner).no_length(),
            (403, code("forbidden")),
        ),
        (
            "action before framing",
            Req::new(p, "POST", "/api/commands/key")
                .bearer(&agent)
                .no_length(),
            (403, code("forbidden")),
        ),
        (
            "unknown command before framing",
            Req::new(p, "POST", "/api/commands/bogus")
                .bearer(&owner)
                .no_length(),
            (404, code("not_found")),
        ),
        (
            "transfer-encoding before length",
            Req::new(p, "POST", "/hook")
                .bearer(&hook)
                .header("Transfer-Encoding", "chunked")
                .no_length(),
            (400, code("bad_request")),
        ),
        (
            "framing before size",
            Req::new(p, "POST", "/hook").bearer(&hook).no_length(),
            (411, code("length_required")),
        ),
        (
            "size before unimplemented",
            Req::new(p, "POST", "/hook")
                .bearer(&hook)
                .no_length()
                .header("Content-Length", &big),
            (413, code("payload_too_large")),
        ),
        (
            "unimplemented last",
            Req::new(p, "POST", "/hook").bearer(&hook).body(b"{}"),
            (501, code("not_implemented")),
        ),
    ];
    for (name, req, want) in cases {
        let mut s = connect(p).await;
        // Heads only: none of these may need the body.
        s.write_all(&req.head()).await.unwrap();
        let (out, end) = read_to_end(&mut s).await;
        assert_eq!(parse(&out, end).outcome(), want, "{name}");
    }
    srv.stop().await;
}

/// Asserts the reply is the only one on its connection and the connection then ended.
fn assert_last(r: &support::http::Reply, what: &str) {
    assert_eq!(r.header("connection"), Some("close"), "{what}: {r:?}");
    assert!(r.rest.is_empty(), "{what}: a second reply followed: {r:?}");
    // EOF, or a reset when the peer closed with our unread second request (Windows).
    assert!(
        matches!(
            r.end,
            None | Some(
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            )
        ),
        "{what}: the connection did not end cleanly: {:?}",
        r.end
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_request_per_connection() {
    let (mounts, calls) = counting_root();
    let srv = TestServer::start(mounts).await;
    let ok = srv.req("GET", "/");
    let bad_host = srv.req("GET", "/").host(Some("evil:1"));
    let bad_method = srv.req("DELETE", "/");

    // Two requests in one write: only the first is answered and dispatched.
    for (first, dispatched) in [(&ok, 1), (&bad_host, 0), (&bad_method, 0)] {
        let before = calls.load(Ordering::SeqCst);
        let mut both = first.bytes();
        both.extend_from_slice(&ok.bytes());
        let mut s = connect(srv.port).await;
        s.write_all(&both).await.unwrap();
        let (out, end) = read_to_end(&mut s).await;
        let r = parse(&out, end);
        assert_last(&r, "pipelined");
        assert_eq!(
            calls.load(Ordering::SeqCst) - before,
            dispatched,
            "pipelined: {r:?}"
        );
    }

    // A second request after the first response: never dispatched.
    for (first, dispatched) in [(&ok, 1), (&bad_host, 0)] {
        let before = calls.load(Ordering::SeqCst);
        let mut s = connect(srv.port).await;
        s.write_all(&first.bytes()).await.unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while !got.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = tokio::io::AsyncReadExt::read(&mut s, &mut buf)
                .await
                .unwrap();
            assert!(n > 0, "EOF before the first reply");
            got.extend_from_slice(&buf[..n]);
        }
        // The peer may have closed already; a failed write is part of the outcome.
        let _ = s.write_all(&ok.bytes()).await;
        let (out, end) = read_to_end(&mut s).await;
        got.extend_from_slice(&out);
        let r = parse(&got, end);
        assert_last(&r, "sequential");
        assert_eq!(
            calls.load(Ordering::SeqCst) - before,
            dispatched,
            "sequential: {r:?}"
        );
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn te_with_content_length_is_400() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let hook = srv.token(TokenKind::Hook, None, None).await;
    for req in [
        srv.req("POST", "/hook")
            .bearer(&hook)
            .header("Transfer-Encoding", "chunked")
            .no_length()
            .body(b"0\r\n\r\n"),
        srv.req("POST", "/hook")
            .bearer(&hook)
            .header("Transfer-Encoding", "chunked")
            .header("Content-Length", "5")
            .no_length()
            .body(b"0\r\n\r\n"),
    ] {
        let r = srv.send(&req).await;
        assert_eq!(r.outcome(), (400, code("bad_request")), "{r:?}");
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn framing_and_size() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let hook = srv.token(TokenKind::Hook, None, None).await;
    let post = || srv.req("POST", "/hook").bearer(&hook);
    let max = 4 * 1024 * 1024;
    let cases = [
        (
            "no length",
            post().no_length(),
            (411, code("length_required")),
        ),
        (
            "over 4 MiB",
            post()
                .no_length()
                .header("Content-Length", &(max + 1).to_string()),
            (413, code("payload_too_large")),
        ),
        ("empty body", post(), (501, code("not_implemented"))),
        (
            "exactly 4 MiB declared",
            post()
                .no_length()
                .header("Content-Length", &max.to_string()),
            (501, code("not_implemented")),
        ),
    ];
    for (name, req, want) in cases {
        let mut s = connect(srv.port).await;
        s.write_all(&req.head()).await.unwrap();
        let (out, end) = read_to_end(&mut s).await;
        assert_eq!(parse(&out, end).outcome(), want, "{name}");
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_with_body_is_400() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let get = || srv.req("GET", "/api/threads").bearer(&owner);
    let r = srv
        .send(&get().header("Content-Length", "5").body(b"hello"))
        .await;
    assert_eq!(r.outcome(), (400, code("bad_request")));
    // Any spelling of zero is no body.
    for zero in ["0", "00"] {
        let r = srv.send(&get().header("Content-Length", zero)).await;
        assert_eq!(r.outcome(), (200, None), "{zero}");
    }
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parser_layer_cases() {
    let (mounts, calls) = counting_root();
    let srv = TestServer::start(mounts).await;
    let get = || srv.req("GET", "/");
    let mut many = get();
    for i in 0..65 {
        many = many.header(&format!("X-H{i}"), "1");
    }
    let cases = [
        (
            "Content-Length: +5",
            get().header("Content-Length", "+5").body(b"hello"),
            400,
        ),
        (
            "two different lengths",
            get()
                .header("Content-Length", "1")
                .header("Content-Length", "2")
                .body(b"ab"),
            400,
        ),
        (
            "9 KiB head",
            get().header("X-Big", &"a".repeat(9 * 1024)),
            431,
        ),
        ("65 headers", many, 431),
    ];
    for (name, req, status) in cases {
        let r = srv.send(&req).await;
        assert_eq!(r.status, status, "{name}: {r:?}");
        // hyper's own reply: no ErrorBody, no daemon headers.
        assert_eq!(r.code(), None, "{name}: {r:?}");
        assert!(r.headers_named("cache-control").is_empty(), "{name}: {r:?}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the service was called");
    // Positive control: the counting handler is reachable.
    assert_eq!(srv.send(&get()).await.status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_head_is_closed() {
    let (mounts, calls) = counting_root();
    let srv = TestServer::start(mounts).await;
    let mut s = connect(srv.port).await;
    let head = srv.req("GET", "/").head();
    let started = Instant::now();
    s.write_all(&head[..head.len() - 2]).await.unwrap();
    let (out, end) = read_to_end(&mut s).await;
    let took = started.elapsed();
    assert!(
        out.is_empty(),
        "a reply to an incomplete head: {:?}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(end, None, "expected EOF");
    assert!(
        took >= HEAD_TIMEOUT - Duration::from_millis(100)
            && took < HEAD_TIMEOUT + Duration::from_secs(3),
        "closed after {took:?}, header read timeout {HEAD_TIMEOUT:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    srv.stop().await;
}

/// Trickles `bytes` in `parts` writes spread over `over`.
async fn trickle(s: &mut tokio::net::TcpStream, bytes: &[u8], parts: usize, over: Duration) {
    let step = bytes.len().div_ceil(parts);
    for chunk in bytes.chunks(step) {
        s.write_all(chunk).await.unwrap();
        tokio::time::sleep(over / u32::try_from(parts).unwrap()).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deadline_from_first_head_byte() {
    // A write route whose handler reads the body; the body never completes.
    let outcome = Arc::new(std::sync::Mutex::new(None));
    let o = outcome.clone();
    let mounts = Mounts::mvp().with(Mount::write(
        spec(&Method::POST, "/hook").unwrap(),
        move |ctx: WriteCtx, req: axum::http::Request<Body>| {
            let o = o.clone();
            async move {
                let r = ctx.body_within(req.into_body(), 1024).await;
                *o.lock().unwrap() = Some(r.as_ref().map(|_| ()).map_err(ApiError::status));
                r.map(|_| ok_response())
            }
        },
    ));
    let srv = TestServer::start(mounts).await;
    let hook = srv.token(TokenKind::Hook, None, None).await;
    let req = srv
        .req("POST", "/hook")
        .bearer(&hook)
        .no_length()
        .header("Content-Length", "100");
    let mut s = connect(srv.port).await;
    let first_byte = Instant::now();
    // The head takes 1.4 s; measured from the head's end the deadline would be 3.4 s.
    trickle(&mut s, &req.head(), 7, Duration::from_millis(1400)).await;
    s.write_all(b"{\"partial\":").await.unwrap();
    let (out, end) = read_to_end(&mut s).await;
    let took = first_byte.elapsed();
    assert_eq!(parse(&out, end).outcome(), (408, code("request_timeout")));
    assert!(
        took >= DEADLINE - Duration::from_millis(50)
            && took < DEADLINE + Duration::from_millis(900),
        "408 after {took:?} from the first head byte; deadline {DEADLINE:?}"
    );
    assert_eq!(
        *outcome.lock().unwrap(),
        Some(Err(axum::http::StatusCode::REQUEST_TIMEOUT))
    );
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_past_deadline_is_408() {
    let mounts = Mounts::mvp().with(Mount::read(
        spec(&Method::GET, "/").unwrap(),
        |_ctx: ReadCtx, _req| std::future::pending::<Result<Response<Body>, ApiError>>(),
    ));
    let srv = TestServer::start(mounts).await;
    let started = Instant::now();
    let r = srv.send(&srv.req("GET", "/")).await;
    let took = started.elapsed();
    assert_eq!(r.outcome(), (408, code("request_timeout")));
    assert!(
        took >= DEADLINE - Duration::from_millis(50)
            && took < DEADLINE + Duration::from_millis(900),
        "{took:?}"
    );
    srv.stop().await;
}

/// A `/hook` write handler that submits one plain write and reports its deadline.
fn plain_write_mount(deadline_tx: tokio::sync::mpsc::UnboundedSender<Instant>) -> Mounts {
    Mounts::mvp().with(Mount::write(
        spec(&Method::POST, "/hook").unwrap(),
        move |ctx: WriteCtx, _req| {
            let tx = deadline_tx.clone();
            async move {
                tx.send(ctx.deadline()).unwrap();
                ctx.write(|_tx, auth| Ok(auth.is_some())).await?;
                Ok(ok_response())
            }
        },
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_write_expired_is_408() {
    let (dtx, mut drx) = tokio::sync::mpsc::unbounded_channel();
    let srv = TestServer::start_with(plain_write_mount(dtx), |c| {
        c.write_reply_grace = Duration::from_secs(1);
    })
    .await;
    let hook = srv.token(TokenKind::Hook, None, None).await;
    let release = srv.hold_writer().await;
    let port = srv.port;
    let req = srv.req("POST", "/hook").bearer(&hook).body(b"{}");
    let reply = tokio::spawn(async move { support::http::send(port, &req.bytes()).await });
    let deadline = drx.recv().await.unwrap();
    // Release the writer just after the job's `not_after`: it takes the job late and
    // replies `Expired` within the reply grace.
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        deadline + Duration::from_millis(100),
    ))
    .await;
    release.send(()).unwrap();
    let r = reply.await.unwrap();
    assert_eq!(r.outcome(), (408, code("request_timeout")));
    assert!(srv.sink.0.lock().unwrap().is_empty());
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_write_unreplied_is_outcome_unknown() {
    let (dtx, mut drx) = tokio::sync::mpsc::unbounded_channel();
    let srv = TestServer::start(plain_write_mount(dtx)).await;
    let hook = srv.token(TokenKind::Hook, None, None).await;
    let release = srv.hold_writer().await;
    let r = srv
        .send(&srv.req("POST", "/hook").bearer(&hook).body(b"{}"))
        .await;
    let deadline = drx.recv().await.unwrap();
    assert!(Instant::now() >= deadline + Duration::from_millis(100));
    assert_eq!(r.outcome(), (503, code("outcome_unknown")));
    assert!(r.body.windows(14).any(|w| w == b"\"retry_after\":"));
    release.send(()).unwrap();
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reserved_launch_token_is_current() {
    // The agent check reads the thread's current launch: a newer reservation makes the
    // older launch's token invalid even before PR 10 revokes it.
    let srv = TestServer::start(Mounts::mvp()).await;
    let (t, l1, l2) = (
        ThreadId::from_u128(5),
        LaunchId::from_u128(51),
        LaunchId::from_u128(52),
    );
    let agent = srv.agent(t, l1).await;
    let get = || srv.req("GET", "/api/threads").bearer(&agent);
    assert_eq!(srv.send(&get()).await.status, 200);
    srv.append(vec![exited(t, l1)]).await;
    srv.append(vec![reserved(t, l2)]).await;
    assert_eq!(srv.send(&get()).await.status, 401);
    srv.stop().await;
}

/// A handler that would answer at once is not invoked once the deadline has passed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_read_past_deadline_is_408() {
    let (mounts, calls) = counting_root();
    let srv = TestServer::start_with(mounts, |c| c.short_deadline = Duration::ZERO).await;
    let r = srv.send(&srv.req("GET", "/")).await;
    assert_eq!(r.outcome(), (408, code("request_timeout")));
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the handler ran");
    srv.stop().await;
}

/// Deadlines are on the server's clock: a clock moved forward by more than the short
/// deadline before the request takes nothing from the request's budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skewed_clock_keeps_the_budget() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    srv.clock.skew(DEADLINE * 30);
    let r = srv
        .send(&srv.req("GET", "/api/threads").bearer(&owner))
        .await;
    srv.clock.skew(Duration::ZERO);
    assert_eq!(r.outcome(), (200, None));
    srv.stop().await;
}

/// HTTP/1.0 requests get the same rules: the guard decides, and the connection carries one
/// request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http10_gets_the_same_rules() {
    let (mounts, calls) = counting_root();
    let srv = TestServer::start(mounts).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let r = support::http::send(srv.port, &srv.req("GET", "/").http10_bytes()).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.header("connection"), Some("close"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for (name, req, want) in [
        (
            "bad Host",
            srv.req("GET", "/").host(Some("evil:1")),
            (421, code("misdirected_request")),
        ),
        (
            "no token",
            srv.req("GET", "/api/threads"),
            (401, code("unauthorized")),
        ),
        (
            "owner",
            srv.req("GET", "/api/threads").bearer(&owner),
            (200, None),
        ),
    ] {
        let r = support::http::send(srv.port, &req.http10_bytes()).await;
        assert_eq!(r.outcome(), want, "{name}: {r:?}");
        assert_eq!(r.header("connection"), Some("close"), "{name}");
    }
    srv.stop().await;
}

/// A read handler's result taken at or after the deadline is discarded: the handler moves
/// the server's clock past the deadline and answers at once, so its result and the cutoff
/// are both ready when the guard takes them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_handler_result_is_408() {
    let clock = Arc::new(support::http::SkewClock::default());
    let c = clock.clone();
    let mounts = Mounts::mvp().with(Mount::read(
        spec(&Method::GET, "/").unwrap(),
        move |_ctx: ReadCtx, _req| {
            c.skew(Duration::from_secs(3600));
            async { Ok(ok_response()) }
        },
    ));
    let cfg_clock = clock.clone();
    let srv = TestServer::start_with(mounts, move |cfg| cfg.clock = cfg_clock).await;
    let r = srv.send(&srv.req("GET", "/")).await;
    clock.skew(Duration::ZERO);
    assert_eq!(r.outcome(), (408, code("request_timeout")));
    srv.stop().await;
}

/// The cutoff timers run on real time converted from the server's clock: with the clock
/// moved an hour forward, a read mount that never answers is still cut off at its budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skewed_clock_keeps_the_timers() {
    let mounts = Mounts::mvp().with(Mount::read(
        spec(&Method::GET, "/").unwrap(),
        |_ctx: ReadCtx, _req| std::future::pending::<Result<Response<Body>, ApiError>>(),
    ));
    let srv = TestServer::start(mounts).await;
    srv.clock.skew(Duration::from_secs(3600));
    let started = Instant::now();
    let r = srv.send(&srv.req("GET", "/")).await;
    let took = started.elapsed();
    srv.clock.skew(Duration::ZERO);
    assert_eq!(r.outcome(), (408, code("request_timeout")));
    assert!(took < DEADLINE + Duration::from_millis(900), "{took:?}");
    srv.stop().await;
}
