//! Hook unit tests that need the HTTP and store fault points or crate internals
//! (`docs/design/hook-ingest.md` §7.2). Tests that need only the public API are the
//! integration tests `crates/daemon/tests/hook_*.rs`.

pub(crate) mod points;

mod handler;
mod intake;

use std::time::Duration;

use crate::store::tests::support;
use support::hook::{Hook, HookServer, Launch};
use support::http::Reply;

/// Waits (at most 10 s) for a gate's `reached` message.
async fn reached(rx: tokio::sync::oneshot::Receiver<()>) {
    support::http::reached(rx).await;
}

/// Waits (at most 10 s) for a blocking gate's `reached` message.
async fn reached_sync(rx: std::sync::mpsc::Receiver<()>) {
    support::http::reached_sync(rx).await;
}

fn ok(r: &Reply) {
    assert_eq!((r.status, r.body.as_slice()), (200, &b""[..]), "{r:?}");
}

/// Binds `l` (Codex) to `session` through `/hook`.
async fn bind(h: &HookServer, l: &Launch, session: &str) {
    ok(&h.post(&Hook::of(l, "SessionStart", session)).await);
    assert_eq!(h.launch_row(l.launch).session_id.as_deref(), Some(session));
}

/// The store's annex text of the event under `key` in any stream.
fn annex_of(h: &HookServer, key: &str) -> Option<String> {
    let c = rusqlite::Connection::open_with_flags(
        h.srv.store.path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    c.query_row(
        "SELECT annex FROM events WHERE idempotency_key = ?1",
        [key],
        |r| r.get(0),
    )
    .unwrap()
}

const WAIT: Duration = Duration::from_secs(10);
