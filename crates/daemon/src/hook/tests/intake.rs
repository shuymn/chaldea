//! The intake order and its release points (PR9-CL-6).

use std::sync::Arc;
use std::time::{Duration, Instant};

use chaldea_core::ids::LaunchId;

use super::support::hook::{Hook, HookServer};
use super::support::http::{TestServer, connect, write};
use super::{WAIT, ok, reached};
use crate::hook::{IntakeOrder, NoIntakeFacts};
use crate::http::tests::shared;
use crate::http::{ApiError, Mounts, WriteCtx};
use crate::store::Routing;

fn l() -> LaunchId {
    LaunchId::from_u128(1)
}

/// Whether `earlier_released` for `t` resolves within `within`.
async fn resolves(
    order: &Arc<IntakeOrder>,
    t: &crate::hook::IntakeTicket,
    within: Duration,
) -> bool {
    tokio::time::timeout(within, order.earlier_released(t))
        .await
        .is_ok()
}

/// The intake instant is sampled with the intake lock held, in the critical section that
/// takes the ordinal and the facts.
#[test]
fn intake_samples_under_the_lock() {
    let order = Arc::new(IntakeOrder::default());
    let o = order.clone();
    let (_, _, at) = order.intake(l(), 0, &NoIntakeFacts, || {
        assert!(o.locked(), "the clock was sampled outside the intake lock");
        Instant::now()
    });
    assert!(at <= Instant::now());
    assert!(!order.locked());
}

/// Ordinals continue from the stored maximum; a waiter resolves once every lower ordinal
/// is released, whatever the order of the releases; a release between the waiter's check
/// and its await wakes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_order() {
    let order = Arc::new(IntakeOrder::default());
    let (t1, _, _) = order.intake(l(), 7, &NoIntakeFacts, Instant::now);
    let (t2, _, _) = order.intake(l(), 0, &NoIntakeFacts, Instant::now);
    let (t3, _, _) = order.intake(l(), 0, &NoIntakeFacts, Instant::now);
    assert_eq!((t1.ordinal(), t2.ordinal(), t3.ordinal()), (8, 9, 10));
    // The lowest never waits.
    assert!(resolves(&order, &t1, WAIT).await);
    assert!(!resolves(&order, &t3, Duration::from_millis(100)).await);
    // Release on drop (a request ended before submission): 2 first, then 1.
    drop(t2);
    assert!(!resolves(&order, &t3, Duration::from_millis(100)).await);
    drop(t1);
    assert!(resolves(&order, &t3, WAIT).await);

    // A release between the check and the await: the future already observes it.
    let order = Arc::new(IntakeOrder::default());
    let (a, _, _) = order.intake(l(), 0, &NoIntakeFacts, Instant::now);
    let (b, _, _) = order.intake(l(), 0, &NoIntakeFacts, Instant::now);
    let a = Arc::new(std::sync::Mutex::new(Some(a)));
    let a2 = a.clone();
    *order.between_check_and_await.lock().unwrap() = Some(Box::new(move || {
        drop(a2.lock().unwrap().take());
    }));
    assert!(
        resolves(&order, &b, WAIT).await,
        "a release between the check and the await was missed"
    );
    assert!(a.lock().unwrap().is_none(), "the hook did not run");
}

/// A test server and a request context without a principal, for `write_then`.
async fn server_and_ctx() -> (TestServer, WriteCtx) {
    let srv = TestServer::start(Mounts::mvp()).await;
    let ctx = WriteCtx::new(None, Instant::now() + Duration::from_secs(5), shared(&srv));
    (srv, ctx)
}

/// `write_then` releases at submit success (with the job still queued behind a held
/// writer), at submit failure, and at registry refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_points() {
    // Success: released when `submit` returns, not when the job runs.
    let (srv, ctx) = server_and_ctx().await;
    let order = Arc::new(IntakeOrder::default());
    let (t, _, _) = order.intake(l(), 0, &NoIntakeFacts, Instant::now);
    let release_writer = srv.hold_writer().await;
    let (gate, open) = shared(&srv).faults.arm_submit_gate();
    let ctx = Arc::new(ctx);
    let c = ctx.clone();
    let write = tokio::spawn(async move { c.write_then(|_tx, _a| Ok(()), Box::new(t)).await });
    reached(gate).await;
    assert!(order.is_pending(l(), 1), "released before submission");
    open.send(()).unwrap();
    let o = order.clone();
    super::support::http::eventually(WAIT, "released at submit", move || !o.is_pending(l(), 1))
        .await;
    assert!(
        !write.is_finished(),
        "the job ran before the writer was free"
    );
    release_writer.send(()).unwrap();
    assert_eq!(write.await.unwrap(), Ok(()));
    srv.stop().await;

    // Failure: the store refuses the submission.
    let (srv, ctx) = server_and_ctx().await;
    let (t, _, _) = order.intake(l(), 0, &NoIntakeFacts, Instant::now);
    let ordinal = t.ordinal();
    srv.store.store().shutdown(Instant::now() + WAIT);
    assert_eq!(
        ctx.write_then(|_tx, _a| Ok(()), Box::new(t)).await,
        Err(ApiError::shutting_down())
    );
    assert!(
        !order.is_pending(l(), ordinal),
        "kept after a refused submit"
    );
    drop(ctx);
    let _ = srv.into_store(WAIT).await;

    // Registry refusal: the observer is never registered.
    let (mut srv, ctx) = server_and_ctx().await;
    let (t, _, _) = order.intake(l(), 0, &NoIntakeFacts, Instant::now);
    let ordinal = t.ordinal();
    srv.shutdown_http(Instant::now() + WAIT).await;
    assert_eq!(
        ctx.write_then(|_tx, _a| Ok(()), Box::new(t)).await,
        Err(ApiError::shutting_down())
    );
    assert!(
        !order.is_pending(l(), ordinal),
        "kept after a refused registration"
    );
    srv.store.shutdown();
}

/// A client that disconnects after its observer registered no longer holds its ticket:
/// the observer submits the job and releases the ordinal then, and a waiting binding job
/// commits after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnect_after_registration() {
    let h = HookServer::start().await;
    let l = h.codex(1).await;
    let (gate, open) = shared(&h.srv).faults.arm_submit_gate();
    let first = Hook::of(&l, "UserPromptSubmit", "s");
    let mut conn = connect(h.srv.port).await;
    write(&mut conn, &first.req(&h.srv).bytes()).await;
    reached(gate).await;
    drop(conn);
    let start = Hook::of(&l, "SessionStart", "s");
    let start_reply = h.spawn_post(&start);
    // The binding waits on ordinal 1, held by the registered observer.
    let order = h.ingest.order.clone();
    super::support::http::eventually(WAIT, "ordinal 2 taken", || order.is_pending(l.launch, 2))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!start_reply.is_finished(), "the binding overtook ordinal 1");
    assert!(order.is_pending(l.launch, 1));
    open.send(()).unwrap();
    ok(&start_reply.await.unwrap());
    let d1 = h.delivery(l.launch, first.emit).expect("hook 1 committed");
    let d2 = h.delivery(l.launch, start.emit).unwrap();
    assert!(d1.commit_ordinal < d2.commit_ordinal);
    assert!(matches!(d1.routing, Routing::Appended { .. }), "{d1:?}");
    h.stop().await;
}
