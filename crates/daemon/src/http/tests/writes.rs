//! Plain writes at the cutoff (design §2.3, §2.8; PR7-CL-8): a body completed after the
//! deadline submits nothing, and a writer reply taken after `deadline + write_reply_grace`
//! is 503 `outcome_unknown` whatever it says.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, Response};
use chaldea_core::auth::TokenKind;

use super::shared;
use super::support::http::{SkewClock, TestServer};
use crate::http::{Mount, Mounts, WriteCtx, spec};

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_after_the_grace_is_outcome_unknown() {
    let clock = Arc::new(SkewClock::default());
    let c = clock.clone();
    // The job itself moves the server's clock past `deadline + write_reply_grace`, so its
    // reply is ready when the request takes it, and the cutoff has passed.
    let mounts = Mounts::mvp().with(Mount::write(
        spec(&Method::POST, "/hook").unwrap(),
        move |ctx: WriteCtx, _req: Request<Body>| {
            let c = c.clone();
            async move {
                ctx.write(move |_tx, _auth| {
                    c.skew(Duration::from_secs(3600));
                    Ok(())
                })
                .await?;
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
    assert_eq!(r.outcome(), (503, Some("outcome_unknown".to_owned())));
    // The job was submitted and its observer saw it through: nothing fatal was reported.
    assert_eq!(s.faults.inner.submits(), before + 1);
    assert!(srv.sink.0.lock().unwrap().is_empty());
    srv.stop().await;
}
