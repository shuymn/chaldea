//! The intake order and its release points (PR9-CL-6).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chaldea_core::ids::LaunchId;
use chaldea_core::payload::RawUnknownReason;

use super::support::hook::{Hook, HookServer, direct};
use super::support::http::{TestServer, connect, write};
use super::{WAIT, ok, reached, reached_sync};
use crate::hook::{IntakeFacts, IntakeGuard, IntakeOrder, IntakeProbe};
use crate::http::tests::shared;
use crate::http::{ApiError, MonotonicClock, Mounts, Shared, WriteCtx};
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

/// Asserts the launch's intake lock is held while the facts are taken.
struct LockedProbe(Arc<IntakeOrder>);

impl IntakeProbe for LockedProbe {
    fn facts(
        &self,
        launch: LaunchId,
        _: u64,
        _: Instant,
        _: &crate::hook::IntakeGuard,
    ) -> Option<crate::hook::IntakeFacts> {
        assert!(
            self.0.locked(launch),
            "the facts were taken outside the intake lock"
        );
        None
    }
}

/// The facts and the intake instant are taken with the launch's intake lock held, which is
/// released before the intake returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_samples_under_the_lock() {
    let (srv, ctx) = server_and_ctx().await;
    let order = Arc::new(IntakeOrder::default());
    let probe: Arc<dyn IntakeProbe> = Arc::new(LockedProbe(order.clone()));
    let s = shared(&srv);
    let t = order
        .intake(
            &s,
            ctx.deadline(),
            l(),
            0,
            &probe,
            Duration::from_millis(500),
        )
        .await
        .unwrap();
    assert_eq!(t.ticket.ordinal(), 1);
    assert!(t.at <= s.cfg.clock.now());
    assert!(!order.locked(l()));
    drop(t);
    drop(ctx);
    srv.stop().await;
}

/// Ordinals continue from the stored maximum; a waiter resolves once every lower ordinal
/// is released, whatever the order of the releases; a release between the waiter's check
/// and its await wakes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_order() {
    let order = Arc::new(IntakeOrder::default());
    let t1 = order.ticket(l(), 7);
    let t2 = order.ticket(l(), 0);
    let t3 = order.ticket(l(), 0);
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
    let a = order.ticket(l(), 0);
    let b = order.ticket(l(), 0);
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
    let t = order.ticket(l(), 0);
    let release_writer = srv.hold_writer().await;
    let (gate, open) = shared(&srv).faults.arm_submit_gate();
    let ctx = Arc::new(ctx);
    let c = ctx.clone();
    let write =
        tokio::spawn(async move { c.write_then(|_tx, _a| Ok(()), Box::new(t), None).await });
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
    let t = order.ticket(l(), 0);
    let ordinal = t.ordinal();
    srv.store.store().shutdown(Instant::now() + WAIT);
    assert_eq!(
        ctx.write_then(|_tx, _a| Ok(()), Box::new(t), None).await,
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
    let t = order.ticket(l(), 0);
    let ordinal = t.ordinal();
    srv.shutdown_http(Instant::now() + WAIT).await;
    assert_eq!(
        ctx.write_then(|_tx, _a| Ok(()), Box::new(t), None).await,
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

/// A probe that arms every guard, counts its `facts` calls, blocks one call on its latch when
/// one is set, and records each `finished` as `(ordinal, marks, committed)`.
#[derive(Default)]
struct Counting {
    facts: AtomicUsize,
    finished: Mutex<Vec<(u64, usize, bool)>>,
    latch: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl IntakeProbe for Counting {
    fn facts(
        &self,
        _: LaunchId,
        ordinal: u64,
        _: Instant,
        arm: &IntakeGuard,
    ) -> Option<IntakeFacts> {
        self.facts.fetch_add(1, Ordering::SeqCst);
        arm.arm();
        let latch = self.latch.lock().unwrap().take();
        if let Some(f) = latch {
            f();
        }
        Some(IntakeFacts(serde_json::json!({ "ordinal": ordinal })))
    }

    fn finished(
        &self,
        _: LaunchId,
        ordinal: u64,
        marks: &[(u64, crate::hook::TurnMark)],
        at: Option<Instant>,
    ) {
        self.finished
            .lock()
            .unwrap()
            .push((ordinal, marks.len(), at.is_some()));
    }
}

/// A sync gate: the hook tells `entered` and waits (at most 30 s) for `go`.
fn gate() -> (
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
    Box<dyn FnOnce() + Send>,
) {
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    (
        entered,
        go,
        Box::new(move || {
            let _ = entered_tx.send(());
            let _ = wait.recv_timeout(Duration::from_secs(30));
        }),
    )
}

/// An intake order beside a test server on a stopped clock, a request deadline 5 s ahead on
/// it, and a counting probe.
struct Env {
    srv: TestServer,
    s: Arc<Shared>,
    deadline: Instant,
    order: Arc<IntakeOrder>,
    counting: Arc<Counting>,
    probe: Arc<dyn IntakeProbe>,
}

const INTAKE_WAIT: Duration = Duration::from_millis(500);

impl Env {
    async fn new() -> Env {
        let srv = TestServer::start(Mounts::mvp()).await;
        srv.clock.stop();
        let s = shared(&srv);
        let deadline = srv.clock.now() + Duration::from_secs(5);
        let counting = Arc::new(Counting::default());
        Env {
            srv,
            s,
            deadline,
            order: Arc::new(IntakeOrder::default()),
            probe: counting.clone(),
            counting,
        }
    }

    /// One intake of `launch` on its own task; its output is the ordinal taken.
    fn spawn(&self, launch: LaunchId) -> tokio::task::JoinHandle<Result<u64, ApiError>> {
        let (order, s, probe, deadline) = (
            self.order.clone(),
            self.s.clone(),
            self.probe.clone(),
            self.deadline,
        );
        tokio::spawn(async move {
            order
                .intake(&s, deadline, launch, 0, &probe, INTAKE_WAIT)
                .await
                .map(|t| t.ticket.ordinal())
        })
    }

    fn finished(&self) -> Vec<(u64, usize, bool)> {
        self.counting.finished.lock().unwrap().clone()
    }

    async fn stop(self) {
        drop(self.s);
        self.srv.stop().await;
    }
}

/// SF-7: an intake waiting in its probe holds up nothing but its own launch's later intakes:
/// on one runtime thread, with ticket 3 of L waiting, dropping ticket 1 lets ticket 2's
/// `earlier_released` complete, an intake of launch M completes, and a timer fires.
#[tokio::test]
async fn waiting_intake_blocks_nothing() {
    let env = Env::new().await;
    let m = LaunchId::from_u128(2);
    let t1 = env.order.ticket(l(), 0);
    let t2 = env.order.ticket(l(), 0);
    let (entered, go, hold) = gate();
    *env.counting.latch.lock().unwrap() = Some(hold);
    let waiting = env.spawn(l());
    reached_sync(entered).await;
    drop(t1);
    waits::bounded(WAIT, env.order.earlier_released(&t2))
        .await
        .expect("ticket 2's earlier_released completed");
    let other = waits::bounded(WAIT, env.spawn(m))
        .await
        .expect("the intake of M completed");
    assert_eq!(other.unwrap(), Ok(1));
    waits::bounded(WAIT, waits::pause(Duration::from_millis(10)))
        .await
        .expect("the timer fired");
    assert!(!waiting.is_finished(), "intake 3 is still waiting");
    go.send(()).unwrap();
    assert_eq!(waiting.await.unwrap(), Ok(3));
    drop(t2);
    env.stop().await;
}

/// SF-9: a request dropped while its intake waits for the blocking pool takes no ordinal: its
/// task, started after the drop, takes no ordinal and calls no probe, and releases the
/// intake lock, so the next intake gets ordinal 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_waiting_for_pool() {
    let env = Env::new().await;
    let (entered, go, hold) = gate();
    *env.order.task_started.lock().unwrap() = Some(hold);
    let req = env.spawn(l());
    reached_sync(entered).await;
    req.abort();
    assert!(req.await.unwrap_err().is_cancelled());
    go.send(()).unwrap();
    let order = env.order.clone();
    super::support::http::eventually(WAIT, "the intake lock released", move || !order.locked(l()))
        .await;
    assert_eq!(env.order.next(l()), Some(1), "no ordinal");
    assert_eq!(env.counting.facts.load(Ordering::SeqCst), 0, "no facts");
    assert_eq!(env.spawn(l()).await.unwrap(), Ok(1));
    env.stop().await;
}

/// SF-9: the request deadline passed at each of the three checks before the start point (on
/// entry, after the intake lock, at the start point) is 408 with no ordinal and no facts; the
/// first two start no blocking task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deadline_before_ordinal() {
    let env = Env::new().await;
    let tasks = |env: &Env| env.order.tasks.load(Ordering::SeqCst);
    // On entry.
    let (order, s, probe) = (env.order.clone(), env.s.clone(), env.probe.clone());
    let now = env.srv.clock.now();
    let r = order.intake(&s, now, l(), 0, &probe, INTAKE_WAIT).await;
    assert_eq!(r.err(), Some(ApiError::request_timeout()));
    assert_eq!((tasks(&env), env.order.next(l())), (0, None));
    // After the intake lock, before the dispatch.
    let clock = env.srv.clock.clone();
    *env.order.after_lock.lock().unwrap() =
        Some(Box::new(move || clock.advance(Duration::from_secs(6))));
    assert_eq!(
        env.spawn(l()).await.unwrap(),
        Err(ApiError::request_timeout())
    );
    assert_eq!(tasks(&env), 0, "no blocking task");
    assert_eq!(env.order.next(l()), Some(1));
    // At the start point.
    let env = {
        env.stop().await;
        Env::new().await
    };
    let clock = env.srv.clock.clone();
    *env.order.task_started.lock().unwrap() =
        Some(Box::new(move || clock.advance(Duration::from_secs(6))));
    assert_eq!(
        env.spawn(l()).await.unwrap(),
        Err(ApiError::request_timeout())
    );
    assert_eq!(tasks(&env), 1);
    assert_eq!(env.order.next(l()), Some(1), "no ordinal");
    assert_eq!(env.counting.facts.load(Ordering::SeqCst), 0);
    env.stop().await;
}

/// SF-9: the request deadline passing just before the start point's claim (the request
/// clock read after it): the claim takes nothing, so a `UserPromptSubmit` is 408 with the
/// launch's next ordinal, its fence word, and its `latest_intake` unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deadline_at_claim() {
    use crate::supervisor::send::tests::SRig;
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let (w, latest, next) = (
        sl.fence.load(Ordering::SeqCst),
        sl.latest_intake.load(Ordering::SeqCst),
        rig.ingest.order.next(live.l),
    );
    let clock = rig.clock.clone();
    *rig.ingest.order.before_claim.lock().unwrap() =
        Some(Box::new(move || clock.advance(Duration::from_secs(3600))));
    let r = rig
        .post(&rig.hook_of(
            &live,
            "UserPromptSubmit",
            &serde_json::json!({ "prompt": "p" }),
        ))
        .await;
    assert_eq!(r.status, 408, "{r:?}");
    assert!(
        rig.ingest.order.before_claim.lock().unwrap().is_none(),
        "the claim was reached"
    );
    assert_eq!(rig.ingest.order.next(live.l), next, "no ordinal");
    assert_eq!(sl.fence.load(Ordering::SeqCst), w, "the fence unchanged");
    assert_eq!(sl.latest_intake.load(Ordering::SeqCst), latest);
    rig.stop().await;
}

/// SF-9: a request dropped after its intake took the ordinal: the intake runs to its end and
/// finishes once with no marks, releasing the ordinal; the ordinal is a gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_after_ordinal() {
    let env = Env::new().await;
    let (entered, go, hold) = gate();
    *env.order.after_ordinal.lock().unwrap() = Some(hold);
    let req = env.spawn(l());
    reached_sync(entered).await;
    req.abort();
    assert!(req.await.unwrap_err().is_cancelled());
    go.send(()).unwrap();
    let c = env.counting.clone();
    super::support::http::eventually(WAIT, "finished", move || {
        !c.finished.lock().unwrap().is_empty()
    })
    .await;
    assert_eq!(env.finished(), [(1, 0, false)], "once, with no marks");
    assert!(!env.order.is_pending(l(), 1));
    assert_eq!(env.spawn(l()).await.unwrap(), Ok(2), "a gap at 1");
    env.stop().await;
}

/// SF-9: the request deadline passing while the probe waits: 408 once the probe returns, the
/// intake finished once with no marks, and its ordinal a gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deadline_during_wait() {
    let env = Env::new().await;
    let (entered, go, hold) = gate();
    *env.counting.latch.lock().unwrap() = Some(hold);
    let req = env.spawn(l());
    reached_sync(entered).await;
    env.srv.clock.advance(Duration::from_secs(6));
    go.send(()).unwrap();
    assert_eq!(req.await.unwrap(), Err(ApiError::request_timeout()));
    let c = env.counting.clone();
    super::support::http::eventually(WAIT, "finished", move || {
        !c.finished.lock().unwrap().is_empty()
    })
    .await;
    assert_eq!(env.finished(), [(1, 0, false)]);
    let env = Env {
        deadline: env.srv.clock.now() + Duration::from_secs(5),
        ..env
    };
    assert_eq!(env.spawn(l()).await.unwrap(), Ok(2), "a gap at 1");
    env.stop().await;
}

/// SF-12: an intake that unwinds after its fence advance (its observation panics) answers
/// 500 and finishes once with no marks: its ordinal is released, so a later binding
/// `SessionStart` is not held up, and the fence is quiet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panic_after_advance() {
    use crate::supervisor::send::tests::SRig;
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(chaldea_core::ids::Agent::Claude, None).await;
    rig.paste(&live, true).await;
    let sl = rig.sl(&live);
    *crate::supervisor::lock(&sl.hooks.observe) =
        Some(Box::new(|_, _| panic!("the observation panicked")));
    let ord = rig.next_ordinal(&live);
    let r = rig
        .post(&rig.hook_of(
            &live,
            "UserPromptSubmit",
            &serde_json::json!({ "prompt": "p" }),
        ))
        .await;
    assert_eq!(r.status, 500, "{r:?}");
    assert!(
        !rig.ingest.order.is_pending(live.l, ord),
        "the ordinal released"
    );
    assert!(
        crate::supervisor::send::fence_quiet(sl.fence.load(Ordering::SeqCst)),
        "finished: the fence is quiet"
    );
    rig.hook(&live, "SessionStart", &serde_json::json!({}))
        .await;
    assert!(rig.launch(live.l).session_id.is_some(), "the later binding");
    rig.stop().await;
}

/// SF-12: an intake that unwinds before its fence advance (in its `SendState` section)
/// answers 500, releases its ordinal, and leaves the fence alone: another intake's open count
/// stays open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panic_before_advance() {
    use crate::supervisor::send::tests::SRig;
    use crate::supervisor::send::tests::probe::{intake, probe_of, soon};
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let p = probe_of(&rig);
    let (_, other) = intake(&p, live.l, 0, soon());
    let w = sl.fence.load(Ordering::SeqCst);
    assert_eq!(w & crate::supervisor::send::OPEN_MASK, 1, "one intake open");
    crate::supervisor::send::tests::gate::set(&sl.hooks.after_written_read, || {
        panic!("the state section panicked")
    });
    let ord = rig.next_ordinal(&live);
    let r = rig
        .post(&rig.hook_of(
            &live,
            "UserPromptSubmit",
            &serde_json::json!({ "prompt": "p" }),
        ))
        .await;
    assert_eq!(r.status, 500, "{r:?}");
    assert!(
        !rig.ingest.order.is_pending(live.l, ord),
        "the ordinal released"
    );
    assert_eq!(sl.fence.load(Ordering::SeqCst), w, "the fence unchanged");
    drop(other);
    assert!(crate::supervisor::send::fence_quiet(
        sl.fence.load(Ordering::SeqCst)
    ));
    rig.stop().await;
}
