//! HTTP unit tests that need fault points or crate internals (design §7). Tests that need
//! only the public API are the integration tests `crates/daemon/tests/http_*.rs`.

pub(crate) mod faults;

/// The shared test support, included once by the store's unit tests.
pub(crate) use crate::store::tests::support;

mod auth;
mod errors;
mod layers;
mod listener;
mod reads;
mod registry;
mod upgrade;
mod writes;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::Message;
use chaldea_core::sync::WsServerMessage;
use sha2::{Digest, Sha256};

use super::tickets::{TicketEntry, TicketPurpose};
use super::{Shared, UpgradedSocket};
use support::http::{Req, TestServer};

pub(crate) fn shared(srv: &TestServer) -> Arc<Shared> {
    srv.server.shared().clone()
}

/// Mints a ticket for the token `secret` directly in the server's ticket store (the
/// minting route is PR 7b's).
pub(crate) fn mint_ticket(srv: &TestServer, secret: &str, purpose: TicketPurpose) -> String {
    let sha: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
    let row = srv
        .store
        .read(move |tx| tx.token_by_hash(&sha))
        .expect("token exists");
    let s = shared(srv);
    s.tickets
        .mint(TicketEntry {
            token_id: row.token_id,
            token_sha256: sha,
            kind: row.kind,
            thread: row.thread_id,
            purpose,
            expires_at: s.cfg.clock.now() + s.cfg.ticket_ttl,
        })
        .expose()
        .to_owned()
}

/// A WebSocket handshake for `/api/ws` with `ticket` and the `Connection` value
/// `connection`; the key is the sample nonce of RFC 6455 §1.3.
pub(crate) fn handshake(srv: &TestServer, ticket: &str, connection: &str) -> Req {
    use base64::Engine;
    let key = base64::engine::general_purpose::STANDARD.encode(b"the sample nonce");
    srv.req("GET", &format!("/api/ws?ticket={ticket}"))
        .header("Connection", connection)
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", &key)
}

/// A test upgrade handler: sends `hello`, then holds the socket until the client closes or
/// shutdown begins.
pub(crate) async fn hello_then_hold(mut s: UpgradedSocket) {
    let hello = WsServerMessage::Hello {
        epoch: chaldea_core::ids::Epoch::from("test"),
        server_time: chaldea_core::ids::Timestamp::from("2026-11-22T00:00:00.000Z"),
    };
    let text = serde_json::to_string(&hello).unwrap();
    if s.socket.send(Message::Text(text.into())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            () = s.shutdown.wait() => return,
            m = s.socket.recv() => if !matches!(m, Some(Ok(_))) { return },
        }
    }
}

/// Waits until `cond` holds, polling with yields, at most `within`.
pub(crate) async fn eventually(within: Duration, what: &str, mut cond: impl FnMut() -> bool) {
    let until = std::time::Instant::now() + within;
    while !cond() {
        assert!(
            std::time::Instant::now() < until,
            "not within {within:?}: {what}"
        );
        tokio::task::yield_now().await;
    }
}
