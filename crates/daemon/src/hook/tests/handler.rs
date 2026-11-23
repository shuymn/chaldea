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

/// The pid wait: a start committed before its subscription, one committed during it, one
/// committed after its bound and before the job, re-reads held by the reader pool across
/// the pid-wait deadline, and a pid-wait deadline equal to the request deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pid_wait_timings() {
    // Committed after the pre-read, before the subscription: the first re-read sees it.
    let h = HookServer::start().await;
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

    // Committed during the wait: the notification wakes it. The pid-wait bound is past the
    // request deadline, so only the notification can bring the 200.
    let h = HookServer::start_with(
        HookConfig {
            pid_wait: Duration::from_secs(10),
            ..HookConfig::default()
        },
        |_| {},
    )
    .await;
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
        |_| {},
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
        |_| {},
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

    // The pid wait starts at intake, with the ordinal and the facts: time spent before
    // intake (here 1.2 s of server clock, past a 1 s wait counted from before intake) does
    // not shorten it, so a start committed right after intake binds.
    let h = HookServer::start().await;
    let l = unstarted(&h).await;
    let (before_intake, go) = h.ingest.points.arm_before_intake();
    let (after_intake, go_on) = h.ingest.points.arm_intake(1);
    let x = Hook::of(&l, "SessionStart", "s");
    let reply = h.spawn_post(&x);
    reached(before_intake).await;
    h.srv.clock.skew(Duration::from_millis(1200));
    go.send(()).unwrap();
    reached(after_intake).await;
    start_now(&h, &l).await;
    go_on.send(()).unwrap();
    ok(&reply.await.unwrap());
    assert_eq!(h.launch_row(l.launch).session_id.as_deref(), Some("s"));
    h.stop().await;

    // The pid-wait deadline is the request deadline: 408, nothing written.
    let h = HookServer::start_with(
        HookConfig {
            pid_wait: Duration::from_secs(10),
            ..HookConfig::default()
        },
        |_| {},
    )
    .await;
    let l = unstarted(&h).await;
    let before = h.snapshot(&l);
    let x = Hook::of(&l, "SessionStart", "s");
    assert_eq!(
        outcome(&h.post(&x).await),
        (408, Some("request_timeout".into()))
    );
    assert_eq!(h.snapshot(&l), before);
    h.stop().await;
}

/// A server over a store whose fault points the test keeps.
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
    (
        HookServer::start_on(ts, HookConfig::default(), |_| {}).await,
        faults,
    )
}

/// Each store outcome of the job maps to its §2.11 J row, and stores nothing except a
/// commit whose outcome is unknown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_outcomes_map() {
    // J1: expired before the job held the lock.
    let h = HookServer::start_with(HookConfig::default(), |c| {
        c.write_reply_grace = Duration::from_secs(1);
    })
    .await;
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    let s = shared(&h.srv);
    let release = h.srv.hold_writer().await;
    let x = Hook::of(&l, "Stop", "s");
    let before = s.faults.submits();
    let reply = h.spawn_post(&x);
    eventually(WAIT, "the hook job is submitted", || {
        s.faults.submits() > before
    })
    .await;
    // Released just after the job's `not_after`, inside the 1 s reply grace: the writer
    // answers `Expired`.
    let not_after = *s.faults.submitted_not_afters().last().unwrap();
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        not_after + Duration::from_millis(100),
    ))
    .await;
    release.send(()).unwrap();
    assert_eq!(
        outcome(&reply.await.unwrap()),
        (408, Some("request_timeout".into()))
    );
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

    // J7 then J8: commit unknown (the row committed), then the failed store.
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

    // J7: the writer is gone.
    let (h, faults) = faulty().await;
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    let release = h.srv.hold_writer().await;
    faults
        .panic_outside_job
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let x = Hook::of(&l, "Stop", "s");
    let reply = h.spawn_post(&x);
    let s = shared(&h.srv);
    eventually(WAIT, "the hook job is queued", || s.faults.submits() >= 2).await;
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

/// The job held inside the writer past the reply grace: 503 `outcome_unknown`, no row yet;
/// released, the row commits; a retry with the same emit id replays it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grace_exceeded_then_commits() {
    let h = HookServer::start().await;
    let l = h.codex(1).await;
    bind(&h, &l, "s").await;
    let (reached_job, release) = h.adapter.hold();
    let x = Hook::of(&l, "Stop", "s");
    let reply = h.spawn_post(&x);
    reached_sync(reached_job).await;
    assert_eq!(
        outcome(&reply.await.unwrap()),
        (503, Some("outcome_unknown".into()))
    );
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
    let calls = h.adapter.calls().len();
    ok(&h.post(&x.clone().attempt(Some("2"))).await);
    assert_eq!(h.adapter.calls().len(), calls, "the retry was interpreted");
    h.stop().await;
}

/// A revocation committed after the pre-read refuses the job with 401; nothing changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_after_preread() {
    let h = HookServer::start().await;
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

/// G9, H5, H6: a deadline passed before the handler, every pre-read failure, and the walk,
/// pid wait, and binding order cut by the deadline; none writes anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handler_rejections() {
    let timeout = (408, Some("request_timeout".to_owned()));
    // G9: the server's clock moves past the deadline while the guard checks the token;
    // no handler step runs.
    let h = HookServer::start().await;
    let l = h.codex(1).await;
    let before = h.snapshot(&l);
    let clock = h.srv.clock.clone();
    shared(&h.srv)
        .faults
        .arm_after_read(0, move || clock.skew(Duration::from_secs(3)));
    assert_eq!(
        outcome(&h.post(&Hook::of(&l, "SessionStart", "s")).await),
        timeout
    );
    h.srv.clock.skew(Duration::ZERO);
    assert!(
        h.chains.walked.lock().unwrap().is_empty(),
        "the handler ran"
    );
    assert_eq!(h.snapshot(&l), before);
    h.stop().await;

    // H5: the token revoked between the guard and the pre-read.
    let h = HookServer::start().await;
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

    // H5: every reader held through the deadline: the pre-read is busy or cut off.
    let h = HookServer::start().await;
    let l = h.codex(1).await;
    let before = h.snapshot(&l);
    let (gate, open) = h.ingest.points.arm_pre_read();
    let reply = h.spawn_post(&Hook::of(&l, "SessionStart", "s"));
    reached(gate).await;
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
    let l = h.codex(1).await;
    let before = h.snapshot(&l);
    let (gate, open) = h.ingest.points.arm_intake(1);
    let reply = h.spawn_post(&Hook::of(&l, "SessionStart", "s"));
    reached(gate).await;
    h.srv.clock.skew(Duration::from_secs(3));
    open.send(()).unwrap();
    assert_eq!(outcome(&reply.await.unwrap()), timeout);
    h.srv.clock.skew(Duration::ZERO);
    // A walk spawned anyway would record itself on its blocking thread within this window.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        h.chains.walked.lock().unwrap().is_empty(),
        "walked after the deadline"
    );
    assert_eq!(h.snapshot(&l), before);
    h.stop().await;

    // H6: the walk held past the deadline.
    let h = HookServer::start().await;
    let l = h.codex(1).await;
    let before = h.snapshot(&l);
    let (_walking, release) = h.chains.hold(super::support::hook::EMITTER_PID);
    assert_eq!(
        outcome(&h.post(&Hook::of(&l, "SessionStart", "s")).await),
        timeout
    );
    drop(release);
    assert_eq!(h.snapshot(&l), before);
    h.stop().await;

    // H6: the binding order held past the deadline by a lower ordinal. The lower request
    // holds its ticket at the job point, which its own deadline does not end, so ordinal 1
    // stays pending through the `SessionStart`'s whole deadline.
    let h = HookServer::start().await;
    let l = h.codex(1).await;
    let (gate, open) = h.ingest.points.arm_job(1);
    let lower = h.spawn_post(&Hook::of(&l, "UserPromptSubmit", "s"));
    reached(gate).await;
    let start = Hook::of(&l, "SessionStart", "s");
    assert_eq!(outcome(&h.post(&start).await), timeout);
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
    let mut conn = connect(h.srv.port).await;
    write(&mut conn, &x.req(&h.srv).bytes()).await;
    eventually(WAIT, "the hook job is submitted", || {
        s.faults.submits() > before
    })
    .await;
    drop(conn);
    let permits = u32::try_from(body.len().div_ceil(1024)).unwrap() as usize;
    tokio::time::sleep(Duration::from_millis(200)).await;
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
