//! Read outcomes and the one cutoff (design §2.3, §2.9; PR7-CL-29, CL-3's lookup cases).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use axum::http::Method;
use chaldea_core::auth::TokenKind;
use futures_util::StreamExt;

use super::support::http::{DEADLINE, TestServer};
use super::{handshake, hello_then_hold, mint_ticket, shared};
use crate::http::ctx::read_until;
use crate::http::tickets::TicketPurpose;
use crate::http::{ApiError, MonotonicClock, Mount, Mounts, ReadCtx, spec};
use crate::store::StoreError;
use crate::store::tests::support::waits;
use axum::body::Body;

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
    s.faults.arm_before_read(0, move || {
        let _ = release_rx.recv_timeout(Duration::from_secs(20));
    });
    let r = read_until(&s, Instant::now() + Duration::from_millis(300), |_| Ok(1)).await;
    assert_eq!(r, Err(ApiError::request_timeout()));
    drop(release_tx);

    // The reader pool exhausted through the cutoff: 503 `busy` or 408, never a success.
    let release = srv.hold_readers(4);
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
    let (reached, release) = s.faults.arm_cutoff_gate();
    let w = s.clone();
    let deadline = srv.clock.now() + Duration::from_millis(200);
    let waiter = tokio::spawn(async move {
        read_until(&w, deadline, |_| Err::<(), _>(StoreError::DiskFull)).await
    });
    waits::bounded(Duration::from_secs(10), reached)
        .await
        .expect("the waiter reached its gate")
        .unwrap();
    // A lower bound: the real cutoff (the deadline itself, the clock not skewed) has passed.
    waits::pause_until(tokio::time::Instant::from_std(
        deadline + Duration::from_millis(50),
    ))
    .await;
    drop(release);
    let r = waits::bounded(Duration::from_secs(20), waiter)
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
    let srv = TestServer::start_with(Mounts::mvp(), |c| c.short_deadline = DEADLINE).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let req = srv.req("GET", "/api/threads").bearer(&owner);

    // The reader pool exhausted through the cutoff: 503 `busy` or 408.
    let release = srv.hold_readers(4);
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

/// The cutoff precedes the outcome: a failed join (the closure panicked) taken after the
/// cutoff is 408, not 500, for `WriteCtx::read_by` and `read_until`; reader-pool exhaustion
/// through `by` is 408 for `read_by` (not 503 `busy`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_by_cutoff_precedes_the_outcome() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let s = shared(&srv);
    let ctx = std::sync::Arc::new(crate::http::WriteCtx::new(
        None,
        srv.clock.now() + Duration::from_secs(10),
        s.clone(),
    ));
    let (reached, release) = s.faults.arm_cutoff_gate();
    let by = srv.clock.now() + Duration::from_millis(200);
    let c = ctx.clone();
    let waiter = tokio::spawn(async move {
        c.read_by(by, |_| -> Result<(), StoreError> {
            chaldea_pty_runtime::fake::inject_panic("injected read panic")
        })
        .await
    });
    waits::bounded(Duration::from_secs(10), reached)
        .await
        .expect("the waiter reached its gate")
        .unwrap();
    waits::pause_until(tokio::time::Instant::from_std(
        by + Duration::from_millis(50),
    ))
    .await;
    drop(release);
    let r = waits::bounded(Duration::from_secs(20), waiter)
        .await
        .expect("the read ends")
        .unwrap();
    assert_eq!(r, Err(ApiError::request_timeout()));

    // `read_until` (every read's cutoff) takes the clock before the join too.
    let (reached, release) = s.faults.arm_cutoff_gate();
    let deadline = srv.clock.now() + Duration::from_millis(200);
    let w = s.clone();
    let waiter = tokio::spawn(async move {
        read_until(&w, deadline, |_| -> Result<(), StoreError> {
            chaldea_pty_runtime::fake::inject_panic("injected read panic")
        })
        .await
    });
    waits::bounded(Duration::from_secs(10), reached)
        .await
        .expect("the waiter reached its gate")
        .unwrap();
    waits::pause_until(tokio::time::Instant::from_std(
        deadline + Duration::from_millis(50),
    ))
    .await;
    drop(release);
    let r = waits::bounded(Duration::from_secs(20), waiter)
        .await
        .expect("the read ends")
        .unwrap();
    assert_eq!(r, Err(ApiError::request_timeout()));

    // Exhaustion: the pool's wait ends at `by` with `Busy`, which is ready when the waiter,
    // held at its gate past `by`, takes it.
    let readers = srv.hold_readers(4);
    let (reached, release) = s.faults.arm_cutoff_gate();
    let by = srv.clock.now() + Duration::from_millis(200);
    let c = ctx.clone();
    let waiter = tokio::spawn(async move { c.read_by(by, |_| Ok(())).await });
    waits::bounded(Duration::from_secs(10), reached)
        .await
        .expect("the waiter reached its gate")
        .unwrap();
    waits::pause_until(tokio::time::Instant::from_std(
        by + Duration::from_millis(300),
    ))
    .await;
    drop(release);
    let r = waits::bounded(Duration::from_secs(20), waiter)
        .await
        .expect("the read ends")
        .unwrap();
    assert_eq!(r, Err(ApiError::request_timeout()));
    drop(readers);
    drop(ctx);
    srv.stop().await;
}

/// Reader-pool exhaustion on a plain (non-strict) read stays 503 `busy` when its result is
/// taken after the cutoff: the pool's wait ends at the deadline with `Busy`, and the waiter,
/// held at its gate until that result is in, takes it ready on its first poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_pool_exhaustion_on_a_plain_read_is_busy() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let s = shared(&srv);
    let readers = srv.hold_readers(4);
    let (reached, release) = s.faults.arm_cutoff_gate();
    let deadline = srv.clock.now() + Duration::from_millis(200);
    let w = s.clone();
    let waiter = tokio::spawn(async move { read_until(&w, deadline, |_| Ok(())).await });
    waits::bounded(Duration::from_secs(10), reached)
        .await
        .expect("the waiter reached its gate")
        .unwrap();
    drop(release);
    let r = waits::bounded(Duration::from_secs(20), waiter)
        .await
        .expect("the read ends")
        .unwrap();
    assert_eq!(r, Err(ApiError::busy()));
    assert!(srv.clock.now() >= deadline);
    drop(readers);
    srv.stop().await;
}

/// `until_deadline` reads the clock after the handler's future is dropped: a future ready
/// before the deadline whose destructor runs past it is 408.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn until_deadline_drop_past_the_deadline_is_408() {
    struct SkewOnDrop(std::sync::Arc<super::support::http::SkewClock>);
    impl std::future::Future for SkewOnDrop {
        type Output = u8;
        fn poll(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<u8> {
            std::task::Poll::Ready(7)
        }
    }
    impl Drop for SkewOnDrop {
        fn drop(&mut self) {
            self.0.skew(Duration::from_secs(3600));
        }
    }
    let srv = TestServer::start(Mounts::mvp()).await;
    let s = shared(&srv);
    let deadline = srv.clock.now() + Duration::from_secs(5);
    let clock = srv.clock.clone();
    let r = crate::http::until_deadline(&s, deadline, move || SkewOnDrop(clock)).await;
    assert_eq!(r, Err(ApiError::request_timeout()));
    srv.clock.skew(Duration::ZERO);
    srv.stop().await;
}

/// `WriteCtx::read_by` classifies a ready result by the clock sample taken when its wait
/// returned it: a result taken before `by` wins even when the waiter resumes past `by`.
#[tokio::test]
async fn read_by_classifies_with_one_sample() {
    let srv = TestServer::start(Mounts::mvp()).await;
    let s = shared(&srv);
    let ctx =
        crate::http::WriteCtx::new(None, srv.clock.now() + Duration::from_secs(10), s.clone());
    let (reached, release) = s.faults.arm_sample_gate();
    let by = srv.clock.now() + Duration::from_secs(5);
    let clock = srv.clock.clone();
    let skew = async move {
        waits::bounded(Duration::from_secs(10), reached)
            .await
            .expect("the waiter took its sample")
            .unwrap();
        clock.skew(Duration::from_secs(10));
        drop(release);
    };
    let (r, ()) = tokio::join!(ctx.read_by(by, |_| Ok(7)), skew);
    assert!(srv.clock.now() > by, "the waiter resumed past `by`");
    assert_eq!(r, Ok(7));
    srv.clock.skew(Duration::ZERO);
    drop(ctx);
    srv.stop().await;
}

/// `GET /api/threads/{thread_id}` (PR10a1-CL-38): owner and rescue read any thread, an agent
/// its own; an unrelated, missing, or unparseable thread is one 404; the guard rules are
/// those of `GET /api/events`; and the body has a fixed size, whatever the thread's attention
/// causes and history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_summary() {
    use chaldea_core::sync::ThreadSummary;

    use super::support::{launch, thread};

    let (big, other) = (thread(3), thread(4));
    // The store's own calls block; the preparation runs off the runtime's threads.
    let store = tokio::task::spawn_blocking(move || prepare_big_thread(big, other))
        .await
        .unwrap();
    let srv = TestServer::start_on(store, Mounts::mvp(), |_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let rescue = srv.token(TokenKind::Rescue, None, None).await;
    let (mine, l) = (thread(1), launch(1));
    let agent = srv.agent(mine, l).await;
    let hook = srv.hook(thread(2), launch(2)).await;
    let get = |t: &str| srv.req("GET", &format!("/api/threads/{t}"));
    let outcome = |r: &super::support::http::Reply| (r.status, r.code());

    for secret in [&owner, &rescue] {
        let r = srv.send(&get(&big.to_string()).bearer(secret)).await;
        assert_eq!(r.status, 200, "{r:?}");
        assert!(r.body.len() < 1024 * 1024, "{} bytes", r.body.len());
        let s: ThreadSummary = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(
            (s.thread_id, s.attention_count, s.launch),
            (big, 10_000, None)
        );
    }
    let r = srv.send(&get(&mine.to_string()).bearer(&agent)).await;
    assert_eq!(r.status, 200, "{r:?}");
    let s: ThreadSummary = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(
        (
            s.thread_id,
            s.attention_count,
            s.launch.map(|x| x.launch_id)
        ),
        (mine, 0, Some(l))
    );

    let not_found = (404, Some("not_found".to_owned()));
    for (what, target, secret) in [
        ("an unrelated thread", other.to_string(), &agent),
        ("a missing thread", thread(9).to_string(), &owner),
        ("an unparseable id", "not-a-thread".to_owned(), &owner),
        ("a non-canonical id", big.0.simple().to_string(), &owner),
    ] {
        let r = srv.send(&get(&target).bearer(secret)).await;
        assert_eq!(outcome(&r), not_found, "{what}");
    }
    let target = big.to_string();
    let r = srv.send(&get(&target)).await;
    assert_eq!(outcome(&r), (401, Some("unauthorized".to_owned())));
    let r = srv.send(&get(&target).bearer(&hook)).await;
    assert_eq!(r.status, 403, "{r:?}");
    let r = srv
        .send(
            &get(&target)
                .bearer(&owner)
                .host(Some(&format!("evil:{}", srv.port))),
        )
        .await;
    assert_eq!(outcome(&r), (421, Some("misdirected_request".to_owned())));
    let r = srv
        .send(&get(&target).bearer(&owner).header("Origin", "http://evil"))
        .await;
    assert_eq!(r.status, 403, "{r:?}");
    srv.stop().await;
}

/// A store with threads `big` (a long history and 10,000 unresolved attention causes) and
/// `other`.
fn prepare_big_thread(
    big: chaldea_core::ids::ThreadId,
    other: chaldea_core::ids::ThreadId,
) -> super::support::TestStore {
    use std::collections::BTreeSet;

    use chaldea_core::ids::{InputId, StreamId};
    use chaldea_core::payload::Content;
    use chaldea_core::state::AttentionCause;

    use super::support::{TestStore, raw_unknown, registered};

    let mut store = TestStore::new();
    store
        .append(vec![registered(big, false), registered(other, false)])
        .unwrap();
    for chunk in 0..10 {
        let history = (0..200)
            .map(|i| {
                raw_unknown(
                    StreamId::Thread(big),
                    &format!("h{chunk}-{i}"),
                    Content::text("x".repeat(200)),
                )
            })
            .collect();
        store.append(history).unwrap();
    }
    store.shutdown();
    // 10,000 unresolved causes, written into the projection directly.
    let causes: BTreeSet<AttentionCause> = (0..10_000u128)
        .map(|n| AttentionCause::InputUnconfirmed {
            input_id: InputId::from_u128(n),
        })
        .collect();
    {
        let c = rusqlite::Connection::open(store.path()).unwrap();
        let changed = c
            .execute(
                "UPDATE proj_threads SET attention = ?1 WHERE thread_id = ?2",
                [serde_json::to_string(&causes).unwrap(), big.to_string()],
            )
            .unwrap();
        assert_eq!(changed, 1);
    }
    store.reopen();
    store
}
