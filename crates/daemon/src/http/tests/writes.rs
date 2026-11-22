//! Plain writes at the cutoff (design §2.3, §2.8; PR7-CL-8): a body completed after the
//! deadline submits nothing, and a writer reply taken after `deadline + write_reply_grace`
//! is 503 `outcome_unknown` whatever it says, a late `Expired` included.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, Response};
use chaldea_core::auth::TokenKind;

use super::shared;
use super::support::http::{SkewClock, TestServer};
use super::{eventually, support};
use crate::http::ctx::await_write;
use crate::http::{ApiError, MonotonicClock, Mount, Mounts, WriteCtx, spec};

/// A server over `mounts` whose clock is `clock`.
async fn start(mounts: Mounts, clock: &Arc<SkewClock>) -> TestServer {
    let clock = clock.clone();
    TestServer::start_with(mounts, move |c| c.clock = clock).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_after_the_cutoff_submits_nothing() {
    let clock = Arc::new(SkewClock::default());
    let c = clock.clone();
    // The handler passes the deadline (on the server's clock) before it collects a body
    // that is already complete, then tries to write.
    let mounts = Mounts::mvp().with(Mount::write(
        spec(&Method::POST, "/hook").unwrap(),
        move |ctx: WriteCtx, req: Request<Body>| {
            c.skew(Duration::from_secs(3600));
            async move {
                ctx.body_within(req.into_body(), 1024).await?;
                ctx.write(|_tx, _auth| Ok(())).await?;
                Ok(Response::new(Body::from("ok")))
            }
        },
    ));
    let srv = start(mounts, &clock).await;
    let hook = srv.token(TokenKind::Hook, None, None).await;
    let s = shared(&srv);
    let before = s.faults.inner.submits();
    let r = srv
        .send(&srv.req("POST", "/hook").bearer(&hook).body(b"{}"))
        .await;
    clock.skew(Duration::ZERO);
    assert_eq!(r.outcome(), (408, Some("request_timeout".to_owned())));
    assert_eq!(s.faults.inner.submits(), before, "a write was submitted");
    srv.stop().await;
}

/// Occupies the store writer until the returned sender is used or dropped.
async fn hold_writer(srv: &TestServer) -> std::sync::mpsc::Sender<()> {
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let ticket = srv
        .store
        .store()
        .submit(support::soon(), move |_tx| {
            let _ = entered_tx.send(());
            let _ = wait.recv_timeout(Duration::from_secs(20));
            Ok(())
        })
        .unwrap();
    tokio::spawn(ticket);
    entered.await.unwrap();
    release
}

/// The reply cutoff is set first (held at its gate); the job waits behind a held writer past
/// its `not_after`; the server's clock then passes `deadline + write_reply_grace`; the writer
/// is released and replies `Expired`. The request resumes with that reply ready and its real
/// timer still pending, so only the explicit check decides: 503 `outcome_unknown`, not 408.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_after_the_grace_is_outcome_unknown() {
    let srv = TestServer::start_with(Mounts::mvp(), |c| {
        c.write_reply_grace = Duration::from_secs(10);
    })
    .await;
    let s = shared(&srv);
    let writer = hold_writer(&srv).await;
    let (reached, release) = s.faults.inner.arm_cutoff_gate();
    let before = s.faults.inner.submits();
    let deadline = srv.clock.now() + Duration::from_millis(300);
    let w = s.clone();
    let waiter =
        tokio::spawn(async move { await_write(&w, deadline, Box::new(|_tx| Ok(())), None).await });
    tokio::time::timeout(Duration::from_secs(10), reached)
        .await
        .expect("the cutoff was set")
        .unwrap();
    drop(release);
    eventually(Duration::from_secs(10), "the job was submitted", || {
        s.faults.inner.submits() == before + 1
    })
    .await;
    // The store decides expiry on real time: let the job's `not_after` pass.
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        deadline + Duration::from_millis(50),
    ))
    .await;
    srv.clock.skew(Duration::from_secs(3600));
    writer.send(()).unwrap();
    let r = tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .expect("the write reply was taken")
        .unwrap();
    srv.clock.skew(Duration::ZERO);
    assert_eq!(r, Err(ApiError::outcome_unknown()));
    assert!(srv.sink.0.lock().unwrap().is_empty());
    srv.stop().await;
}
