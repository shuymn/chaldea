//! B-8 per layer (`docs/design/hook-ingest.md` §7.1; PR9-CL-17 to CL-21, CL-31): every
//! rejection of `/hook` against a server with the product limits, each with its expected
//! observation, a per-case deadline, and the no-side-effect check; every table ends with
//! its positive control. The `observed_*` cases keep sending while the server decides and
//! print `B8-OBSERVED <case> <os> <status> <end> <send_error>`.

mod support;

use std::io;
use std::time::{Duration, Instant};

use chaldea_core::auth::TokenKind;
use chaldea_daemon::hook::HookConfig;
use support::hook::{Hook, HookServer, Launch, Snapshot};
use support::http::{Req, connect};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The product limits, set explicitly (not the harness defaults).
async fn server() -> HookServer {
    HookServer::start_with(HookConfig::default(), |c| {
        c.short_deadline = Duration::from_secs(5);
        c.header_read_timeout = Duration::from_secs(5);
        c.max_body_bytes = 4 * 1024 * 1024;
        c.max_header_bytes = 8 * 1024;
    })
    .await
}

/// A table row: its name, the request, and the expected status and `ErrorBody` code.
type Case = (&'static str, Req, (u16, Option<String>));

const CASE: Duration = Duration::from_secs(8);
const LONG_CASE: Duration = Duration::from_secs(12);
const MIB: usize = 1024 * 1024;

/// How the connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Eof,
    /// `ConnectionReset` or `ConnectionAborted`: the peer reset the connection.
    Reset(io::ErrorKind),
    /// Any other read error.
    Error(io::ErrorKind),
    /// A whole reply came and the connection stayed open for 1 s after it.
    Open,
    /// The case's deadline came first.
    Timeout,
}

/// Whether `out` holds a whole reply: its head and `Content-Length` body bytes.
fn complete(out: &[u8]) -> bool {
    let Some(at) = out.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&out[..at]).to_ascii_lowercase();
    let len = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    out.len() >= at + 4 + len
}

/// The status code of a complete status line at the start of `out`.
fn status_line(out: &[u8]) -> Option<u16> {
    let at = out.windows(2).position(|w| w == b"\r\n")?;
    let line = std::str::from_utf8(&out[..at]).ok()?;
    let mut parts = line.splitn(3, ' ');
    if !parts.next()?.starts_with("HTTP/1.") {
        return None;
    }
    parts.next()?.parse().ok()
}

/// Whether a complete head at the start of `out` says `Connection: close`.
fn head_closes(out: &[u8]) -> Option<bool> {
    let at = out.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&out[..at]).to_ascii_lowercase();
    Some(head.lines().any(|l| {
        l.strip_prefix("connection:")
            .is_some_and(|v| v.trim() == "close")
    }))
}

/// What the client observed. A reset can cut the reply anywhere, so each part is kept as far
/// as it was read.
#[derive(Debug)]
struct Obs {
    /// The whole reply (its head and `Content-Length` body): status and `ErrorBody` code.
    status: Option<(u16, Option<String>)>,
    /// The code of a complete status line, read even when the rest of the reply was not.
    status_line: Option<u16>,
    end: End,
    send_error: Option<io::ErrorKind>,
    /// Once the reply's head was read: whether it carried `Connection: close`.
    closes: Option<bool>,
    /// From the first byte written to the whole reply, or to the end without one.
    elapsed: Duration,
    /// Body bytes written before the status was read.
    body_sent_before_status: usize,
}

/// What the client sends after the head.
#[derive(Clone)]
enum Body {
    /// Nothing; the client only reads.
    Withheld,
    /// All of it, concurrently with reading, as fast as the peer takes it; then nothing more.
    Sent(Vec<u8>),
    /// `chunk` every `every` until the connection ends.
    Endless { chunk: usize, every: Duration },
}

/// Connects, writes `head` in `head_parts` writes spread over `head_over`, then `body`,
/// and reads until EOF, an error, or `case` from the first byte.
async fn observe(
    port: u16,
    head: &[u8],
    head_parts: usize,
    head_over: Duration,
    body: Body,
    case: Duration,
) -> Obs {
    let s = connect(port).await;
    let (mut rd, mut wr) = s.into_split();
    let first = Instant::now();
    let until = tokio::time::Instant::from_std(first + case);
    let step = head.len().div_ceil(head_parts.max(1));
    let pause = head_over / u32::try_from(head_parts.max(1)).unwrap();
    let mut send_error = None;
    for (i, chunk) in head.chunks(step).enumerate() {
        if i > 0 {
            tokio::time::sleep(pause).await;
        }
        if let Err(e) = wr.write_all(chunk).await {
            send_error = Some(e.kind());
            break;
        }
    }
    // Set once the writer has nothing more to send (it then only keeps the write half open).
    let idle = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let idle_w = idle.clone();
    let sent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sent_w = sent.clone();
    // A failed write completes only once the collector below has started its bounded wait
    // for the writer, so the send error always comes in after the end was seen, as it can
    // when the server closes, and only a collector that waits gets it.
    let (joining, mut joining_w) = tokio::sync::watch::channel(false);
    let writer = tokio::spawn(async move {
        let record = |n: usize| sent_w.fetch_add(n, std::sync::atomic::Ordering::SeqCst);
        let mut failed = async |e: io::Error| {
            let _ = joining_w.wait_for(|j| *j).await;
            Some(e.kind())
        };
        match body {
            Body::Withheld => {
                // Keep the write half open until the reader is done.
                idle_w.store(true, std::sync::atomic::Ordering::SeqCst);
                std::future::pending::<()>().await;
                None
            }
            Body::Sent(b) => {
                for c in b.chunks(64 * 1024) {
                    if let Err(e) = wr.write_all(c).await {
                        return failed(e).await;
                    }
                    record(c.len());
                }
                idle_w.store(true, std::sync::atomic::Ordering::SeqCst);
                std::future::pending::<()>().await;
                None
            }
            Body::Endless { chunk, every } => loop {
                if let Err(e) = wr.write_all(&vec![b'x'; chunk]).await {
                    return failed(e).await;
                }
                record(chunk);
                tokio::time::sleep(every).await;
            },
        }
    });
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let mut body_sent_before_status = None;
    let mut until = until;
    let mut replied_at = None;
    let end = loop {
        match tokio::time::timeout_at(until, rd.read(&mut buf)).await {
            Err(_) if replied_at.is_some() => break End::Open,
            Err(_) => break End::Timeout,
            Ok(Ok(0)) => break End::Eof,
            Ok(Ok(n)) => {
                if body_sent_before_status.is_none() {
                    body_sent_before_status = Some(sent.load(std::sync::atomic::Ordering::SeqCst));
                }
                out.extend_from_slice(&buf[..n]);
                if replied_at.is_none() && complete(&out) {
                    replied_at = Some(first.elapsed());
                    until = until.min(tokio::time::Instant::now() + Duration::from_secs(1));
                }
            }
            Ok(Err(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                ) =>
            {
                break End::Reset(e.kind());
            }
            Ok(Err(e)) => break End::Error(e.kind()),
        }
    };
    let elapsed = replied_at.unwrap_or_else(|| first.elapsed());
    // A writer still sending once the connection ended gets its error soon: wait for it
    // (at most 1 s), then abort a writer still pending (a dropped pending write).
    let mut writer = writer;
    let ended = matches!(end, End::Eof | End::Reset(_) | End::Error(_));
    let sending = !idle.load(std::sync::atomic::Ordering::SeqCst);
    let waited = if sending && ended {
        let _ = joining.send(true);
        tokio::time::timeout(Duration::from_secs(1), &mut writer)
            .await
            .ok()
    } else {
        None
    };
    let send_error = send_error.or(match waited {
        Some(r) => r.ok().flatten(),
        None => {
            writer.abort();
            None
        }
    });
    Obs {
        status: complete(&out).then(|| support::http::parse(&out, None).outcome()),
        status_line: status_line(&out),
        closes: head_closes(&out),
        end,
        send_error,
        elapsed,
        body_sent_before_status: body_sent_before_status.unwrap_or(0),
    }
}

fn os() -> &'static str {
    std::env::consts::OS
}

fn print_observed(case: &str, o: &Obs) {
    let status = o.status.as_ref().map_or_else(
        || "none".to_owned(),
        |(s, c)| format!("{s}/{}", c.as_deref().unwrap_or("-")),
    );
    let end = match o.end {
        End::Eof => "eof".to_owned(),
        End::Reset(k) => format!("reset:{k:?}"),
        End::Error(k) => format!("error:{k:?}"),
        End::Open => "open".to_owned(),
        End::Timeout => "timeout".to_owned(),
    };
    let send = o
        .send_error
        .map_or_else(|| "none".to_owned(), |k| format!("{k:?}"));
    println!("B8-OBSERVED {case} {} {status} {end} {send}", os());
}

/// A launch with its hook token, the standard chain, unbound: a valid hook is buffered (one
/// delivery).
async fn setup(h: &HookServer) -> Launch {
    h.codex(1).await
}

fn valid(l: &Launch) -> Hook {
    Hook::of(l, "Stop", "s")
}

/// The positive control: the same server and launch accept a valid hook (200, one more
/// delivery), so a broken setup cannot pass the negative cases.
async fn positive_control(h: &HookServer, l: &Launch) {
    let before = h.snapshot(l).deliveries;
    let x = valid(l);
    let r = h.post(&x).await;
    assert_eq!(
        (r.status, r.body.len()),
        (200, 0),
        "positive control: {r:?}"
    );
    assert_eq!(h.snapshot(l).deliveries, before + 1, "positive control");
}

fn assert_unchanged(h: &HookServer, l: &Launch, before: &Snapshot, case: &str) {
    assert_eq!(&h.snapshot(l), before, "{case}: a side effect");
}

fn code(s: &str) -> Option<String> {
    Some(s.to_owned())
}

/// PR 7's close contract: a rejection's reply head, when the client read it before the
/// server's close (a reset can discard it), says `Connection: close`, and the server closes:
/// EOF or a reset, never an open connection or another read error.
fn assert_closed(case: &str, o: &Obs) {
    assert_ne!(o.closes, Some(false), "{case}: {o:?}");
    assert!(matches!(o.end, End::Eof | End::Reset(_)), "{case}: {o:?}");
}

/// What a client still sending when the server decides may observe (K2, PR9-CL-20): every
/// part of the reply it read is the decided one (the status line, then the whole reply with
/// its `ErrorBody` code); a reset may have cut the rest, or all of it.
fn assert_decided_part(case: &str, o: &Obs, want: &(u16, Option<String>)) {
    if let Some(code) = o.status_line {
        assert_eq!(code, want.0, "{case}: {o:?}");
    }
    if let Some(s) = &o.status {
        assert_eq!(s, want, "{case}: {o:?}");
    }
}

/// Runs each case with `body`, asserting the case deadline, no side effect, and the exact
/// status when no body byte is in flight. A case still sending body bytes when the server
/// answers from the head may see a reset instead (K2, D9): the server reads what has
/// arrived once and closes, and a close with unread bytes resets the connection, which on
/// Windows discards the reply the client has not read yet; a status it does read is the
/// decided one.
async fn table(
    h: &HookServer,
    l: &Launch,
    cases: Vec<Case>,
    body: impl Fn(&Req) -> Body,
    withheld: bool,
) {
    for (name, req, want) in cases {
        let before = h.snapshot(l);
        let body = body(&req);
        let in_flight = match &body {
            Body::Withheld => false,
            Body::Sent(b) => !b.is_empty(),
            Body::Endless { .. } => true,
        };
        let o = observe(h.srv.port, &req.head(), 1, Duration::ZERO, body, CASE).await;
        if in_flight {
            assert_decided_part(name, &o, &want);
        } else {
            assert_eq!(o.status.as_ref(), Some(&want), "{name}: {o:?}");
        }
        assert!(o.elapsed < CASE, "{name}: {o:?}");
        assert_closed(name, &o);
        if withheld {
            assert_eq!(o.body_sent_before_status, 0, "{name}: {o:?}");
        }
        assert_unchanged(h, l, &before, name);
    }
}

fn with_body(req: &Req) -> Body {
    // The request's own body: the bytes after its head.
    Body::Sent(req.bytes()[req.head().len()..].to_vec())
}

// ---- parser -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parser_layer() {
    let h = server().await;
    let l = setup(&h).await;
    let base = valid(&l).req(&h.srv);
    let mut many = base.clone();
    for i in 0..65 {
        many = many.header(&format!("X-H{i}"), "1");
    }
    let raw_cases: Vec<(&str, Vec<u8>, u16)> = vec![
        (
            "+5",
            base.clone()
                .no_length()
                .header("Content-Length", "+5")
                .head(),
            400,
        ),
        (
            "two different lengths",
            base.clone()
                .no_length()
                .header("Content-Length", "1")
                .header("Content-Length", "2")
                .head(),
            400,
        ),
        ("head at the 8 KiB limit", base.head_at_limit(8 * 1024), 431),
        ("65 headers", many.head(), 431),
        (
            "HTTP/1.0 with Transfer-Encoding",
            {
                let r = base
                    .clone()
                    .no_length()
                    .header("Transfer-Encoding", "chunked")
                    .body(b"");
                r.http10_bytes()
            },
            400,
        ),
    ];
    for (name, head, status) in raw_cases {
        let before = h.snapshot(&l);
        let o = observe(h.srv.port, &head, 1, Duration::ZERO, Body::Withheld, CASE).await;
        assert_eq!(
            o.status.as_ref().map(|s| s.0),
            Some(status),
            "{name}: {o:?}"
        );
        // hyper's own reply: no ErrorBody.
        assert_eq!(
            o.status.as_ref().and_then(|s| s.1.clone()),
            None,
            "{name}: {o:?}"
        );
        assert_unchanged(&h, &l, &before, name);
    }
    // A head missing its final CRLF: EOF after the 5 s parser bound, no status.
    let before = h.snapshot(&l);
    let head = base.head();
    let o = observe(
        h.srv.port,
        &head[..head.len() - 2],
        1,
        Duration::ZERO,
        Body::Withheld,
        LONG_CASE,
    )
    .await;
    assert_eq!(
        (o.status.as_ref(), o.end),
        (None, End::Eof),
        "slow head: {o:?}"
    );
    assert!(
        o.elapsed >= Duration::from_millis(4900) && o.elapsed < CASE,
        "slow head: {o:?}"
    );
    assert_unchanged(&h, &l, &before, "slow head");
    positive_control(&h, &l).await;
    h.stop().await;
}

// ---- guard and handler, then ordering ----------------------------------------------------

async fn guard_cases(h: &HookServer, l: &Launch) -> Vec<Case> {
    let srv = &h.srv;
    let p = srv.port;
    let v = || valid(l).req(srv);
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let unscoped = srv.token(TokenKind::Hook, None, None).await;
    let revoked = srv
        .token(TokenKind::Hook, Some(l.thread), Some(l.launch))
        .await;
    srv.revoke_quietly(&revoked).await;
    let hook_with = |token: &str| {
        let mut x = valid(l);
        x.token = token.to_owned();
        x.req(srv)
    };
    let no_auth = {
        let mut x = valid(l);
        x.token = String::new();
        let r = x.req(srv);
        // Drop the Authorization line the builder added.
        Req::new(p, "POST", "/hook")
            .header("X-Chaldea-Emit-Id", &x.emit.to_string())
            .body(&r.bytes()[r.head().len()..])
    };
    let big = (4 * MIB + 1).to_string();
    vec![
        (
            "bad Host",
            v().host(Some("evil:1")),
            (421, code("misdirected_request")),
        ),
        (
            "wrong port",
            v().host(Some(&format!("127.0.0.1:{}", p.wrapping_add(1)))),
            (421, code("misdirected_request")),
        ),
        (
            "two Host",
            v().header("Host", &format!("127.0.0.1:{p}")),
            (421, code("misdirected_request")),
        ),
        (
            "no Host",
            v().host(None),
            (421, code("misdirected_request")),
        ),
        (
            "GET /hook",
            Req::new(p, "GET", "/hook").bearer(&l.token),
            (405, code("method_not_allowed")),
        ),
        (
            "POST /hooks",
            Req::new(p, "POST", "/hooks").bearer(&l.token).body(b"{}"),
            (404, code("not_found")),
        ),
        (
            "Origin",
            v().header("Origin", &format!("http://127.0.0.1:{p}")),
            (403, code("origin_rejected")),
        ),
        ("no Authorization", no_auth, (401, code("unauthorized"))),
        (
            "two Authorization",
            v().bearer(&l.token),
            (401, code("unauthorized")),
        ),
        (
            "Basic",
            Req::new(p, "POST", "/hook")
                .header("Authorization", &format!("Basic {}", l.token))
                .body(b"{}"),
            (401, code("unauthorized")),
        ),
        (
            "malformed",
            hook_with("chd_short"),
            (401, code("unauthorized")),
        ),
        (
            "unknown",
            hook_with(&format!("chd_{}", "U".repeat(43))),
            (401, code("unauthorized")),
        ),
        ("revoked", hook_with(&revoked), (401, code("unauthorized"))),
        (
            "unscoped hook token",
            hook_with(&unscoped),
            (401, code("unauthorized")),
        ),
        ("owner token", hook_with(&owner), (403, code("forbidden"))),
        (
            "Transfer-Encoding: chunked",
            v().no_length()
                .header("Transfer-Encoding", "chunked")
                .body(b"0\r\n\r\n"),
            (400, code("bad_request")),
        ),
        (
            "chunked + Content-Length",
            v().no_length()
                .header("Transfer-Encoding", "chunked")
                .header("Content-Length", "5")
                .body(b"0\r\n\r\n"),
            (400, code("bad_request")),
        ),
        (
            "Content-Length + chunked",
            v().no_length()
                .header("Content-Length", "5")
                .header("Transfer-Encoding", "chunked")
                .body(b"0\r\n\r\n"),
            (400, code("bad_request")),
        ),
        (
            "no Content-Length",
            v().no_length().body(b""),
            (411, code("length_required")),
        ),
        (
            "4 MiB + 1 declared",
            v().no_length().header("Content-Length", &big).body(b""),
            (413, code("payload_too_large")),
        ),
    ]
}

fn handler_cases(h: &HookServer, l: &Launch) -> Vec<Case> {
    let srv = &h.srv;
    let body = support::hook::payload("Stop", "s");
    let base = || {
        srv.req("POST", "/hook")
            .bearer(&l.token)
            .header("X-Chaldea-Thread-Id", &l.thread.to_string())
            .body(&body)
    };
    let id = uuid::Uuid::new_v4().to_string();
    vec![
        ("no emit id", base(), (400, code("bad_request"))),
        (
            "two emit ids",
            base()
                .header("X-Chaldea-Emit-Id", &id)
                .header("X-Chaldea-Emit-Id", &id),
            (400, code("bad_request")),
        ),
        (
            "not-a-uuid",
            base().header("X-Chaldea-Emit-Id", "not-a-uuid"),
            (400, code("bad_request")),
        ),
        (
            "two thread ids",
            base()
                .header("X-Chaldea-Emit-Id", &id)
                .header("X-Chaldea-Thread-Id", &l.thread.to_string()),
            (400, code("bad_request")),
        ),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guard_layer() {
    let h = server().await;
    let l = setup(&h).await;
    let cases = guard_cases(&h, &l).await;
    table(&h, &l, cases, with_body, false).await;
    let r = h
        .srv
        .send(&h.srv.req("GET", "/hook").bearer(&l.token))
        .await;
    assert_eq!((r.status, r.header("Allow")), (405, Some("POST")), "{r:?}");
    positive_control(&h, &l).await;
    h.stop().await;
}

/// Eight requests each declaring 4 MiB and withholding it hold the whole 32 MiB budget.
async fn exhaust_budget(h: &HookServer, l: &Launch) -> Vec<tokio::net::TcpStream> {
    let mut held = Vec::new();
    for _ in 0..8 {
        let req = valid(l)
            .req(&h.srv)
            .no_length()
            .header("Content-Length", &(4 * MIB).to_string());
        let mut s = connect(h.srv.port).await;
        s.write_all(&req.head()).await.unwrap();
        held.push(s);
    }
    let ingest = h.ingest.clone();
    support::http::eventually(Duration::from_secs(4), "the budget is held", move || {
        ingest.free_permits() == 0
    })
    .await;
    held
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handler_layer() {
    let h = server().await;
    let l = setup(&h).await;
    let cases = handler_cases(&h, &l);
    table(&h, &l, cases, with_body, false).await;
    // H2: the body budget exhausted by held requests: 503 busy before any body byte.
    let held = exhaust_budget(&h, &l).await;
    let before = h.snapshot(&l);
    let req = valid(&l).req(&h.srv);
    let o = observe(
        h.srv.port,
        &req.head(),
        1,
        Duration::ZERO,
        Body::Withheld,
        CASE,
    )
    .await;
    assert_eq!(o.status, Some((503, code("busy"))), "{o:?}");
    assert!(o.elapsed < CASE, "{o:?}");
    assert_closed("budget", &o);
    assert_unchanged(&h, &l, &before, "budget");
    drop(held);
    let ingest = h.ingest.clone();
    support::http::eventually(Duration::from_secs(8), "the budget returns", move || {
        ingest.free_permits() == 32 * 1024
    })
    .await;
    positive_control(&h, &l).await;
    h.stop().await;
}

/// Every guard and handler case with the body withheld until the status is read: the
/// status comes with zero body bytes sent (PR9-CL-18).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordering_body_withheld() {
    let h = server().await;
    let l = setup(&h).await;
    let mut cases = guard_cases(&h, &l).await;
    cases.extend(handler_cases(&h, &l));
    table(&h, &l, cases, |_| Body::Withheld, true).await;
    let held = exhaust_budget(&h, &l).await;
    let req = valid(&l).req(&h.srv);
    let o = observe(
        h.srv.port,
        &req.head(),
        1,
        Duration::ZERO,
        Body::Withheld,
        CASE,
    )
    .await;
    assert_eq!(o.status, Some((503, code("busy"))), "{o:?}");
    assert!(o.elapsed < CASE, "{o:?}");
    assert_closed("budget", &o);
    assert_eq!(o.body_sent_before_status, 0);
    drop(held);
    let ingest = h.ingest.clone();
    support::http::eventually(Duration::from_secs(8), "the budget returns", move || {
        ingest.free_permits() == 32 * 1024
    })
    .await;
    positive_control(&h, &l).await;
    h.stop().await;
}

// ---- deadline ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deadline_from_first_head_byte() {
    let h = server().await;
    let l = setup(&h).await;
    let before = h.snapshot(&l);
    let body = vec![b' '; 4096];
    let req = valid(&l)
        .req(&h.srv)
        .no_length()
        .header("Content-Length", &body.len().to_string());
    // The head over 4.0 s from the first byte (inside the parser's 5 s), half the body by
    // 4.6 s, its last chunk before 5 s, then the client only reads.
    let s = connect(h.srv.port).await;
    let (mut rd, mut wr) = s.into_split();
    let first = Instant::now();
    let head = req.head();
    let step = head.len().div_ceil(9);
    for (i, chunk) in head.chunks(step).enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        wr.write_all(chunk).await.unwrap();
    }
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        first + Duration::from_millis(4300),
    ))
    .await;
    wr.write_all(&body[..1024]).await.unwrap();
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        first + Duration::from_millis(4600),
    ))
    .await;
    wr.write_all(&body[1024..2048]).await.unwrap();
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        first + Duration::from_millis(4800),
    ))
    .await;
    wr.write_all(&body[2048..2560]).await.unwrap();
    let mut out = Vec::new();
    let until = tokio::time::Instant::from_std(first + Duration::from_millis(7500));
    while !complete(&out) {
        let mut buf = [0u8; 4096];
        match tokio::time::timeout_at(until, rd.read(&mut buf)).await {
            Err(_) => panic!("no 408 by first byte + 7.5 s"),
            Ok(Ok(0) | Err(_)) => break,
            Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
        }
    }
    let r = support::http::parse(&out, None);
    assert_eq!(r.outcome(), (408, code("request_timeout")));
    assert!(first.elapsed() < Duration::from_millis(7500));
    drop(wr);
    assert_unchanged(&h, &l, &before, "deadline from the first head byte");
    positive_control(&h, &l).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn body_held_is_408() {
    let h = server().await;
    let l = setup(&h).await;
    let before = h.snapshot(&l);
    let req = valid(&l).req(&h.srv);
    let o = observe(
        h.srv.port,
        &req.head(),
        1,
        Duration::ZERO,
        Body::Withheld,
        LONG_CASE,
    )
    .await;
    assert_eq!(o.status, Some((408, code("request_timeout"))), "{o:?}");
    assert!(
        o.elapsed >= Duration::from_millis(4800) && o.elapsed < Duration::from_millis(7500),
        "{o:?}"
    );
    assert_unchanged(&h, &l, &before, "body held");
    positive_control(&h, &l).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observed_slow_body() {
    let h = server().await;
    let l = setup(&h).await;
    let before = h.snapshot(&l);
    let req = valid(&l)
        .req(&h.srv)
        .no_length()
        .header("Content-Length", &MIB.to_string());
    let o = observe(
        h.srv.port,
        &req.head(),
        1,
        Duration::ZERO,
        Body::Endless {
            chunk: 64,
            every: Duration::from_millis(100),
        },
        LONG_CASE,
    )
    .await;
    print_observed("slow_body", &o);
    assert_decided_part("slow body", &o, &(408, code("request_timeout")));
    assert_closed("slow body", &o);
    // The writer was still sending when the server closed: it reports its error.
    assert!(o.send_error.is_some(), "{o:?}");
    assert_unchanged(&h, &l, &before, "slow body");
    positive_control(&h, &l).await;
    h.stop().await;
}

// ---- in flight ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observed_body_in_flight() {
    let h = server().await;
    let l = setup(&h).await;
    let revoked = h
        .srv
        .token(TokenKind::Hook, Some(l.thread), Some(l.launch))
        .await;
    h.srv.revoke_quietly(&revoked).await;
    let body = vec![b'x'; MIB];
    let mut x = valid(&l);
    x.token = revoked;
    x.body = body.clone();
    let cases = [
        (
            "body_in_flight_401",
            x.req(&h.srv),
            (401, code("unauthorized")),
        ),
        (
            "body_in_flight_413",
            valid(&l)
                .req(&h.srv)
                .no_length()
                .header("Content-Length", &(4 * MIB + 1).to_string()),
            (413, code("payload_too_large")),
        ),
    ];
    for (name, req, want) in cases {
        let before = h.snapshot(&l);
        let o = observe(
            h.srv.port,
            &req.head(),
            1,
            Duration::ZERO,
            Body::Sent(body.clone()),
            LONG_CASE,
        )
        .await;
        print_observed(name, &o);
        assert_decided_part(name, &o, &want);
        assert_closed(name, &o);
        assert_unchanged(&h, &l, &before, name);
    }
    positive_control(&h, &l).await;
    h.stop().await;
}

// ---- accepted ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_equal_length_accepted() {
    let h = server().await;
    let l = setup(&h).await;
    let before = h.snapshot(&l).deliveries;
    let x = valid(&l);
    let n = x.body.len().to_string();
    let req = x.req(&h.srv).header("Content-Length", &n);
    let r = h.srv.send(&req).await;
    assert_eq!((r.status, r.body.len()), (200, 0), "{r:?}");
    assert_eq!(h.snapshot(&l).deliveries, before + 1);
    assert!(h.delivery(l.launch, x.emit).is_some());
    positive_control(&h, &l).await;
    h.stop().await;
}

/// G8 under a larger HTTP limit: with `max_body_bytes = 8 MiB`, `/hook` still answers 413
/// for `4 MiB + 1` declared, before any body byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hook_cap_under_a_larger_http_limit() {
    let h = HookServer::start_with(HookConfig::default(), |c| {
        c.short_deadline = Duration::from_secs(5);
        c.header_read_timeout = Duration::from_secs(5);
        c.max_body_bytes = 8 * 1024 * 1024;
        c.max_header_bytes = 8 * 1024;
    })
    .await;
    let l = setup(&h).await;
    let before = h.snapshot(&l);
    let req = valid(&l)
        .req(&h.srv)
        .no_length()
        .header("Content-Length", &(4 * MIB + 1).to_string());
    let o = observe(
        h.srv.port,
        &req.head(),
        1,
        Duration::ZERO,
        Body::Withheld,
        CASE,
    )
    .await;
    assert_eq!(o.status, Some((413, code("payload_too_large"))), "{o:?}");
    assert_eq!(o.body_sent_before_status, 0);
    assert_unchanged(&h, &l, &before, "4 MiB + 1 under an 8 MiB HTTP limit");
    positive_control(&h, &l).await;
    h.stop().await;
}
