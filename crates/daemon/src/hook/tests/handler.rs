//! The handler's waits, rejections, and store outcomes, through the hook and HTTP sync
//! points (PR9-CL-15, CL-16, CL-17, CL-22, CL-27, CL-31).

use std::time::{Duration, Instant};

use chaldea_core::ids::Agent;
use chaldea_core::payload::RawUnknownReason as R;

use super::support::hook::{Hook, HookServer, Launch, start, status};
use super::support::http::{connect, eventually, write};
use super::support::{FixedClock, T0, TestStore, config};
use super::{WAIT, annex_of, bind, ok, reached, reached_sync};
use crate::hook::HookConfig;
use crate::http::tests::shared;
use crate::store::Routing;
use crate::store::test_faults::{CommitFault, Faults, open_with_shared};
use crate::store::tests::support::waits;

fn outcome(r: &super::support::http::Reply) -> (u16, Option<String>) {
    r.outcome()
}

/// A Codex launch reserved but not started (no pid).
async fn unstarted(h: &HookServer) -> Launch {
    h.launch_with(
        super::support::thread(1),
        super::support::launch(1),
        Agent::Codex,
        false,
    )
    .await
}

/// `launch_started` of `l` with its status event.
async fn start_now(h: &HookServer, l: &Launch) {
    let state = h.thread_row(l.thread).state;
    let mut events = vec![start(l.thread, l.launch)];
    events.extend(
        status(
            l.thread,
            &state,
            &chaldea_core::state::StateInput::Spawned,
            "late-spawn",
        )
        .1,
    );
    h.srv.append(events).await;
}

/// The request deadline of a server whose pid wait runs out in real time: far past the
/// 300 ms pid wait and every wait these tests make, so the pid-wait bound and not the
/// request deadline ends the wait on a slow host.
const LONG_REQUEST: Duration = Duration::from_secs(30);

/// The pid wait: a start committed before its subscription, one committed during it, one
/// committed after its bound and before the job, re-reads held by the reader pool across
/// the pid-wait deadline, and a pid-wait deadline equal to the request deadline. Each case
/// that needs no deadline to pass runs on a stopped clock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pid_wait_timings() {
    use crate::http::MonotonicClock;

    // Committed after the pre-read, before the subscription: the first re-read sees it.
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = unstarted(&h).await;
    let (gate, open) = h.ingest.points.arm_intake(1);
    let x = Hook::of(&l, "SessionStart", "s");
    let reply = h.spawn_post(&x);
    reached(gate).await;
    start_now(&h, &l).await;
    open.send(()).unwrap();
    ok(&reply.await.unwrap());
    assert_eq!(h.launch_row(l.launch).session_id.as_deref(), Some("s"));
    h.stop().await;

    // Committed during the wait: the notification wakes it. On the stopped clock neither
    // the pid-wait bound nor the request deadline passes, so only the notification can
    // bring the 200.
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = unstarted(&h).await;
    let (gate, open) = h.ingest.points.arm_subscribe();
    let x = Hook::of(&l, "SessionStart", "s");
    let reply = h.spawn_post(&x);
    reached(gate).await;
    let (reread, reread_rx) = tokio::sync::oneshot::channel();
    shared(&h.srv).faults.arm_after_read(0, move || {
        let _ = reread.send(());
    });
    open.send(()).unwrap();
    // The first re-read found nothing; the start then commits.
    reached(reread_rx).await;
    start_now(&h, &l).await;
    let r = reply.await.unwrap();
    ok(&r);
    assert_eq!(h.launch_row(l.launch).session_id.as_deref(), Some("s"));
    h.stop().await;

    // Committed after the bound, before the job: decided once, `unbound`.
    let h = HookServer::start_with(
        HookConfig {
            pid_wait: Duration::from_millis(300),
            ..HookConfig::default()
        },
        |c| c.short_deadline = LONG_REQUEST,
    )
    .await;
    let l = unstarted(&h).await;
    let (gate, open) = h.ingest.points.arm_job(1);
    let x = Hook::of(&l, "SessionStart", "s");
    let reply = h.spawn_post(&x);
    reached(gate).await;
    start_now(&h, &l).await;
    open.send(()).unwrap();
    ok(&reply.await.unwrap());
    h.assert_raw(&l, x.emit, R::Unbound);
    assert_eq!(h.launch_row(l.launch).session_id, None);
    h.stop().await;

    // Readers held, released before the pid-wait deadline: the re-read sees the start.
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = unstarted(&h).await;
    let (gate, open) = h.ingest.points.arm_intake(1);
    let x = Hook::of(&l, "SessionStart", "s");
    let reply = h.spawn_post(&x);
    reached(gate).await;
    start_now(&h, &l).await;
    let readers = h.srv.hold_readers(4);
    let (waiting, waiting_rx) = tokio::sync::oneshot::channel();
    shared(&h.srv).faults.arm_before_read(0, move || {
        let _ = waiting.send(());
    });
    open.send(()).unwrap();
    // The re-read is about to wait for a reader.
    reached(waiting_rx).await;
    drop(readers);
    ok(&reply.await.unwrap());
    assert_eq!(h.launch_row(l.launch).session_id.as_deref(), Some("s"));
    h.stop().await;

    // Readers held past the pid-wait deadline: a start it could not read is no start.
    let h = HookServer::start_with(
        HookConfig {
            pid_wait: Duration::from_millis(300),
            ..HookConfig::default()
        },
        |c| c.short_deadline = LONG_REQUEST,
    )
    .await;
    let l = unstarted(&h).await;
    let (gate, open) = h.ingest.points.arm_intake(1);
    let x = Hook::of(&l, "SessionStart", "s");
    let reply = h.spawn_post(&x);
    reached(gate).await;
    start_now(&h, &l).await;
    let readers = h.srv.hold_readers(4);
    let (job_gate, job_open) = h.ingest.points.arm_job(1);
    open.send(()).unwrap();
    reached(job_gate).await;
    drop(readers);
    job_open.send(()).unwrap();
    ok(&reply.await.unwrap());
    h.assert_raw(&l, x.emit, R::Unbound);
    h.stop().await;

    // The pid wait starts at intake, sampled under the intake lock with the ordinal and the
    // facts: time spent before the lock (here 1.2 s of server clock, past a 1 s wait counted
    // from any earlier sample) does not shorten it, so a start committed right after intake
    // binds. The clock moves only by that step.
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = unstarted(&h).await;
    let clock = h.srv.clock.clone();
    *h.ingest.order.before_lock.lock().unwrap() =
        Some(Box::new(move || clock.advance(Duration::from_millis(1200))));
    let (after_intake, go_on) = h.ingest.points.arm_intake(1);
    let x = Hook::of(&l, "SessionStart", "s");
    let reply = h.spawn_post(&x);
    reached(after_intake).await;
    start_now(&h, &l).await;
    go_on.send(()).unwrap();
    ok(&reply.await.unwrap());
    assert_eq!(h.launch_row(l.launch).session_id.as_deref(), Some("s"));
    h.stop().await;

    // The pid-wait deadline is the request deadline: 408, nothing written, and the job is
    // never reached. The clock stands still until the wait has subscribed and then moves
    // exactly to the request deadline: a pid-wait deadline past it would keep waiting, and
    // one before it would decide `unbound` and go on to the job.
    let h = HookServer::start_with(
        HookConfig {
            pid_wait: Duration::from_secs(10),
            ..HookConfig::default()
        },
        |_| {},
    )
    .await;
    h.srv.clock.stop();
    let l = unstarted(&h).await;
    let before = h.snapshot(&l);
    let request_deadline = h.srv.clock.now() + super::support::http::DEADLINE;
    let (subscribed, go_on) = h.ingest.points.arm_subscribe();
    let (mut job, _job_open) = h.ingest.points.arm_job(1);
    let reply = h.spawn_post(&Hook::of(&l, "SessionStart", "s"));
    reached(subscribed).await;
    assert_eq!(
        shared(&h.srv).faults.pid_wait_deadlines(),
        [request_deadline],
        "the pid-wait deadline is not the request deadline"
    );
    h.srv.clock.advance(super::support::http::DEADLINE);
    go_on.send(()).unwrap();
    let r = waits::bounded(WAIT, reply)
        .await
        .expect("the pid wait ended at the request deadline")
        .unwrap();
    assert_eq!(outcome(&r), (408, Some("request_timeout".into())));
    assert!(job.try_recv().is_err(), "the job point was reached");
    assert_eq!(h.snapshot(&l), before);
    h.stop().await;
}

/// A server on a stopped clock over a store whose fault points the test keeps.
async fn faulty() -> (HookServer, std::sync::Arc<Faults>) {
    let faults = std::sync::Arc::new(Faults::default());
    let dir = tempfile::tempdir().unwrap();
    let clock = FixedClock::new(T0);
    let store = open_with_shared(
        config(dir.path().join("chaldea.db"), clock.clone()),
        faults.clone(),
    )
    .unwrap();
    let ts = TestStore {
        store: Some(store),
        clock,
        dir,
    };
    let h = HookServer::start_on(ts, HookConfig::default(), |_| {}).await;
    h.srv.clock.stop();
    (h, faults)
}

/// Each store outcome of the job maps to its §2.11 J row, and stores nothing except a
/// commit whose outcome is unknown. Every server runs on a stopped clock, so no request
/// deadline passes unless a case moves a clock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_outcomes_map() {
    use crate::http::MonotonicClock;

    // J1: expired before the job held the lock. The server's clock stands still, so no
    // reply cutoff fires; the store's clock is put past the queued job's `not_after`
    // before the writer takes it.
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    let s = shared(&h.srv);
    let release = h.srv.hold_writer().await;
    let x = Hook::of(&l, "Stop", "s");
    let before = s.faults.submits();
    let reply = h.spawn_post(&x);
    eventually(WAIT, "the hook job is submitted", || {
        s.faults.submits() > before && !h.ingest.order.is_pending(l.launch, 2)
    })
    .await;
    // The job's `not_after` as the writer has it: the real instant of the request deadline.
    let not_after = h
        .srv
        .clock
        .real(*s.faults.submitted_not_afters().last().unwrap());
    h.srv
        .store
        .clock
        .lead(not_after.saturating_duration_since(Instant::now()) + Duration::from_millis(1));
    release.send(()).unwrap();
    assert_eq!(
        outcome(&reply.await.unwrap()),
        (408, Some("request_timeout".into()))
    );
    h.srv.store.clock.lead(Duration::ZERO);
    assert!(h.delivery(l.launch, x.emit).is_none());
    h.stop().await;

    // J2: `RolledBack(Busy)` at COMMIT.
    let (h, faults) = faulty().await;
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    *faults.commit.lock().unwrap() = Some(CommitFault::BusyBeforeCommit);
    let x = Hook::of(&l, "Stop", "s");
    assert_eq!(outcome(&h.post(&x).await), (503, Some("busy".into())));
    assert!(h.delivery(l.launch, x.emit).is_none());
    h.stop().await;

    // J2: the queue is full.
    let dir = tempfile::tempdir().unwrap();
    let clock = FixedClock::new(T0);
    let mut cfg = config(dir.path().join("chaldea.db"), clock.clone());
    cfg.writer_queue = 1;
    let ts = TestStore {
        store: Some(crate::store::Store::open(cfg).unwrap()),
        clock,
        dir,
    };
    let h = HookServer::start_on(ts, HookConfig::default(), |_| {}).await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    let release = h.srv.hold_writer().await;
    let filler = h
        .srv
        .store
        .store()
        .submit(super::support::soon(), |_tx| Ok(()))
        .unwrap();
    let x = Hook::of(&l, "Stop", "s");
    assert_eq!(outcome(&h.post(&x).await), (503, Some("busy".into())));
    release.send(()).unwrap();
    super::support::http::settled(filler).await.unwrap();
    assert!(h.delivery(l.launch, x.emit).is_none());
    h.stop().await;

    // J5: `RolledBack(DiskFull)`: the database may not grow, and a raw body needs pages.
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    h.run(|tx| {
        let pages: i64 = tx
            .raw()
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .map_err(crate::store::StoreError::from)?;
        tx.raw()
            .execute_batch(&format!("PRAGMA max_page_count = {pages}"))
            .map_err(crate::store::StoreError::from)?;
        Ok(())
    })
    .await;
    let x = Hook::new(&l, &vec![b'x'; 256 * 1024]);
    assert_eq!(outcome(&h.post(&x).await), (503, Some("disk_full".into())));
    assert!(h.delivery(l.launch, x.emit).is_none());
    h.stop().await;

    // J2: `WriteError::Busy`: another connection holds the write lock past the writer's busy
    // timeout.
    let dir = tempfile::tempdir().unwrap();
    let clock = FixedClock::new(T0);
    let mut cfg = config(dir.path().join("chaldea.db"), clock.clone());
    cfg.busy_timeout = Duration::from_millis(100);
    let ts = TestStore {
        store: Some(crate::store::Store::open(cfg).unwrap()),
        clock,
        dir,
    };
    let h = HookServer::start_on(ts, HookConfig::default(), |_| {}).await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    let other = rusqlite::Connection::open(h.srv.store.path()).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let x = Hook::of(&l, "Stop", "s");
    assert_eq!(outcome(&h.post(&x).await), (503, Some("busy".into())));
    other.execute_batch("ROLLBACK").unwrap();
    drop(other);
    assert!(h.delivery(l.launch, x.emit).is_none());
    h.stop().await;

    // J3: the registry is closing.
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    let (gate, open) = h.ingest.points.arm_job(2);
    let x = Hook::of(&l, "Stop", "s");
    let reply = h.spawn_post(&x);
    reached(gate).await;
    shared(&h.srv).registry.close();
    open.send(()).unwrap();
    assert_eq!(
        outcome(&reply.await.unwrap()),
        (503, Some("shutting_down".into()))
    );
    assert!(h.delivery(l.launch, x.emit).is_none());
    let HookServer { srv, .. } = h;
    let (_, mut store) = srv.into_store(WAIT).await;
    store.shutdown();

    // J7 then J8: commit unknown (the row committed), then the failed store. The server's
    // clock stands still, so no reply cutoff fires: each 503 is the observer's, sent after
    // it reported the outcome to the sink.
    let (h, faults) = faulty().await;
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    *faults.commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    let x = Hook::of(&l, "Stop", "s");
    assert_eq!(
        outcome(&h.post(&x).await),
        (503, Some("outcome_unknown".into()))
    );
    let y = Hook::of(&l, "Stop", "s");
    assert_eq!(
        outcome(&h.post(&y).await),
        (503, Some("store_failed".into()))
    );
    assert_eq!(
        *h.srv.sink.0.lock().unwrap(),
        [
            crate::http::StoreFatal::CommitUnknown,
            crate::http::StoreFatal::Failed
        ]
    );
    let HookServer { srv, .. } = h;
    let (_, mut store) = srv.into_store(WAIT).await;
    store.shutdown();

    // J7: the writer is gone. The clock stands still as above: a reply cutoff answers the
    // same 503 before the sink is told (§2.11 J7), which would leave nothing to check here.
    let (h, faults) = faulty().await;
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    let release = h.srv.hold_writer().await;
    faults
        .panic_outside_job
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let x = Hook::of(&l, "Stop", "s");
    // The writer is let go only once the hook job is in its queue (the observer waits on its
    // ticket); released earlier, it could end before the submission, which `Closed` then
    // answers with 503 `shutting_down`.
    let parked = shared(&h.srv).faults.arm_ticket_park();
    let reply = h.spawn_post(&x);
    reached(parked).await;
    release.send(()).unwrap();
    assert_eq!(
        outcome(&reply.await.unwrap()),
        (503, Some("outcome_unknown".into()))
    );
    assert!(
        h.srv
            .sink
            .0
            .lock()
            .unwrap()
            .contains(&crate::http::StoreFatal::WriterGone)
    );
    let HookServer { srv, .. } = h;
    let (_, mut store) = srv.into_store(WAIT).await;
    store.store = None;
}

/// The handler's context for a request of `l`'s hook token, as the guard builds it: the
/// principal and the test deadline from the server's clock.
fn write_ctx(h: &HookServer, l: &Launch) -> crate::http::WriteCtx {
    use crate::http::{MonotonicClock, Principal, WriteCtx};

    let token = h.srv.token_row(&l.token).unwrap();
    WriteCtx::new(
        Some(Principal {
            token_id: token.token_id,
            token_sha256: token.sha256,
            kind: token.kind,
            thread: token.thread_id,
        }),
        h.srv.clock.now() + super::support::http::DEADLINE,
        shared(&h.srv),
    )
}

/// `hook`'s request as the guard hands it to the handler.
fn request(hook: &Hook) -> axum::http::Request<axum::body::Body> {
    let mut r = axum::http::Request::post("/hook")
        .header("content-length", hook.body.len().to_string())
        .header("x-chaldea-emit-id", hook.emit.to_string());
    if let Some(a) = &hook.attempt {
        r = r.header("x-chaldea-attempt", a);
    }
    if let Some(p) = hook.pid {
        r = r.header("x-chaldea-emitter-pid", p.to_string());
    }
    if let Some(t) = &hook.thread {
        r = r.header("x-chaldea-thread-id", t);
    }
    if let Some(a) = &hook.agent {
        r = r.header("x-chaldea-agent", a);
    }
    r.body(axum::body::Body::from(hook.body.clone())).unwrap()
}

/// A commit that fails never answers 200, appended or buffered: `BusyBeforeCommit` rolls
/// the job back, the reply is 503 `busy`, and nothing is stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_commit_never_replies_ok() {
    for (event, bound) in [("Stop", true), ("UserPromptSubmit", false)] {
        let (h, faults) = faulty().await;
        let l = h.codex(1).await;
        if bound {
            bind(&h, &l, "s").await;
        }
        let hook = Hook::of(&l, event, "s");
        *faults.commit.lock().unwrap() = Some(CommitFault::BusyBeforeCommit);
        assert_eq!(
            outcome(&h.post(&hook).await),
            (503, Some("busy".into())),
            "{event}"
        );
        assert!(h.delivery(l.launch, hook.emit).is_none(), "{event}");
        h.stop().await;
    }
}

/// A handler dropped after its write observer registered no longer holds its ticket: the
/// observer, held at the submit gate, still owns ordinal 1 and the job; a binding
/// `SessionStart` 2 is seen waiting on it; released, the observer submits the job, the
/// ordinal is released at submit, and the binding commits after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_handler_after_registration_preserves_the_job() {
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    let first = Hook::of(&l, "UserPromptSubmit", "s");
    let (registered, open) = shared(&h.srv).faults.arm_submit_gate();
    let mut handler = Box::pin(h.ingest.clone().handle(write_ctx(&h, &l), request(&first)));
    tokio::select! {
        biased;
        _ = &mut handler => panic!("the handler answered before its job was submitted"),
        () = reached(registered) => {}
    }
    drop(handler);
    assert!(
        h.ingest.order.is_pending(l.launch, 1),
        "released with the handler"
    );
    assert!(h.delivery(l.launch, first.emit).is_none());
    let (waiting, at_wait) = tokio::sync::oneshot::channel();
    *h.ingest.order.between_check_and_await.lock().unwrap() =
        Some(Box::new(move || waiting.send(()).unwrap()));
    let start = Hook::of(&l, "SessionStart", "s");
    let start_reply = h.spawn_post(&start);
    reached(at_wait).await;
    open.send(()).unwrap();
    ok(&start_reply.await.unwrap());
    let d1 = h
        .delivery(l.launch, first.emit)
        .expect("the observer committed hook 1");
    let d2 = h.delivery(l.launch, start.emit).unwrap();
    assert!(
        d1.commit_ordinal < d2.commit_ordinal,
        "hook 1 then the binding"
    );
    assert!(matches!(d1.routing, Routing::Appended { .. }), "{d1:?}");
    h.stop().await;
}

/// The reply waits for the job's commit, appended (PR9-CL-1) and buffered (PR9-CL-4): the
/// handler, polled directly with no test gate, is still pending once its write observer
/// waits for the job's answer behind a held writer (the HTTP faults' ticket park), and
/// nothing is stored; released, 200 and the row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submitted_hook_cannot_reply_before_commit() {
    use std::future::Future;
    use std::task::{Context, Waker};

    for (event, ordinal) in [("Stop", 2), ("UserPromptSubmit", 1)] {
        let h = HookServer::start().await;
        h.srv.clock.stop();
        let l = h.codex(1).await;
        if ordinal == 2 {
            bind(&h, &l, "s").await;
        }
        let hook = Hook::of(&l, event, "s");
        let s = shared(&h.srv);
        let ctx = write_ctx(&h, &l);
        let release = h.srv.hold_writer().await;
        let parked = s.faults.arm_ticket_park();
        let response = tokio::task::unconstrained(h.ingest.clone().handle(ctx, request(&hook)));
        tokio::pin!(response);
        // The write observer waits for its job's answer behind the held writer.
        tokio::select! {
            biased;
            _ = &mut response => panic!("{event}: replied before its job answered"),
            () = reached(parked) => {}
        }
        assert!(
            !h.ingest.order.is_pending(l.launch, ordinal),
            "{event}: not submitted"
        );
        let mut cx = Context::from_waker(Waker::noop());
        assert!(
            response.as_mut().poll(&mut cx).is_pending(),
            "{event}: replied after submit while the writer was held"
        );
        assert!(h.delivery(l.launch, hook.emit).is_none());
        release.send(()).unwrap();
        let reply = waits::bounded(WAIT, response.as_mut())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.status(), axum::http::StatusCode::OK, "{event}");
        let d = h.delivery(l.launch, hook.emit).unwrap();
        if ordinal == 1 {
            assert_eq!(d.routing, Routing::Buffered);
        } else {
            assert!(matches!(d.routing, Routing::Appended { .. }), "{d:?}");
        }
        h.stop().await;
    }
}

/// The handler's last deadline check is before its write observer registers (L-12): a
/// request deadline that passes after the registration but before `Store::submit` is
/// decided by the writer. The server's clock stands still; the observer is held at the
/// submit gate while the clock is put past the deadline, so the job's `not_after` is past
/// on submission, the writer answers J1 `Expired`, and the reply, inside the grace, is 408
/// with nothing stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiry_after_registration_before_submit_is_j1() {
    use crate::http::{ApiError, MonotonicClock};

    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    let hook = Hook::of(&l, "Stop", "s");
    let ctx = write_ctx(&h, &l);
    let deadline = ctx.deadline();
    let s = shared(&h.srv);
    let expired = s.faults.expired_writes();
    let (registered, release) = s.faults.arm_submit_gate();
    let response = h.ingest.clone().handle(ctx, request(&hook));
    tokio::pin!(response);
    tokio::select! {
        biased;
        _ = &mut response => panic!("the handler replied before submission"),
        () = reached(registered) => {}
    }
    assert!(h.ingest.order.is_pending(l.launch, 1), "not submitted yet");
    h.srv
        .clock
        .advance(deadline.duration_since(h.srv.clock.now()) + Duration::from_millis(1));
    release.send(()).unwrap();
    let reply = waits::bounded(WAIT, &mut response).await.unwrap();
    assert_eq!(reply.err(), Some(ApiError::request_timeout()));
    assert_eq!(s.faults.expired_writes(), expired + 1);
    assert!(!h.ingest.order.is_pending(l.launch, 1));
    assert!(h.delivery(l.launch, hook.emit).is_none());
    h.stop().await;
}

/// A hook whose job passed `Store::submit` and whose client then disconnected is still
/// decided by the writer: reached after its `not_after`, it expires and stores nothing
/// (§2.13 "Best-effort delivery"). The empty job after it is a FIFO barrier. The server's
/// clock stands still; the store's clock is put past the job's `not_after` instead. The
/// store accepted the job before the disconnect (its observer waits on the ticket), and
/// the ticket answers the observer `Expired`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submitted_hook_can_expire_after_disconnect() {
    use crate::http::MonotonicClock;

    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    let hook = Hook::of(&l, "Stop", "s");
    let release = h.srv.hold_writer().await;
    let s = shared(&h.srv);
    let parked = s.faults.arm_ticket_park();
    let mut conn = connect(h.srv.port).await;
    write(&mut conn, &hook.req(&h.srv).bytes()).await;
    reached(parked).await;
    assert!(!h.ingest.order.is_pending(l.launch, 1));
    let not_after = h
        .srv
        .clock
        .real(*s.faults.submitted_not_afters().last().unwrap());
    drop(conn);
    h.srv
        .store
        .clock
        .lead(not_after.saturating_duration_since(Instant::now()) + Duration::from_millis(1));
    release.send(()).unwrap();
    // The barrier's own `not_after` is past the store clock's lead.
    let barrier = h
        .srv
        .store
        .store()
        .submit(not_after + WAIT, |_tx| Ok(()))
        .unwrap();
    super::support::http::settled(barrier).await.unwrap();
    h.srv.store.clock.lead(Duration::ZERO);
    assert!(h.delivery(l.launch, hook.emit).is_none());
    eventually(WAIT, "the observer saw the job expire", || {
        s.faults.expired_writes() == 1
    })
    .await;
    h.stop().await;
}

/// The stopped clock `hook_ingest` runs on (`SkewClock::stop`): a hook whose job has passed
/// `Store::submit` behind a held writer is held there past the request deadline (2 s, the
/// test server's) and the reply grace (100 ms), and the job still answers 200.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_clock_keeps_a_queued_hook_past_reply_grace() {
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    let hook = Hook::of(&l, "Stop", "s");
    let release = h.srv.hold_writer().await;
    let s = shared(&h.srv);
    let before = s.faults.submits();
    let reply = h.spawn_post(&hook);
    // Ordinal 1's ticket is released when `Store::submit` returns.
    eventually(WAIT, "the hook passed Store::submit", || {
        s.faults.submits() > before && !h.ingest.order.is_pending(l.launch, 1)
    })
    .await;
    // Measured from the submission, which follows the request's first head byte.
    waits::pause(Duration::from_millis(2000 + 100 + 400)).await;
    assert!(
        !reply.is_finished(),
        "a cutoff answered while the job was queued"
    );
    release.send(()).unwrap();
    ok(&reply.await.unwrap());
    assert!(h.delivery(l.launch, hook.emit).is_some());
    h.stop().await;
}

/// The job held inside the writer past the reply grace: 503 `outcome_unknown`, no row yet;
/// released, the row commits; a retry with the same emit id replays it. The server's clock
/// stands still except for the held hook's reply wait: frozen at the job point, it lets
/// that hook's reply cutoff fire in real time, measured from the freeze. The store's clock
/// lags meanwhile, so the job is not expired in the queue however late the writer takes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grace_exceeded_then_commits() {
    use crate::http::MonotonicClock;

    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    let (reached_job, release) = h.adapter.hold();
    let (before_job, go_on) = h.ingest.points.arm_job(2);
    let x = Hook::of(&l, "Stop", "s");
    let reply = h.spawn_post(&x);
    reached(before_job).await;
    let timers = shared(&h.srv).faults.timers().len();
    h.srv.store.clock.lag(WAIT);
    h.srv.clock.freeze(h.srv.clock.now());
    go_on.send(()).unwrap();
    reached_sync(reached_job).await;
    h.srv.store.clock.lag(Duration::ZERO);
    assert_eq!(
        outcome(&reply.await.unwrap()),
        (503, Some("outcome_unknown".into()))
    );
    assert_reply_cutoff(&h, timers);
    assert!(
        h.delivery(l.launch, x.emit).is_none(),
        "a row before commit"
    );
    release.send(()).unwrap();
    let (lid, emit) = (l.launch, x.emit);
    let store = h.srv.store.store().clone();
    eventually(WAIT, "the held job commits", || {
        store
            .read(super::support::soon(), move |tx| {
                tx.hook_delivery(lid, emit)
            })
            .unwrap()
            .is_some()
    })
    .await;
    h.srv.clock.stop();
    let calls = h.adapter.calls().len();
    ok(&h.post(&x.clone().attempt(Some("2"))).await);
    assert_eq!(h.adapter.calls().len(), calls, "the retry was interpreted");
    h.stop().await;
}

/// The only timer the hook's write set after its job point (`from`, the timer count there)
/// is the reply cutoff: its `not_after` (the request deadline) plus the reply grace, as the
/// server's clock has them. Earlier requests' timers do not count.
fn assert_reply_cutoff(h: &HookServer, from: usize) {
    let s = shared(&h.srv);
    let deadline = *s.faults.submitted_not_afters().last().unwrap();
    let reply_by = deadline + s.cfg.write_reply_grace;
    assert_eq!(
        s.faults.timers()[from..],
        [reply_by],
        "the reply timer is not at deadline + write_reply_grace"
    );
}

/// A job's answer taken after the request deadline and before `deadline +
/// write_reply_grace` is the reply: 200 and the row. The server's clock stands still until
/// the job point and is frozen there; the grace (30 s) keeps the reply cutoff far past the
/// request deadline, and the store's clock lags so the queued job is not expired. With
/// the job held, the clock is set inside the grace, past the deadline, and the job is
/// released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reply_inside_the_grace_is_ok() {
    use crate::http::MonotonicClock;

    let h = HookServer::start_with(HookConfig::default(), |c| {
        c.write_reply_grace = LONG_REQUEST;
    })
    .await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    let (reached_job, release) = h.adapter.hold();
    let (before_job, go_on) = h.ingest.points.arm_job(2);
    let x = Hook::of(&l, "Stop", "s");
    let reply = h.spawn_post(&x);
    reached(before_job).await;
    let timers = shared(&h.srv).faults.timers().len();
    h.srv.store.clock.lag(WAIT);
    h.srv.clock.freeze(h.srv.clock.now());
    go_on.send(()).unwrap();
    reached_sync(reached_job).await;
    h.srv.store.clock.lag(Duration::ZERO);
    assert_reply_cutoff(&h, timers);
    // The request deadline (the job's `not_after`) has passed; the grace has not.
    let deadline = *shared(&h.srv).faults.submitted_not_afters().last().unwrap();
    h.srv.clock.freeze(deadline + Duration::from_secs(1));
    release.send(()).unwrap();
    ok(&reply.await.unwrap());
    assert!(h.delivery(l.launch, x.emit).is_some());
    h.stop().await;
}

/// A revocation committed after the pre-read refuses the job with 401; nothing changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_after_preread() {
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    let before = h.snapshot(&l);
    let (gate, open) = h.ingest.points.arm_job(1);
    let x = Hook::of(&l, "SessionStart", "s");
    let reply = h.spawn_post(&x);
    reached(gate).await;
    h.srv.revoke_quietly(&l.token).await;
    open.send(()).unwrap();
    assert_eq!(
        outcome(&reply.await.unwrap()),
        (401, Some("unauthorized".into()))
    );
    assert_eq!(h.snapshot(&l), before);
    h.stop().await;
}

/// G9, H5, H6: a deadline passed before the handler, the pre-read's refusals other than a
/// store error (500), and the walk and binding order cut by the deadline; none writes
/// anything. Each case runs on a stopped clock: a deadline that must pass is reached by
/// moving the clock (`advance`), or passes in real time from a freeze at the test gate
/// just before the timer that cuts the step.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handler_rejections() {
    use crate::http::MonotonicClock;

    let timeout = (408, Some("request_timeout".to_owned()));
    // G9: the server's clock moves past the deadline after the guard's token read took its
    // clock sample, so that read counts; the guard's own deadline check answers 408, and
    // the handler never reads the body.
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    let before = h.snapshot(&l);
    let s = shared(&h.srv);
    let polls = s.faults.polls();
    let (sampled, classify) = s.faults.arm_sample_gate();
    let reply = h.spawn_post(&Hook::of(&l, "SessionStart", "s"));
    reached(sampled).await;
    h.srv.clock.advance(Duration::from_secs(3));
    classify.send(()).unwrap();
    assert_eq!(outcome(&reply.await.unwrap()), timeout);
    assert_eq!(s.faults.polls(), polls, "the handler read the body");
    assert_eq!(h.ingest.points.walks(), 0, "the handler ran");
    assert_eq!(h.snapshot(&l), before);
    h.stop().await;

    // H5: the token revoked between the guard and the pre-read.
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    let before = h.snapshot(&l);
    let (gate, open) = h.ingest.points.arm_pre_read();
    let reply = h.spawn_post(&Hook::of(&l, "SessionStart", "s"));
    reached(gate).await;
    h.srv.revoke_quietly(&l.token).await;
    open.send(()).unwrap();
    assert_eq!(
        outcome(&reply.await.unwrap()),
        (401, Some("unauthorized".into()))
    );
    assert_eq!(h.snapshot(&l), before);
    h.stop().await;

    // H5: every reader held through the deadline: the pre-read is busy or cut off. The
    // clock stands still until the pre-read; frozen there, its deadline passes in real time.
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    let before = h.snapshot(&l);
    let (gate, open) = h.ingest.points.arm_pre_read();
    let reply = h.spawn_post(&Hook::of(&l, "SessionStart", "s"));
    reached(gate).await;
    h.srv.clock.freeze(h.srv.clock.now());
    let readers = h.srv.hold_readers(4);
    open.send(()).unwrap();
    let r = reply.await.unwrap();
    assert!(
        outcome(&r) == (503, Some("busy".into())) || outcome(&r) == timeout,
        "{r:?}"
    );
    drop(readers);
    assert_eq!(h.snapshot(&l), before);
    h.stop().await;

    // H5: the store closed.
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    let (gate, open) = h.ingest.points.arm_pre_read();
    let reply = h.spawn_post(&Hook::of(&l, "SessionStart", "s"));
    reached(gate).await;
    h.srv.store.store().shutdown(Instant::now() + WAIT);
    open.send(()).unwrap();
    assert_eq!(
        outcome(&reply.await.unwrap()),
        (503, Some("shutting_down".into()))
    );
    let HookServer { srv, .. } = h;
    let _ = srv.into_store(WAIT).await;

    // H6: the deadline passed before the walk: 408, and no walk starts.
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    let before = h.snapshot(&l);
    let (gate, open) = h.ingest.points.arm_intake(1);
    let reply = h.spawn_post(&Hook::of(&l, "SessionStart", "s"));
    reached(gate).await;
    h.srv.clock.advance(Duration::from_secs(3));
    open.send(()).unwrap();
    assert_eq!(outcome(&reply.await.unwrap()), timeout);
    assert_eq!(h.ingest.points.walks(), 0, "walked after the deadline");
    assert_eq!(h.snapshot(&l), before);
    h.stop().await;

    // H6: the walk held past the deadline. Frozen after intake, the clock lets the walk's
    // deadline pass in real time while the walk is held.
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    let before = h.snapshot(&l);
    let (walking, release) = h.chains.hold(super::support::hook::EMITTER_PID);
    let (gate, open) = h.ingest.points.arm_intake(1);
    let reply = h.spawn_post(&Hook::of(&l, "SessionStart", "s"));
    reached(gate).await;
    h.srv.clock.freeze(h.srv.clock.now());
    open.send(()).unwrap();
    reached_sync(walking).await;
    assert_eq!(outcome(&reply.await.unwrap()), timeout);
    drop(release);
    assert_eq!(h.snapshot(&l), before);
    h.stop().await;

    // H6: the binding order held past the deadline by a lower ordinal. The lower request
    // holds its ticket at the job point, which its own deadline does not end, so ordinal 1
    // stays pending through the `SessionStart`'s whole deadline. The `SessionStart` names
    // no emitter pid and its launch has started, so neither a walk nor a pid wait comes
    // between its intake and the binding order: the clock stands still until its intake
    // gate and, frozen there, the deadline passes in real time while the binding order
    // waits.
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    let (gate, open) = h.ingest.points.arm_job(1);
    let lower = h.spawn_post(&Hook::of(&l, "UserPromptSubmit", "s"));
    reached(gate).await;
    let (waiting, at_wait) = tokio::sync::oneshot::channel();
    *h.ingest.order.between_check_and_await.lock().unwrap() =
        Some(Box::new(move || waiting.send(()).unwrap()));
    let mut start = Hook::of(&l, "SessionStart", "s");
    start.pid = None;
    let (intake, go_on) = h.ingest.points.arm_intake(2);
    let reply = h.spawn_post(&start);
    reached(intake).await;
    h.srv.clock.freeze(h.srv.clock.now());
    go_on.send(()).unwrap();
    reached(at_wait).await;
    assert_eq!(outcome(&reply.await.unwrap()), timeout);
    assert!(h.delivery(l.launch, start.emit).is_none());
    open.send(()).unwrap();
    let _ = lower.await;
    h.stop().await;
}

/// The body permits are held from the read until the writer drops the job, also after the
/// client left.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn body_permit_lifetime() {
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    let total = 32 * 1024;
    assert_eq!(h.ingest.free_permits(), total);
    let release = h.srv.hold_writer().await;
    let body = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "Stop", "session_id": "s", "pad": "x".repeat(1024 * 1024 - 64)
    }))
    .unwrap();
    let x = Hook::new(&l, &body);
    let s = shared(&h.srv);
    let before = s.faults.submits();
    // The handler, driven here, is dropped once its job passed `Store::submit`: the client
    // is gone and the handler with it.
    let mut handler = Box::pin(h.ingest.clone().handle(write_ctx(&h, &l), request(&x)));
    tokio::select! {
        biased;
        _ = &mut handler => panic!("the handler answered while the writer was held"),
        () = eventually(WAIT, "the hook job is submitted", || {
            s.faults.submits() > before && !h.ingest.order.is_pending(l.launch, 2)
        }) => {}
    }
    drop(handler);
    let permits = u32::try_from(body.len().div_ceil(1024)).unwrap() as usize;
    assert_eq!(
        h.ingest.free_permits(),
        total - permits,
        "released before the job"
    );
    release.send(()).unwrap();
    let ingest = h.ingest.clone();
    eventually(WAIT, "the permits return", move || {
        ingest.free_permits() == total
    })
    .await;
    // The writer drops the closure after the job ran, before its commit.
    let (lid, emit) = (l.launch, x.emit);
    let store = h.srv.store.store().clone();
    eventually(WAIT, "the job commits", move || {
        store
            .read(super::support::soon(), move |tx| {
                tx.hook_delivery(lid, emit)
            })
            .unwrap()
            .is_some_and(|d| matches!(d.routing, Routing::Appended { .. }))
    })
    .await;
    h.stop().await;
}

/// A truncated body is `unparseable` with the annex marker; an untruncated one has none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncated_annex_is_stored() {
    let h = HookServer::start().await;
    h.srv.clock.stop();
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    let cut = Hook::of(&l, "Stop", "s").truncated();
    ok(&h.post(&cut).await);
    let r = h.assert_raw(&l, cut.emit, R::Unparseable);
    assert_eq!(r.content_type, "application/octet-stream");
    assert_eq!(
        annex_of(&h, &format!("hook:{}", cut.emit)).as_deref(),
        Some(r#"{"hook":{"truncated":true}}"#)
    );
    let plain = super::support::hook::Hook::new(&l, b"not json");
    ok(&h.post(&plain).await);
    h.assert_raw(&l, plain.emit, R::Unparseable);
    assert_eq!(annex_of(&h, &format!("hook:{}", plain.emit)), None);
    h.stop().await;
}
