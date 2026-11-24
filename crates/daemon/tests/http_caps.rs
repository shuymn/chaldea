//! Resource caps (`docs/design/http-surface.md` PR7-CL-26): each cap's last allowed and
//! first refused request, and the next request after a slot frees. The caps are the
//! published numbers, written out here rather than read from the code.

mod support;

use std::time::{Duration, Instant};

use chaldea_core::auth::TokenKind;
use chaldea_core::ids::{StreamId, ThreadId};
use chaldea_core::payload::Content;
use chaldea_core::sync::WsServerMessage;
use chaldea_daemon::http::{HttpConfig, Mounts, WsConfig};
use chaldea_daemon::store::prepare_content;
use futures_util::{SinkExt, StreamExt};
use support::http::{Reply, TestServer, WsClient, connect};
use support::waits;
use support::{raw_unknown, registered};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;

fn busy(r: &Reply) {
    assert_eq!(r.outcome(), (503, Some("busy".to_owned())), "{r:?}");
    assert!(
        r.body.windows(14).any(|w| w == b"\"retry_after\":"),
        "{r:?}"
    );
}

/// A server whose heads refresh is far off.
async fn quiet(tune: impl FnOnce(&mut HttpConfig)) -> TestServer {
    TestServer::start_with(Mounts::mvp(), |c| {
        c.ws.heads_interval = Duration::from_secs(600);
        tune(c);
    })
    .await
}

/// Tickets held at once, and per token.
const TICKETS: usize = 1024;
const TICKETS_PER_TOKEN: usize = 32;
const PAIR_CODES: usize = 8;
const SOCKETS: usize = 128;
const SOCKETS_PER_TOKEN: usize = 16;
const BLOB_SLOTS: usize = 4;
const SUBSCRIPTIONS: usize = 1024;
const MESSAGE_BYTES: usize = 64 * 1024;

async fn tickets_cap(srv: &TestServer) {
    let mut tokens = Vec::new();
    for _ in 0..TICKETS / TICKETS_PER_TOKEN {
        tokens.push(srv.token(TokenKind::Owner, None, None).await);
    }
    let mut first = None;
    for tok in &tokens {
        for _ in 0..TICKETS_PER_TOKEN {
            let t = srv.ticket(tok, "events").await.expect("below the caps");
            first.get_or_insert(t);
        }
        // The token's 33rd.
        busy(&srv.ticket(tok, "events").await.unwrap_err());
    }
    // The 1025th, by a token holding none.
    let fresh = srv.token(TokenKind::Owner, None, None).await;
    busy(&srv.ticket(&fresh, "events").await.unwrap_err());
    // A redemption frees a slot of its token and of the total.
    let mut ws = srv.ws_with(&first.unwrap()).await.unwrap();
    assert!(matches!(
        ws.next().await,
        Some(WsServerMessage::Hello { .. })
    ));
    srv.ticket(&tokens[0], "events")
        .await
        .expect("a slot freed");
}

async fn pair_codes_cap(srv: &TestServer) {
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let mut codes = Vec::new();
    for _ in 0..PAIR_CODES {
        codes.push(srv.pair_code(&owner).await.expect("below the cap"));
    }
    busy(&srv.pair_code(&owner).await.unwrap_err());
    assert_eq!(srv.pair(&codes[0]).await.status, 200);
    srv.pair_code(&owner).await.expect("a slot freed");
}

/// Opens `n` sockets for `token`.
async fn sockets(srv: &TestServer, token: &str, n: usize) -> Vec<WsClient> {
    let mut v = Vec::new();
    for _ in 0..n {
        v.push(srv.ws(token).await);
    }
    v
}

/// The status of an upgrade with a fresh ticket of `token`.
async fn upgrade_status(srv: &TestServer, token: &str) -> u16 {
    let ticket = srv.ticket(token, "events").await.unwrap();
    match srv.ws_with(&ticket).await {
        Ok(_) => 101,
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => {
            let body = r.body().clone().unwrap_or_default();
            assert!(
                body.windows(14).any(|w| w == b"\"retry_after\":"),
                "{body:?}"
            );
            r.status().as_u16()
        }
        Err(e) => panic!("{e}"),
    }
}

/// Closes `ws` and waits until the server has freed its slot, so the next upgrade of
/// `token` gets `101`.
async fn close_and_reopen(srv: &TestServer, mut ws: WsClient, token: &str) {
    ws.ws.close(None).await.unwrap();
    while ws.next().await.is_some() {}
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        match upgrade_status(srv, token).await {
            101 => return,
            503 => assert!(Instant::now() < until, "the slot never freed"),
            s => panic!("{s}"),
        }
        tokio::task::yield_now().await;
    }
}

async fn sockets_cap(srv: &TestServer) {
    let (max, per) = (SOCKETS, SOCKETS_PER_TOKEN);
    let mut tokens = Vec::new();
    let mut open = Vec::new();
    for _ in 0..max / per {
        let tok = srv.token(TokenKind::Owner, None, None).await;
        open.push(sockets(srv, &tok, per).await);
        // The token's 17th.
        assert_eq!(upgrade_status(srv, &tok).await, 503);
        tokens.push(tok);
    }
    // The 129th, by a token holding none.
    let fresh = srv.token(TokenKind::Owner, None, None).await;
    assert_eq!(upgrade_status(srv, &fresh).await, 503);
    let ws = open[0].pop().unwrap();
    close_and_reopen(srv, ws, &tokens[0]).await;
}

/// Reads one reply head (up to the blank line) and nothing more.
async fn read_head(c: &mut TcpStream) -> String {
    let mut got = Vec::new();
    let mut byte = [0u8; 1];
    while !got.ends_with(b"\r\n\r\n") {
        let n = waits::bounded(Duration::from_secs(10), c.read(&mut byte))
            .await
            .expect("a reply head within 10 s")
            .unwrap();
        assert_eq!(n, 1, "EOF before a reply head");
        got.push(byte[0]);
    }
    String::from_utf8(got).unwrap()
}

/// Reads `c` to its end (EOF or a reset); the bytes read.
async fn read_rest(c: &mut TcpStream) -> usize {
    let mut buf = vec![0u8; 1 << 16];
    let mut n = 0;
    loop {
        match c.read(&mut buf).await {
            Ok(0) | Err(_) => return n,
            Ok(k) => n += k,
        }
    }
}

/// A 32 MiB blob referenced from a new thread; its download request.
async fn big_blob(srv: &TestServer) -> support::http::Req {
    let t = ThreadId::from_u128(0xCA95);
    srv.append(vec![registered(t, false)]).await;
    let stream = StreamId::Thread(t);
    let bytes: Vec<u8> = (0..32 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let p = prepare_content(bytes, "application/octet-stream").unwrap();
    let Content::Blob { blob, .. } = &p.content else {
        panic!("a blob")
    };
    let sha = blob.sha256.clone();
    let mut e = raw_unknown(stream, "big", p.content.clone());
    e.blobs = p.blob.into_iter().collect();
    srv.append(vec![e]).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    srv.req("GET", &format!("/api/blobs/{sha}?stream_id={stream}"))
        .bearer(&owner)
}

/// Starts a download that reads only the reply head: it holds a blob slot.
async fn stalled_download(srv: &TestServer, req: &support::http::Req) -> TcpStream {
    let mut c = connect(srv.port).await;
    c.write_all(&req.bytes()).await.unwrap();
    let head = read_head(&mut c).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    c
}

/// A socket of a fresh token, its subscribe answered.
async fn subscribed_socket(srv: &TestServer) -> WsClient {
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let mut ws = srv.ws(&owner).await;
    ws.subscribe(&[StreamId::Host]).await;
    ws.heads_for(&[StreamId::Host], |_| {}).await;
    ws
}

/// Sends `m`, then reads to the end: the close code if a close frame came (best effort),
/// and the transport's teardown always, within `close_grace` of the refused frame.
async fn closed_after(srv: &TestServer, m: WsMessage) -> Option<u16> {
    let ws = subscribed_socket(srv).await;
    refused_with(ws, m).await
}

async fn refused_with(mut ws: WsClient, m: WsMessage) -> Option<u16> {
    let at = Instant::now();
    // The server may close while the client is still sending; the outcome is read below.
    let _ = ws.ws.send(m).await;
    let (code, _) = ws.until_closed().await;
    // After the close frame, the reply to it and then the end of the stream.
    while let Some(Ok(_)) = waits::bounded(Duration::from_secs(10), ws.ws.next())
        .await
        .expect("the transport ends within the wait")
    {}
    let bound = WsConfig::default().close_grace + Duration::from_millis(500);
    assert!(
        at.elapsed() < bound,
        "torn down {:?} after the frame",
        at.elapsed()
    );
    code
}

/// A `subscribe` of `ids` distinct thread ids, padded with spaces to `len` bytes if longer.
fn subscribe_text(ids: usize, len: usize) -> String {
    let ids: Vec<String> = (0..ids)
        .map(|i| StreamId::Thread(ThreadId::from_u128(0x1_0000 + i as u128)).to_string())
        .collect();
    let mut t = serde_json::json!({"type": "subscribe", "stream_ids": ids}).to_string();
    t.push_str(&" ".repeat(len.saturating_sub(t.len())));
    t
}

async fn socket_frame_caps(srv: &TestServer) {
    let max_subs = SUBSCRIPTIONS;
    // The last allowed subscription count, then the first refused.
    let mut ws = subscribed_socket(srv).await;
    let text = subscribe_text(max_subs, 0);
    assert!(text.len() <= MESSAGE_BYTES);
    ws.text(&text).await;
    match ws.next().await.expect("open") {
        WsServerMessage::Heads { heads } => assert!(heads.is_empty(), "none exist"),
        other => panic!("unexpected {other:?}"),
    }
    let text = subscribe_text(max_subs + 1, 0);
    assert!(text.len() <= MESSAGE_BYTES);
    assert_eq!(
        closed_after(srv, WsMessage::Text(text.into())).await,
        Some(1008)
    );
    // An inbound message of exactly the bound, then one byte over it.
    let mut ws = subscribed_socket(srv).await;
    let exact = subscribe_text(1, MESSAGE_BYTES);
    assert_eq!(exact.len(), MESSAGE_BYTES);
    ws.text(&exact).await;
    assert!(matches!(
        ws.next().await,
        Some(WsServerMessage::Heads { .. })
    ));
    // One byte over: the server may close while the client is still writing, so the
    // close code is asserted exactly in `oversized_message_closes_with_1009`.
    let over = subscribe_text(1, MESSAGE_BYTES + 1);
    let code = closed_after(srv, WsMessage::Text(over.into())).await;
    assert!(matches!(code, Some(1009) | None), "{code:?}");
    // Binary and invalid frames.
    assert_eq!(
        closed_after(srv, WsMessage::Binary(vec![1, 2, 3].into())).await,
        Some(1003)
    );
    for invalid in [
        "not json",
        r#"{"type":"unsubscribe"}"#,
        r#"{"type":"subscribe","stream_ids":["host"],"extra":1}"#,
    ] {
        assert_eq!(
            closed_after(srv, WsMessage::Text(invalid.into())).await,
            Some(1003),
            "{invalid}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_cap_at_its_boundary() {
    // A server per cap: a full ticket table would refuse the other cases' tickets.
    let srv = quiet(|_| {}).await;
    tickets_cap(&srv).await;
    srv.stop().await;
    let srv = quiet(|_| {}).await;
    pair_codes_cap(&srv).await;
    srv.stop().await;
    let srv = quiet(|_| {}).await;
    sockets_cap(&srv).await;
    srv.stop().await;
    let srv = quiet(|_| {}).await;
    socket_frame_caps(&srv).await;
    srv.stop().await;
}

/// An inbound message over a small `max_message_bytes`, sent whole before the server
/// reads it: close code 1009 exactly, then the teardown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_message_closes_with_1009() {
    let srv = quiet(|c| c.ws.max_message_bytes = 64).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ws = srv.ws(&owner).await;
    let code = refused_with(ws, WsMessage::Text(subscribe_text(0, 65).into())).await;
    assert_eq!(code, Some(1009));
    srv.stop().await;
}

/// The blob slots' cap: all four held by clients that never read past the head (the last
/// allowed), a download is refused with 503 `busy` while they hold them (the first refused); their connections end at the write stall, before the
/// whole body was sent; and the next download acquires a slot and completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob_slot_frees_after_a_stalled_client() {
    let srv = quiet(|c| c.write_stall_timeout = Duration::from_millis(500)).await;
    let req = big_blob(&srv).await;
    let base = srv.server.tasks();
    let mut held = Vec::new();
    for _ in 0..BLOB_SLOTS {
        held.push(stalled_download(&srv, &req).await);
    }
    busy(&srv.send(&req).await);
    let until = Instant::now() + Duration::from_secs(20);
    let r = loop {
        let r = srv.send(&req).await;
        if r.status != 503 {
            break r;
        }
        assert!(Instant::now() < until, "the slots never freed");
        tokio::task::yield_now().await;
    };
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body.len(), 32 * 1024 * 1024);
    // Every stalled connection was ended by the server (its task is gone while the client
    // still has not read), short of the body.
    support::http::eventually(
        Duration::from_secs(10),
        "the stalled connections ended",
        || srv.server.tasks() <= base,
    )
    .await;
    for mut c in held {
        let got = waits::bounded(Duration::from_secs(10), read_rest(&mut c))
            .await
            .expect("the stalled connection ends within the wait");
        assert!(got < 32 * 1024 * 1024, "the whole body arrived: {got}");
    }
    srv.stop().await;
}

/// A refused frame on a socket whose peer never answers the close: the server stops
/// waiting at `close_grace` and the socket task ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_ends_without_the_peer_acknowledging() {
    let srv = quiet(|c| c.ws.close_grace = Duration::from_millis(300)).await;
    let base = srv.server.tasks();
    let mut ws = subscribed_socket(&srv).await;
    ws.ws
        .send(WsMessage::Text("not json".into()))
        .await
        .unwrap();
    let (code, _) = ws.until_closed().await;
    assert_eq!(code, Some(1003));
    // The client stream is not polled again: its reply to the close is queued, never
    // flushed. The client stays open while the server is observed.
    support::http::eventually(
        Duration::from_secs(2),
        "the unanswered socket ended",
        || srv.server.tasks() <= base,
    )
    .await;
    drop(ws);
    srv.stop().await;
}

/// The last allowed subscription count and message size keep an existing stream: 1023
/// unknown ids plus `host`, and `host` alone padded to exactly 64 KiB.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_boundaries_keep_an_existing_stream() {
    let srv = quiet(|_| {}).await;
    let mut ws = subscribed_socket(&srv).await;
    let mut mixed: serde_json::Value =
        serde_json::from_str(&subscribe_text(SUBSCRIPTIONS - 1, 0)).unwrap();
    mixed["stream_ids"]
        .as_array_mut()
        .unwrap()
        .push("host".into());
    let one = r#"{"type":"subscribe","stream_ids":["host"]}"#;
    let padded = one.to_owned() + &" ".repeat(MESSAGE_BYTES - one.len());
    let mut got = Vec::new();
    for text in [mixed.to_string(), padded] {
        assert!(text.len() <= MESSAGE_BYTES);
        ws.text(&text).await;
        match ws.next().await.expect("open") {
            WsServerMessage::Heads { heads } => {
                got.push(heads.iter().map(|e| e.stream_id).collect::<Vec<_>>());
            }
            other => panic!("expected heads, got {other:?}"),
        }
    }
    assert_eq!(got, [[StreamId::Host], [StreamId::Host]]);
    srv.stop().await;
}
