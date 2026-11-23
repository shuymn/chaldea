//! Plain writes at the cutoff (design §2.3, §2.8; PR7-CL-8): a body completed after the
//! deadline submits nothing, and a writer reply taken after `deadline + write_reply_grace`
//! is 503 `outcome_unknown` whatever it says, a late `Expired` included.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, Response};
use chaldea_core::auth::TokenKind;

use super::eventually;
use super::shared;
use super::support::http::{SkewClock, TestServer};
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
    let hook = srv.token_of(TokenKind::Hook, 1).await;
    let s = shared(&srv);
    let before = s.faults.submits();
    let r = srv
        .send(&srv.req("POST", "/hook").bearer(&hook).body(b"{}"))
        .await;
    clock.skew(Duration::ZERO);
    assert_eq!(r.outcome(), (408, Some("request_timeout".to_owned())));
    assert_eq!(s.faults.submits(), before, "a write was submitted");
    srv.stop().await;
}

/// The waiter is held at its gate while its job waits behind a held writer past
/// `deadline + write_reply_grace` (and so past its `not_after`); the writer is then released
/// and replies `Expired`. Released, the waiter polls with both the reply and the timer ready,
/// and `timeout_at` takes the ready reply, so only the explicit check decides: 503
/// `outcome_unknown`, not 408.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_after_the_grace_is_outcome_unknown() {
    let srv = TestServer::start_with(Mounts::mvp(), |c| {
        c.write_reply_grace = Duration::from_millis(100);
    })
    .await;
    let s = shared(&srv);
    let writer = srv.hold_writer().await;
    let (reached, release) = s.faults.arm_cutoff_gate();
    let before = s.faults.submits();
    let deadline = srv.clock.now() + Duration::from_millis(300);
    let w = s.clone();
    let waiter =
        tokio::spawn(async move { await_write(&w, deadline, Box::new(|_tx| Ok(())), None).await });
    tokio::time::timeout(Duration::from_secs(10), reached)
        .await
        .expect("the waiter reached its gate")
        .unwrap();
    eventually(Duration::from_secs(10), "the job was submitted", || {
        s.faults.submits() == before + 1
    })
    .await;
    // Lower bounds: the reply cutoff and the job's `not_after` have passed in real time.
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        deadline + Duration::from_millis(150),
    ))
    .await;
    writer.send(()).unwrap();
    drop(release);
    let r = tokio::time::timeout(Duration::from_secs(20), waiter)
        .await
        .expect("the write reply was taken")
        .unwrap();
    assert_eq!(r, Err(ApiError::outcome_unknown()));
    assert!(srv.sink.0.lock().unwrap().is_empty());
    srv.stop().await;
}
