//! The intake order and its release points (PR9-CL-6).

use std::sync::Arc;
use std::time::{Duration, Instant};

use chaldea_core::ids::LaunchId;
use chaldea_core::payload::RawUnknownReason;

use super::support::hook::{Hook, HookServer, direct};
use super::support::http::{TestServer, connect, write};
use super::{WAIT, ok, reached, reached_sync};
use crate::hook::{IntakeOrder, NoIntakeFacts};
use crate::http::tests::shared;
use crate::http::{ApiError, Mounts, WriteCtx};
use crate::store::Routing;
use crate::store::tests::support::waits;

fn l() -> LaunchId {
    LaunchId::from_u128(1)
}

/// Whether `earlier_released` for `t` resolves within `within`.
async fn resolves(
    order: &Arc<IntakeOrder>,
    t: &crate::hook::IntakeTicket,
    within: Duration,
) -> bool {
    waits::bounded(within, order.earlier_released(t))
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

/// A test server on a stopped clock and a request context without a principal, for
/// `write_then`, whose deadline is on that clock and so never passes.
async fn server_and_ctx() -> (TestServer, WriteCtx) {
    use crate::http::MonotonicClock;

    let srv = TestServer::start(Mounts::mvp()).await;
    srv.clock.stop();
    let ctx = WriteCtx::new(None, srv.clock.now() + Duration::from_secs(5), shared(&srv));
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

/// A binding `SessionStart` waits for an earlier hook of its launch (PR9-CL-6): hook 1 held
/// in its walk, `SessionStart` 2 is seen waiting on the pending lower ordinal and `Stop` 3
/// is buffered meanwhile; released, the stream is `session_bound`, the binding's raw,
/// `UserPromptSubmit` 1, `Stop` 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binding_waits_for_earlier_hook() {
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    h.chains.set(7001, direct(7001, "codex"));
    let (walking, release) = h.chains.hold(7001);
    let first = Hook::of(&l, "UserPromptSubmit", "s").pid(Some(7001));
    let first_reply = h.spawn_post(&first);
    reached_sync(walking).await;
    let (waiting, at_wait) = tokio::sync::oneshot::channel();
    *h.ingest.order.between_check_and_await.lock().unwrap() =
        Some(Box::new(move || waiting.send(()).unwrap()));
    let start = Hook::of(&l, "SessionStart", "s");
    let start_reply = h.spawn_post(&start);
    reached(at_wait).await;
    let stop = Hook::of(&l, "Stop", "s");
    ok(&h.post(&stop).await);
    assert_eq!(
        h.launch_row(l.launch).session_id,
        None,
        "bound before hook 1"
    );
    release.send(()).unwrap();
    ok(&first_reply.await.unwrap());
    ok(&start_reply.await.unwrap());
    let order: Vec<uuid::Uuid> = h
        .adapter
        .calls()
        .into_iter()
        .filter(|c| c.promoted || c.event_name != "SessionStart")
        .map(|c| c.emit_id)
        .collect();
    assert_eq!(
        order,
        vec![first.emit, stop.emit],
        "promotion in receipt order"
    );
    let kinds: Vec<String> = h
        .hook_events(l.thread)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        kinds,
        [
            "session_bound",
            "raw_unknown",
            "user_message",
            "assistant_message"
        ],
        "session_bound, the binding's raw, UserPromptSubmit 1, Stop 3"
    );
    h.stop().await;
}

/// Of two eligible `SessionStart`s of different sessions, the one received first binds
/// (PR9-CL-6): with the first held before its job, the second is seen waiting on the
/// pending lower ordinal; released, the first binds and the second is `foreign_session`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn later_session_start_waits_before_the_first_submits() {
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    let first = Hook::of(&l, "SessionStart", "first");
    let second = Hook::of(&l, "SessionStart", "second");
    let (at_job, release_first) = h.ingest.points.arm_job(1);
    let first_reply = h.spawn_post(&first);
    reached(at_job).await;
    let (waiting, at_wait) = tokio::sync::oneshot::channel();
    *h.ingest.order.between_check_and_await.lock().unwrap() =
        Some(Box::new(move || waiting.send(()).unwrap()));
    let second_reply = h.spawn_post(&second);
    reached(at_wait).await;
    assert!(h.launch_row(l.launch).session_id.is_none());
    release_first.send(()).unwrap();
    ok(&first_reply.await.unwrap());
    ok(&second_reply.await.unwrap());
    assert_eq!(h.launch_row(l.launch).session_id.as_deref(), Some("first"));
    h.assert_raw(&l, second.emit, RawUnknownReason::ForeignSession);
    h.stop().await;
}

/// A queued earlier job releases its ordinal at submission: hook 1 passes `Store::submit`
/// behind a held writer and its client disconnects; the binding `SessionStart` 2 is then
/// submitted while hook 1 is still queued; released, hook 1 commits, then the binding.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_earlier_job_releases_at_submit() {
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    let release = h.srv.hold_writer().await;
    let s = shared(&h.srv);
    let order = h.ingest.order.clone();
    let before = s.faults.submits();
    let first = Hook::of(&l, "UserPromptSubmit", "s");
    let mut conn = connect(h.srv.port).await;
    write(&mut conn, &first.req(&h.srv).bytes()).await;
    super::support::http::eventually(WAIT, "hook 1 passed Store::submit", || {
        s.faults.submits() > before && !order.is_pending(l.launch, 1)
    })
    .await;
    drop(conn);
    let start = Hook::of(&l, "SessionStart", "s");
    let start_reply = h.spawn_post(&start);
    super::support::http::eventually(WAIT, "the binding passed Store::submit", || {
        s.faults.submits() > before + 1 && !order.is_pending(l.launch, 2)
    })
    .await;
    assert!(
        h.delivery(l.launch, first.emit).is_none(),
        "hook 1 ran before the writer was free"
    );
    release.send(()).unwrap();
    ok(&start_reply.await.unwrap());
    let d1 = h.delivery(l.launch, first.emit).expect("hook 1 committed");
    let d2 = h.delivery(l.launch, start.emit).unwrap();
    assert!(
        d1.commit_ordinal < d2.commit_ordinal,
        "hook 1 then the binding"
    );
    assert!(
        matches!(d1.routing, Routing::Appended { .. }),
        "promoted: {d1:?}"
    );
    h.stop().await;
}
