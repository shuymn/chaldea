//! The upgrade path through the 7a test upgrade mount (design §2.2, §2.4 "Redemption";
//! PR7-CL-7's `101` case, PR7-CL-28's socket case).

use std::time::{Duration, Instant};

use axum::http::Method;
use chaldea_core::auth::TokenKind;
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::support::http::{TestServer, connect};
use super::{handshake, hello_then_hold, mint_ticket};
use crate::http::tickets::TicketPurpose;
use crate::http::{Mount, Mounts, UpgradedSocket, spec};

fn ws_mounts() -> Mounts {
    Mounts::mvp().with(Mount::upgrade(
        spec(&Method::GET, "/api/ws").unwrap(),
        hello_then_hold,
    ))
}

/// Reads one reply head (up to the blank line) without waiting for EOF.
async fn read_head(c: &mut tokio::net::TcpStream) -> String {
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(10), c.read(&mut buf))
            .await
            .expect("a reply head within 10 s")
            .unwrap();
        assert!(
            n > 0,
            "EOF before a reply head: {:?}",
            String::from_utf8_lossy(&got)
        );
        got.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&got).into_owned()
}

async fn redeem_with_client(srv: &TestServer, ticket: &str) {
    let url = format!("ws://127.0.0.1:{}/api/ws?ticket={ticket}", srv.port);
    let (mut ws, resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("the pinned client accepts the 101");
    assert_eq!(resp.status(), 101);
    let hello = ws.next().await.unwrap().unwrap();
    assert!(hello.to_text().unwrap().contains("\"hello\""), "{hello:?}");
    ws.close(None).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade_keeps_connection_upgrade() {
    let srv = TestServer::start(ws_mounts()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;

    // The raw 101: exactly one `Connection` header, exactly `Upgrade`.
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let mut c = connect(srv.port).await;
    c.write_all(&handshake(&srv, &ticket, "Upgrade").bytes())
        .await
        .unwrap();
    let head = read_head(&mut c).await;
    assert!(head.starts_with("HTTP/1.1 101 "), "{head}");
    let connection: Vec<&str> = head
        .lines()
        .filter_map(|l| l.split_once(':'))
        .filter(|(k, _)| k.eq_ignore_ascii_case("connection"))
        .map(|(_, v)| v.trim())
        .collect();
    assert_eq!(connection, ["Upgrade"], "{head}");
    drop(c);

    // The pinned client completes the handshake and receives `hello`.
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    redeem_with_client(&srv, &ticket).await;
    srv.stop().await;
}

/// An upgrade request whose `Connection` carries `close` is 400 before the ticket is
/// removed: the same ticket redeems afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade_asking_close_is_400() {
    let srv = TestServer::start(ws_mounts()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    for connection in ["Upgrade, close", "close, Upgrade", "upgrade,CLOSE"] {
        let r = srv.send(&handshake(&srv, &ticket, connection)).await;
        assert_eq!(
            r.outcome(),
            (400, Some("bad_request".to_owned())),
            "{connection}"
        );
        assert_eq!(r.header("connection"), Some("close"));
    }
    // A request that is not a handshake is 400 too, and keeps the ticket.
    let r = srv
        .send(&srv.req("GET", &format!("/api/ws?ticket={ticket}")))
        .await;
    assert_eq!(r.outcome(), (400, Some("bad_request".to_owned())));
    redeem_with_client(&srv, &ticket).await;
    // Spent now.
    let r = srv.send(&handshake(&srv, &ticket, "Upgrade")).await;
    assert_eq!(r.outcome(), (401, Some("unauthorized".to_owned())));
    srv.stop().await;
}

/// A `Connection` value that is not UTF-8 still counts its `close` token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_utf8_connection_close_is_400() {
    let srv = TestServer::start(ws_mounts()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let mut bytes = handshake(&srv, &ticket, "Upgrade").head();
    // A second `Connection` header, `close, \xff`, just before the blank line.
    bytes.truncate(bytes.len() - 2);
    bytes.extend_from_slice(b"Connection: close, \xff\r\n\r\n");
    let r = super::support::http::send(srv.port, &bytes).await;
    assert_eq!(r.outcome(), (400, Some("bad_request".to_owned())));
    redeem_with_client(&srv, &ticket).await;
    srv.stop().await;
}

/// An HTTP/1.0 request cannot upgrade: the handshake is 400 and the ticket is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http10_upgrade_is_400() {
    let srv = TestServer::start(ws_mounts()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let bytes = handshake(&srv, &ticket, "Upgrade").http10_bytes();
    let r = super::support::http::send(srv.port, &bytes).await;
    assert_eq!(r.outcome(), (400, Some("bad_request".to_owned())), "{r:?}");
    assert_eq!(r.header("connection"), Some("close"));
    redeem_with_client(&srv, &ticket).await;
    srv.stop().await;
}

/// The socket task waits for its admission: its hand-over bound starts only after the
/// ticket is removed, so a spent ticket always has a task waiting for its socket. The
/// admission is held between the final read and the removal until the socket task has
/// parked; no hand-over bound has started by then.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hand_over_bound_starts_after_the_ticket_is_removed() {
    let srv = TestServer::start(ws_mounts()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let s = super::shared(&srv);
    let (reached, release) = s.faults.arm_removal_gate();
    let parked = s.faults.arm_first_park();
    let url = format!("ws://127.0.0.1:{}/api/ws?ticket={ticket}", srv.port);
    let client = tokio::spawn(tokio_tungstenite::connect_async(url));
    for (what, signal) in [
        ("admission reached the gate", reached),
        ("the socket task parked", parked),
    ] {
        tokio::time::timeout(Duration::from_secs(10), signal)
            .await
            .expect(what)
            .unwrap();
    }
    assert_eq!(s.faults.steps(), Vec::<&str>::new());
    drop(release);
    let (mut ws, resp) = tokio::time::timeout(Duration::from_secs(10), client)
        .await
        .expect("the handshake ends")
        .unwrap()
        .expect("a 101");
    assert_eq!(resp.status(), 101);
    let hello = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("hello within 5 s: the socket was handed over")
        .unwrap()
        .unwrap();
    assert!(hello.to_text().unwrap().contains("\"hello\""), "{hello:?}");
    assert_eq!(
        s.faults.steps(),
        ["ticket removed", "hand-over bound started"]
    );
    ws.close(None).await.unwrap();
    srv.stop().await;
}

/// A socket that ignores shutdown is still in the registry at the deadline: it gets an
/// abort request and is counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_aborts_a_held_socket() {
    let (entered_tx, mut entered) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mounts = Mounts::mvp().with(Mount::upgrade(
        spec(&Method::GET, "/api/ws").unwrap(),
        move |_s: UpgradedSocket| {
            let _ = entered_tx.send(());
            std::future::pending::<()>()
        },
    ));
    let mut srv = TestServer::start(mounts).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let url = format!("ws://127.0.0.1:{}/api/ws?ticket={ticket}", srv.port);
    let (_ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    entered.recv().await.unwrap();
    let deadline = Instant::now() + Duration::from_millis(500);
    let report = srv.shutdown_http(deadline).await;
    assert!(
        Instant::now() < deadline + Duration::from_millis(500),
        "late"
    );
    assert_eq!(report.abort_requested.socket, 1, "{report:?}");
    assert_eq!(report.exited.accept, 1, "{report:?}");
    srv.store.shutdown();
}
