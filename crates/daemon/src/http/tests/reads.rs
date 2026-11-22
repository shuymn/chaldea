//! Read outcomes and the one cutoff (design §2.3, §2.9; PR7-CL-29, CL-3's lookup cases).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use axum::http::Method;
use chaldea_core::auth::TokenKind;
use futures_util::StreamExt;

use super::support::http::TestServer;
use super::{handshake, hello_then_hold, mint_ticket, shared};
use crate::http::ctx::read_until;
use crate::http::tickets::TicketPurpose;
use crate::http::{ApiError, MonotonicClock, Mount, Mounts, ReadCtx, spec};
use crate::store::StoreError;
use axum::body::Body;

/// Holds every reader of the pool until the returned sender is dropped.
fn hold_readers(srv: &TestServer, n: usize) -> mpsc::Sender<()> {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
    let (held_tx, held_rx) = mpsc::channel::<()>();
    for _ in 0..n {
        let store = srv.store.store().clone();
        let held = held_tx.clone();
        let release = release_rx.clone();
        std::thread::spawn(move || {
            let _ = store.read(Instant::now() + Duration::from_secs(5), move |_tx| {
                held.send(()).unwrap();
                // Released when the sender drops; every holder then returns.
                let _ = release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(20));
                Ok(())
            });
        });
    }
    for _ in 0..n {
        held_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    }
    release_tx
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_outcomes_map() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let s = shared(&srv);
    let soon = || Instant::now() + Duration::from_secs(5);

    // A store outcome returned before the cutoff maps through §2.9.
    let r = read_until(&s, soon(), |_| Err::<(), _>(StoreError::Busy)).await;
    assert_eq!(r, Err(ApiError::busy()));
    let r = read_until(&s, soon(), |_| Err::<(), _>(StoreError::DiskFull)).await;
    assert_eq!(r, Err(ApiError::internal()));
    assert_eq!(read_until(&s, soon(), |_| Ok(7)).await, Ok(7));

    // A read held running past the cutoff: 408.
    let (release_tx, release_rx) = mpsc::channel::<()>();
    s.faults.inner.arm_before_read(0, move || {
        let _ = release_rx.recv_timeout(Duration::from_secs(20));
    });
    let r = read_until(&s, Instant::now() + Duration::from_millis(300), |_| Ok(1)).await;
    assert_eq!(r, Err(ApiError::request_timeout()));
    drop(release_tx);

    // The reader pool exhausted through the cutoff: 503 `busy` or 408, never a success.
    let release = hold_readers(&srv, 4);
    let r = read_until(&s, Instant::now() + Duration::from_millis(300), |_| Ok(1)).await;
    assert!(
        r == Err(ApiError::busy()) || r == Err(ApiError::request_timeout()),
        "{r:?}"
    );
    drop(release);

    // The store shut down: 503 `shutting_down`.
    let mut srv = srv;
    srv.store.shutdown();
    let r = read_until(&s, soon(), |_| Ok(1)).await;
    assert_eq!(r, Err(ApiError::shutting_down()));
    srv.shutdown_http(Instant::now() + Duration::from_secs(10))
        .await;
}

/// A successful result taken at or after the deadline is discarded: the server's clock is
/// moved past the deadline after the read returned, so the HTTP task resumes with the
/// result ready and the deadline passed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_read_success_is_408() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let s = shared(&srv);
    let clock = srv.clock.clone();
    s.faults
        .inner
        .arm_after_read(0, move || clock.skew(Duration::from_secs(3600)));
    let r = read_until(&s, Instant::now() + Duration::from_secs(5), |_| Ok(1)).await;
    assert_eq!(r, Err(ApiError::request_timeout()));
    srv.clock.skew(Duration::ZERO);
    srv.stop().await;
}

/// The same for a store error other than reader-pool exhaustion: taken after the cutoff it
/// is 408, not its §2.9 mapping. The waiter is held at its gate while the `DiskFull` read
/// finishes and the real cutoff passes; released, it polls with both the result and the
/// timer ready, and `timeout_at` takes the ready result, so only the explicit check decides.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_read_error_is_408() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let s = shared(&srv);
    let (reached, release) = s.faults.inner.arm_cutoff_gate();
    let w = s.clone();
    let deadline = srv.clock.now() + Duration::from_millis(200);
    let waiter = tokio::spawn(async move {
        read_until(&w, deadline, |_| Err::<(), _>(StoreError::DiskFull)).await
    });
    tokio::time::timeout(Duration::from_secs(10), reached)
        .await
        .expect("the waiter reached its gate")
        .unwrap();
    // A lower bound: the real cutoff (the deadline itself, the clock not skewed) has passed.
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        deadline + Duration::from_millis(50),
    ))
    .await;
    drop(release);
    let r = tokio::time::timeout(Duration::from_secs(20), waiter)
        .await
        .expect("the read ends")
        .unwrap();
    assert_eq!(r, Err(ApiError::request_timeout()));
    srv.stop().await;
}

/// `Shared::real` converts with the clock's own state: a thread preempted while converting
/// (here, a clock whose reading takes 50 ms) does not move the deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conversion_reads_no_clock() {
    struct Preempted;
    impl MonotonicClock for Preempted {
        fn now(&self) -> Instant {
            std::thread::sleep(Duration::from_millis(50));
            Instant::now()
        }

        fn real(&self, at: Instant) -> Instant {
            at
        }
    }
    let srv =
        TestServer::start_with(Mounts::mvp(), |c| c.clock = std::sync::Arc::new(Preempted)).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    assert_eq!(shared(&srv).real(deadline), deadline);
    srv.stop().await;
}

/// A body complete only after the deadline is 408, even when it is ready at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_body_is_408() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let ctx = ReadCtx::new(None, Instant::now() - Duration::from_secs(1), shared(&srv));
    let r = ctx.body_within(Body::from("late"), 4).await;
    assert_eq!(r, Err(ApiError::request_timeout()));
    srv.stop().await;
}

/// The same for the final ticket admission read: 408, no `101`, and the ticket is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_admission_read_keeps_the_ticket() {
    let mounts = Mounts::mvp().with(Mount::upgrade(
        spec(&Method::GET, "/api/ws").unwrap(),
        hello_then_hold,
    ));
    let srv = TestServer::start(mounts).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let ticket = mint_ticket(&srv, &owner, TicketPurpose::Events);
    let s = shared(&srv);
    let clock = srv.clock.clone();
    // Read 1 is the credential read, read 2 the final admission read.
    s.faults
        .inner
        .arm_after_read(1, move || clock.skew(Duration::from_secs(3600)));
    let r = srv.send(&handshake(&srv, &ticket, "Upgrade")).await;
    assert_eq!(r.outcome(), (408, Some("request_timeout".to_owned())));
    srv.clock.skew(Duration::ZERO);
    // The ticket was not spent: it redeems now.
    let url = format!("ws://127.0.0.1:{}/api/ws?ticket={ticket}", srv.port);
    let (mut ws, resp) = tokio_tungstenite::connect_async(url).await.unwrap();
    assert_eq!(resp.status(), 101);
    let hello = ws.next().await.unwrap().unwrap();
    assert!(hello.to_text().unwrap().contains("\"hello\""));
    ws.close(None).await.unwrap();
    srv.stop().await;
}

/// A token lookup that fails or is cut off is never 401 (PR7-CL-3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lookup_failure_is_not_unauthorized() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let req = srv.req("GET", "/api/threads").bearer(&owner);

    // The reader pool exhausted through the cutoff: 503 `busy` or 408.
    let release = hold_readers(&srv, 4);
    let r = srv.send(&req).await;
    assert!(
        [
            (503, Some("busy".to_owned())),
            (408, Some("request_timeout".to_owned()))
        ]
        .contains(&r.outcome()),
        "{r:?}"
    );
    drop(release);
    // The store closed: 503 `shutting_down`.
    let mut srv = srv;
    srv.store.shutdown();
    let r = srv.send(&req).await;
    assert_eq!(r.outcome(), (503, Some("shutting_down".to_owned())));
    srv.shutdown_http(Instant::now() + Duration::from_secs(10))
        .await;
}
