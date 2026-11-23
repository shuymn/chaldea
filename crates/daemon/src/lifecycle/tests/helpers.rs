//! Helper-process tests (design §7): this test binary re-executed with `--exact`, running
//! `prepare_process` and `run` as `main` does and exiting through `std::process::exit`; the
//! parent observes the report lines, stderr, the exit code, and the lock, under a 30 s
//! timeout. A controller thread inside the helper drives each scenario.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::future::BoxFuture;

use super::super::faults::{Faults, Point};
use super::super::files::{LockProbe, probe};
use super::super::gate::{ConsoleEvent, ShutdownGate, SignalName, StopCause};
use super::super::{DaemonConfig, ShutdownParticipant, names, prepare_process, run_process_with};
use super::{
    Helper, RecordingReady, TestComponents, WAIT, detached, field, foreground, free_port,
    helper_dir, helper_exit, in_helper, report, request, state, token_of, wait_for,
};
use crate::http::{ApiError, Mount, Mounts, WriteCtx, spec};
use crate::store::test_faults::{CommitFault, Faults as StoreFaults};
use crate::store::{JobError, NewToken, Store, TokenKind};

const PREFIX: &str = "lifecycle::tests::helpers::";
const TOKEN: &str = "chd_TTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTT";
const STEP_ENV: &str = "CHALDEA_LIFECYCLE_STEP";

/// When the controller requested the stop.
static STOPPED_AT: OnceLock<Instant> = OnceLock::new();

/// The helper's view of its own run.
struct Ctl {
    faults: Arc<Faults>,
    dir: PathBuf,
}

impl Ctl {
    fn gate(&self) -> Arc<ShutdownGate> {
        assert!(wait_for(WAIT, || self.faults.gate.get().is_some()));
        self.faults.gate.get().unwrap().clone()
    }

    /// Waits for the `serving` record; the port.
    fn serving(&self) -> u16 {
        assert!(wait_for(WAIT, || std::fs::read_to_string(
            self.dir.join(names::RECORD)
        )
        .is_ok_and(|r| r.contains("\"serving\""))));
        *self.faults.port.get().unwrap()
    }

    fn store(&self) -> Store {
        assert!(wait_for(WAIT, || self.faults.store.get().is_some()));
        self.faults.store.get().unwrap().clone()
    }

    fn stop(&self) {
        let _ = STOPPED_AT.set(Instant::now());
        self.gate().request(StopCause::Signal(SignalName::Term));
    }
}

/// Runs the helper side: the controller on a thread, `run` on this one, then the exit.
fn helper(
    cfg: DaemonConfig,
    comps: TestComponents,
    faults: Arc<Faults>,
    pause: Duration,
    controller: impl FnOnce(&Ctl) + Send + 'static,
) -> ! {
    prepare_process();
    let ctl = Ctl {
        faults: faults.clone(),
        dir: helper_dir(),
    };
    let controller = std::thread::spawn(move || controller(&ctl));
    let r = run_process_with(cfg, Box::new(comps), faults);
    // The controller's own reports come before the exit.
    wait_for(Duration::from_secs(10), || controller.is_finished());
    if let Some(t) = STOPPED_AT.get() {
        report(&format!("since_stop_ms={}", t.elapsed().as_millis()));
    }
    helper_exit(&r, pause)
}

fn fg() -> DaemonConfig {
    foreground(&helper_dir(), Some(free_port()))
}

/// The parent: starts the helper on a fresh state directory.
fn parent(name: &str, env: &[(&str, &str)]) -> (Helper, super::Dir) {
    let dir = state();
    let h = Helper::spawn(&format!("{PREFIX}{name}"), &dir.path, env);
    (h, dir)
}

/// During the helper's pause after `run` returned, the lock is held; after its exit, free.
fn held_then_free(h: &mut Helper, dir: &Path) {
    assert_eq!(
        probe(dir).unwrap(),
        LockProbe::Held,
        "the lock was released in process"
    );
    h.wait(WAIT);
    assert!(
        wait_for(Duration::from_secs(10), || probe(dir).ok()
            == Some(LockProbe::Free)),
        "the lock was not released by the exit"
    );
}

fn post_events(mounts_handler: Mount) -> Mounts {
    Mounts::mvp().with(mounts_handler)
}

fn events_spec() -> &'static crate::http::RouteSpec {
    spec(&Method::POST, "/events").unwrap()
}

/// A job that parks forever inside its transaction, after telling `entered`.
fn park_forever(
    entered: mpsc::Sender<()>,
) -> impl FnOnce(&mut crate::store::WriteTx<'_>) -> Result<(), JobError> + Send + 'static {
    move |_tx| {
        let _ = entered.send(());
        loop {
            std::thread::park();
        }
    }
}

// ---- PR8-CL-3 ----

/// Dropping an `InstanceLock` never releases it; only the exit does.
#[test]
fn drop_does_not_release() {
    if in_helper() {
        let d = helper_dir();
        std::fs::create_dir_all(&d).unwrap();
        let l = super::super::InstanceLock::acquire(&d, Duration::ZERO).unwrap();
        drop(l);
        report("dropped");
        std::thread::sleep(Duration::from_secs(2));
        std::process::exit(0);
    }
    let (mut h, dir) = parent("drop_does_not_release", &[]);
    h.expect("dropped", WAIT);
    held_then_free(&mut h, &dir.path);
}

// ---- PR8-CL-4: the lock is kept unless every condition holds ----

/// A writer job held past the cutoff: `writer_exited: false`; `run` returns within 10 s + 1 s.
#[test]
fn blocked_writer_keeps_the_lock() {
    if in_helper() {
        helper(
            fg(),
            TestComponents::default(),
            Arc::default(),
            Duration::from_secs(2),
            |c| {
                c.serving();
                let (tx, rx) = mpsc::channel();
                let _ticket = c
                    .store()
                    .submit(Instant::now() + WAIT, park_forever(tx))
                    .unwrap();
                rx.recv_timeout(WAIT).unwrap();
                c.stop();
                std::mem::forget(_ticket);
            },
        );
    }
    let (mut h, dir) = parent("blocked_writer_keeps_the_lock", &[]);
    let since = h.expect("since_stop_ms", WAIT);
    let ms: u64 = field(&since, "since_stop_ms").parse().unwrap();
    assert!(ms <= 11_000, "{ms} ms");
    let r = h.expect("returned", WAIT);
    assert_eq!(field(&r, "lock"), "HeldUntilExit", "{r}");
    assert_eq!(field(&r, "reason"), "lock_held_at_exit", "{r}");
    held_then_free(&mut h, &dir.path);
    assert!(h.stderr().contains("writer_exited false"), "{}", h.stderr());
}

/// A `CommitUnknown` retires the writer, then a real reader is held past the cutoff: the
/// writer exited, a reader did not return, and the lock is held.
#[test]
fn live_reader_keeps_the_lock() {
    if in_helper() {
        let sf = Arc::new(StoreFaults::default());
        let faults = Arc::new(Faults::default());
        *faults.store_faults.lock().unwrap() = Some(sf.clone());
        helper(
            fg(),
            TestComponents::default(),
            faults,
            Duration::from_secs(2),
            move |c| {
                c.serving();
                let store = c.store();
                *sf.commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
                let r = store
                    .submit(Instant::now() + WAIT, |_tx| Ok(()))
                    .unwrap()
                    .blocking_wait();
                assert!(r.is_err());
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let _ = store.read(Instant::now() + WAIT, move |_r| -> Result<(), _> {
                        let _ = tx.send(());
                        loop {
                            std::thread::park();
                        }
                    });
                });
                rx.recv_timeout(WAIT).unwrap();
                c.stop();
            },
        );
    }
    let (mut h, dir) = parent("live_reader_keeps_the_lock", &[]);
    let r = h.expect("returned", WAIT);
    assert_eq!(field(&r, "lock"), "HeldUntilExit", "{r}");
    held_then_free(&mut h, &dir.path);
    assert!(
        h.stderr()
            .contains("readers_returned false, writer_exited true"),
        "{}",
        h.stderr()
    );
}

/// A write handler that never returns: HTTP requests its abort, so the lock is held.
#[test]
fn aborted_http_task_keeps_the_lock() {
    if in_helper() {
        async fn never(_ctx: WriteCtx, _req: Request<Body>) -> Result<Response, ApiError> {
            ENTERED.store(true, Ordering::SeqCst);
            std::future::pending().await
        }
        static ENTERED: AtomicBool = AtomicBool::new(false);
        let comps = TestComponents {
            mounts: Some(post_events(Mount::write(events_spec(), never))),
            ..TestComponents::default()
        };
        helper(fg(), comps, Arc::default(), Duration::from_secs(2), |c| {
            let port = c.serving();
            let token = token_of(&c.dir);
            std::thread::spawn(move || {
                let _ = request(port, "POST", "/events", Some(&token), Some("{}"));
            });
            assert!(wait_for(WAIT, || ENTERED.load(Ordering::SeqCst)));
            c.stop();
        });
    }
    let (mut h, dir) = parent("aborted_http_task_keeps_the_lock", &[]);
    let r = h.expect("returned", WAIT);
    assert_eq!(field(&r, "lock"), "HeldUntilExit", "{r}");
    held_then_free(&mut h, &dir.path);
    assert!(h.stderr().contains("1 abort requested"), "{}", h.stderr());
}

/// A detached daemon whose log thread does not acknowledge leaving `daemon.log` keeps the
/// lock.
#[test]
fn unacknowledged_log_switch_keeps_the_lock() {
    if in_helper() {
        let dir = helper_dir();
        let ready = RecordingReady::new(&dir);
        let cfg = detached(&dir, Some(free_port()), &ready);
        helper(
            cfg,
            TestComponents::default(),
            Arc::default(),
            Duration::from_secs(2),
            move |c| {
                c.serving();
                assert!(wait_for(WAIT, || ready
                    .lines()
                    .iter()
                    .any(|v| v.line.starts_with("ready "))));
                super::super::log::process_logger()
                    .unwrap()
                    .withhold_switch_ack();
                c.stop();
            },
        );
    }
    let (mut h, dir) = parent("unacknowledged_log_switch_keeps_the_lock", &[]);
    let r = h.expect("returned", WAIT);
    assert_eq!(field(&r, "lock"), "HeldUntilExit", "{r}");
    assert_eq!(field(&r, "reason"), "lock_held_at_exit", "{r}");
    held_then_free(&mut h, &dir.path);
    let log = std::fs::read_to_string(dir.path.join(names::LOG)).unwrap();
    assert!(
        log.contains("listening") || log.contains("store shutdown"),
        "{log}"
    );
}

// ---- PR8-CL-5: the store shutdown runs off the workers ----

#[test]
fn store_shutdown_runs_off_the_workers() {
    if in_helper() {
        let faults = Arc::new(Faults::default());
        let (ptx, prx) = tokio::sync::mpsc::unbounded_channel();
        *faults.probe.lock().unwrap() = Some(prx);
        helper(
            fg(),
            TestComponents::default(),
            faults,
            Duration::ZERO,
            move |c| {
                c.serving();
                let store = c.store();
                let (in_tx, in_rx) = mpsc::channel();
                let (rel_tx, rel_rx) = mpsc::channel::<()>();
                std::thread::spawn(move || {
                    let _ = store.read(Instant::now() + WAIT, move |_r| {
                        let _ = in_tx.send(());
                        let _ = rel_rx.recv_timeout(WAIT);
                        Ok(())
                    });
                });
                in_rx.recv_timeout(WAIT).unwrap();
                c.stop();
                let cutoff = c.gate().stopping().unwrap().cutoff;
                // The reader is held until 1 s before the cutoff.
                std::thread::spawn(move || {
                    std::thread::sleep(
                        (cutoff - Duration::from_secs(1)).saturating_duration_since(Instant::now()),
                    );
                    let _ = rel_tx.send(());
                });
                assert!(wait_for(WAIT, || c
                    .faults
                    .marks()
                    .iter()
                    .any(|m| m.0 == "store")));
                assert!(wait_for(WAIT, || c
                    .faults
                    .store_shutdown_thread
                    .get()
                    .is_some()));
                std::thread::sleep(Duration::from_millis(200));
                let (handle, workers) = c.faults.runtime.get().unwrap().clone();
                let barrier = Arc::new(Barrier::new(workers));
                let ids = Arc::new(Mutex::new(Vec::new()));
                let (done_tx, done_rx) = mpsc::channel();
                for _ in 0..workers {
                    let (b, ids, done) = (barrier.clone(), ids.clone(), done_tx.clone());
                    handle.spawn(async move {
                        ids.lock().unwrap().push(std::thread::current().id());
                        b.wait();
                        let _ = done.send(());
                    });
                }
                let crossed =
                    (0..workers).all(|_| done_rx.recv_timeout(Duration::from_secs(3)).is_ok());
                let store_thread = *c.faults.store_shutdown_thread.get().unwrap();
                let off_workers = !ids.lock().unwrap().contains(&store_thread);
                let (rtx, rrx) = std::sync::mpsc::channel();
                let t0 = Instant::now();
                ptx.send(rtx).unwrap();
                let probed = rrx.recv_timeout(Duration::from_millis(100)).is_ok();
                report(&format!(
                    "checks crossed={crossed} off_workers={off_workers} probe={probed} probe_ms={} workers={workers}",
                    t0.elapsed().as_millis()
                ));
            },
        );
    }
    let (mut h, _dir) = parent("store_shutdown_runs_off_the_workers", &[]);
    let checks = h.expect("checks", WAIT);
    assert_eq!(field(&checks, "crossed"), "true", "{checks}");
    assert_eq!(field(&checks, "off_workers"), "true", "{checks}");
    assert_eq!(field(&checks, "probe"), "true", "{checks}");
    let r = h.expect("returned", WAIT);
    assert_eq!(field(&r, "code"), "0", "{r}");
    assert_eq!(field(&r, "lock"), "Released", "{r}");
    h.wait(WAIT);
}

// ---- PR8-CL-10 ----

/// A console end with one job parked inside its transaction and one intent committed: exit 1
/// with the record left; the next start rolls the parked job back and reconciles the receipt.
#[test]
fn console_end_recovers_on_restart() {
    if in_helper() {
        helper(
            fg(),
            TestComponents::default(),
            Arc::default(),
            Duration::ZERO,
            |c| {
                c.serving();
                let store = c.store();
                store
                    .submit(Instant::now() + WAIT, |tx| {
                        let t = chaldea_core::ids::TokenId::from_u128(0x600d);
                        tx.token_insert(NewToken {
                            token_id: t,
                            kind: TokenKind::Owner,
                            sha256: [1; 32],
                            thread_id: None,
                            launch_id: None,
                        })?;
                        tx.receipt_begin(
                            chaldea_core::ids::CommandId::from_u128(0x8a),
                            t,
                            [7; 32],
                        )?;
                        Ok(())
                    })
                    .unwrap()
                    .blocking_wait()
                    .unwrap();
                let (tx, rx) = mpsc::channel();
                let ticket = store
                    .submit(Instant::now() + WAIT, move |w| {
                        w.token_insert(NewToken {
                            token_id: chaldea_core::ids::TokenId::from_u128(0xbad),
                            kind: TokenKind::Owner,
                            sha256: [2; 32],
                            thread_id: None,
                            launch_id: None,
                        })?;
                        park_forever(tx)(w)
                    })
                    .unwrap();
                rx.recv_timeout(WAIT).unwrap();
                std::mem::forget(ticket);
                c.gate().crash_exit(ConsoleEvent::Close);
            },
        );
    }
    let (mut h, dir) = parent("console_end_recovers_on_restart", &[]);
    let r = h.expect("returned", WAIT);
    assert_eq!(field(&r, "reason"), "console_end", "{r}");
    assert_eq!(h.wait(WAIT), Some(1));
    assert!(
        dir.path.join(names::RECORD).exists(),
        "the record was removed"
    );
    let run = super::start(
        foreground(&dir.path, Some(free_port())),
        TestComponents::default(),
        Arc::default(),
    );
    run.serving();
    run.stop();
    assert_eq!(run.join().code.code(), 0);
    let store = super::startup::open_store(&dir.path);
    let parked = store
        .read(Instant::now() + WAIT, |r| r.token_by_hash(&[2; 32]))
        .unwrap();
    super::startup::close(&store);
    assert!(parked.is_none(), "the parked job's rows survived");
    assert_eq!(
        super::startup::receipt(&dir.path),
        Some(chaldea_core::command::ReceiptStatus::UnknownAfterCrash)
    );
}

// ---- PR8-CL-19: panics ----

#[test]
fn payload_is_omitted() {
    if in_helper() {
        async fn boom(ctx: WriteCtx, req: Request<Body>) -> Result<Response, ApiError> {
            let body = ctx.body_within(req.into_body(), 4096).await?;
            panic!("{}", String::from_utf8_lossy(&body));
        }
        let comps = TestComponents {
            mounts: Some(post_events(Mount::write(events_spec(), boom))),
            ..TestComponents::default()
        };
        let marker = std::env::var("CHALDEA_MARKER").unwrap();
        helper(fg(), comps, Arc::default(), Duration::ZERO, move |c| {
            let port = c.serving();
            let _ = request(
                port,
                "POST",
                "/events",
                Some(&token_of(&c.dir)),
                Some(&format!("\"{marker} {TOKEN}\"")),
            );
        });
    }
    let marker = format!("BODY-MARKER-{}", uuid::Uuid::new_v4());
    let (mut h, dir) = parent("payload_is_omitted", &[("CHALDEA_MARKER", &marker)]);
    let r = h.expect("returned", WAIT);
    assert_eq!(field(&r, "reason"), "panic", "{r}");
    assert_eq!(h.wait(WAIT), Some(1));
    let err = h.stderr();
    assert!(err.contains("panic in connection at "), "{err}");
    assert!(err.contains("helpers.rs:"), "{err}");
    assert!(err.contains("(payload omitted)"), "{err}");
    assert!(!err.contains(&marker) && !err.contains(TOKEN), "{err}");
    assert_eq!(probe(&dir.path).unwrap(), LockProbe::Free);
}

#[test]
fn thread_name_is_omitted() {
    if in_helper() {
        async fn spawn_panicking(
            _ctx: WriteCtx,
            _req: Request<Body>,
        ) -> Result<Response, ApiError> {
            let t = std::thread::Builder::new()
                .name(format!("THREAD-MARKER-{TOKEN}"))
                .spawn(|| panic!("inside"))
                .unwrap();
            let _ = t.join();
            Ok(StatusCode::OK.into_response())
        }
        let comps = TestComponents {
            mounts: Some(post_events(Mount::write(events_spec(), spawn_panicking))),
            ..TestComponents::default()
        };
        helper(fg(), comps, Arc::default(), Duration::ZERO, |c| {
            let port = c.serving();
            let _ = request(port, "POST", "/events", Some(&token_of(&c.dir)), Some("{}"));
        });
    }
    let (mut h, _dir) = parent("thread_name_is_omitted", &[]);
    let r = h.expect("returned", WAIT);
    assert_eq!(field(&r, "reason"), "panic", "{r}");
    h.wait(WAIT);
    let err = h.stderr();
    assert!(err.contains("panic in thread at "), "{err}");
    assert!(
        !err.contains("THREAD-MARKER") && !err.contains(TOKEN),
        "{err}"
    );
}

#[test]
fn startup_panic_keeps_the_lock() {
    if in_helper() {
        let comps = TestComponents {
            panic_in_build: true,
            ..TestComponents::default()
        };
        helper(fg(), comps, Arc::default(), Duration::from_secs(2), |_| {});
    }
    let (mut h, dir) = parent("startup_panic_keeps_the_lock", &[]);
    let r = h.expect("returned", WAIT);
    assert_eq!(field(&r, "code"), "1", "{r}");
    assert_eq!(field(&r, "reason"), "panic", "{r}");
    assert_eq!(field(&r, "lock"), "HeldUntilExit", "{r}");
    held_then_free(&mut h, &dir.path);
    assert!(
        h.stderr().contains("panic in runtime at "),
        "{}",
        h.stderr()
    );
}

/// A participant panicking inside the coordinator: `run` returns at once holding the lock.
#[test]
fn coordinator_panic_returns() {
    struct Bomb;
    impl ShutdownParticipant for Bomb {
        fn name(&self) -> &'static str {
            "bomb"
        }
        fn shutdown(&self, _end: Instant) -> BoxFuture<'_, ()> {
            Box::pin(async { panic!("participant") })
        }
    }
    if in_helper() {
        let comps = TestComponents {
            participants: vec![Box::new(Bomb)],
            ..TestComponents::default()
        };
        helper(fg(), comps, Arc::default(), Duration::from_secs(2), |c| {
            c.serving();
            c.stop();
        });
    }
    let (mut h, dir) = parent("coordinator_panic_returns", &[]);
    let since = h.expect("since_stop_ms", WAIT);
    let ms: u64 = field(&since, "since_stop_ms").parse().unwrap();
    assert!(ms < 1000, "{ms} ms");
    let r = h.expect("returned", WAIT);
    assert_eq!(field(&r, "reason"), "panic", "{r}");
    assert_eq!(field(&r, "lock"), "HeldUntilExit", "{r}");
    held_then_free(&mut h, &dir.path);
}

// ---- PR8-CL-25: stops inside a step ----

/// Barriers inside steps 8, 13, and 14: a stop while parked; a second daemon meanwhile exits
/// 3; released, the task finishes the step, publishes nothing after it, and the lock is
/// released only after the join.
#[test]
fn stop_inside_step() {
    if in_helper() {
        let step: u8 = std::env::var(STEP_ENV).unwrap().parse().unwrap();
        let dir = helper_dir();
        let ready = RecordingReady::new(&dir);
        let port = free_port();
        let faults = Arc::new(Faults::default());
        let park = faults.park(Point::Inside(step));
        let cfg = detached(&dir, Some(port), &ready);
        helper(
            cfg,
            TestComponents::default(),
            faults,
            Duration::ZERO,
            move |c| {
                assert!(park.wait_reached(WAIT));
                c.stop();
                let second = super::start(
                    foreground(&c.dir, Some(free_port())),
                    TestComponents::default(),
                    Arc::default(),
                )
                .join();
                let held = probe(&c.dir).ok() == Some(LockProbe::Held);
                park.release();
                assert!(wait_for(WAIT, || !ready.lines().is_empty()));
                let v = ready.lines().remove(0);
                report(&format!(
                    "checks second={} held_while_parked={held} port_file={} record_at_verdict={} lock_at_verdict={:?} verdict={}",
                    second.code.code(),
                    c.dir.join(names::PORT).exists(),
                    v.record_present,
                    v.lock,
                    v.line.trim_end().replace(' ', "_"),
                ));
            },
        );
    }
    for step in ["8", "13", "14"] {
        let (mut h, dir) = parent("stop_inside_step", &[(STEP_ENV, step)]);
        let checks = h.expect("checks", WAIT);
        let r = h.expect("returned", WAIT);
        assert_eq!(field(&r, "code"), "0", "step {step}: {r}");
        assert_eq!(field(&r, "lock"), "Released", "step {step}: {r}");
        assert_eq!(field(&checks, "second"), "3", "step {step}: {checks}");
        assert_eq!(field(&checks, "held_while_parked"), "true", "step {step}");
        assert_eq!(field(&checks, "record_at_verdict"), "false", "step {step}");
        assert_eq!(
            field(&checks, "lock_at_verdict"),
            "Some(Free)",
            "step {step}"
        );
        let verdict = field(&checks, "verdict");
        assert!(
            verdict.starts_with("fail_stopped_"),
            "step {step}: {verdict}"
        );
        match step {
            "8" => {
                assert_eq!(field(&checks, "port_file"), "false", "step 8");
                assert!(!verdict.contains("port_saved"), "{verdict}");
            }
            _ => {
                assert_eq!(field(&checks, "port_file"), "true", "step {step}");
                assert!(verdict.contains("port_saved="), "{verdict}");
            }
        }
        h.wait(WAIT);
        assert!(!dir.path.join(names::RECORD).exists());
    }
}

/// Step 8 parked past the join end: a bare exit 4–4.5 s after the stop, holding the lock,
/// the record left, no verdict.
#[test]
fn unjoined_startup_is_a_bare_exit() {
    if in_helper() {
        let dir = helper_dir();
        let ready = RecordingReady::new(&dir);
        let faults = Arc::new(Faults::default());
        let park = faults.park(Point::Inside(8));
        let cfg = detached(&dir, Some(free_port()), &ready);
        let main_ready = ready.clone();
        prepare_process();
        let ctl = Ctl {
            faults: faults.clone(),
            dir: dir.clone(),
        };
        std::thread::spawn(move || {
            assert!(park.wait_reached(WAIT));
            ctl.stop();
        });
        let r = run_process_with(cfg, Box::new(TestComponents::default()), faults);
        report(&format!(
            "since_stop_ms={}",
            STOPPED_AT.get().unwrap().elapsed().as_millis()
        ));
        report(&format!(
            "verdict lines={} closed={} record={}",
            main_ready.lines().len(),
            *main_ready.closed.lock().unwrap(),
            dir.join(names::RECORD).exists()
        ));
        helper_exit(&r, Duration::from_secs(2));
    }
    let (mut h, dir) = parent("unjoined_startup_is_a_bare_exit", &[]);
    let since = h.expect("since_stop_ms", WAIT);
    let ms: u64 = field(&since, "since_stop_ms").parse().unwrap();
    assert!((4000..=4500).contains(&ms), "{ms} ms");
    let v = h.expect("verdict", WAIT);
    assert_eq!(field(&v, "lines"), "0", "{v}");
    assert_eq!(field(&v, "closed"), "false", "{v}");
    assert_eq!(field(&v, "record"), "true", "{v}");
    let r = h.expect("returned", WAIT);
    assert_eq!(field(&r, "reason"), "startup_unjoined", "{r}");
    assert_eq!(field(&r, "lock"), "HeldUntilExit", "{r}");
    held_then_free(&mut h, &dir.path);
}

// ---- PR8-CL-26 ----

/// A component's blocking task that never returns does not keep `run` from returning.
#[test]
fn blocking_task_past_the_deadline() {
    if in_helper() {
        let comps = TestComponents {
            on_build: Some(Box::new(|_| {
                tokio::task::spawn_blocking(|| {
                    loop {
                        std::thread::park();
                    }
                });
            })),
            ..TestComponents::default()
        };
        helper(fg(), comps, Arc::default(), Duration::ZERO, |c| {
            c.serving();
            c.stop();
        });
    }
    let (mut h, _dir) = parent("blocking_task_past_the_deadline", &[]);
    let since = h.expect("since_stop_ms", WAIT);
    let ms: u64 = field(&since, "since_stop_ms").parse().unwrap();
    assert!(ms <= 11_000, "{ms} ms");
    let r = h.expect("returned", WAIT);
    assert_eq!(field(&r, "code"), "0", "{r}");
    assert_eq!(h.wait(WAIT), Some(0));
}

/// Two `prepare_process` calls install one hook and one subscriber.
#[test]
fn prepare_is_idempotent() {
    if in_helper() {
        prepare_process();
        prepare_process();
        let set = tracing::dispatcher::has_been_set();
        let again =
            tracing::subscriber::set_global_default(tracing_subscriber::fmt().finish()).is_ok();
        let _ = std::thread::Builder::new()
            .name("x".into())
            .spawn(|| panic!("once"))
            .unwrap()
            .join();
        report(&format!("checks set={set} second_install={again}"));
        super::super::flush_process_log(Duration::from_secs(1));
        std::process::exit(0);
    }
    let (mut h, _dir) = parent("prepare_is_idempotent", &[]);
    let c = h.expect("checks", WAIT);
    assert_eq!(field(&c, "set"), "true", "{c}");
    assert_eq!(field(&c, "second_install"), "false", "{c}");
    assert_eq!(h.wait(WAIT), Some(0));
    assert_eq!(
        h.stderr().matches("panic in thread at ").count(),
        1,
        "{}",
        h.stderr()
    );
}

// ---- PR8-CL-18 (macOS) ----

/// While serving, `SIGCHLD` stays `SIG_DFL` without `SA_NOCLDWAIT`, though the helper started
/// with it ignored.
#[cfg(target_os = "macos")]
#[test]
fn sigchld_default_while_serving() {
    struct ReadSignals;
    impl ShutdownParticipant for ReadSignals {
        fn name(&self) -> &'static str {
            "signals"
        }
        fn shutdown(&self, _end: Instant) -> BoxFuture<'_, ()> {
            let s = chaldea_pty_runtime::host::signal_state().unwrap();
            report(&format!(
                "checks sigchld={:?} nocldwait={}",
                s.sigchld, s.no_cldwait
            ));
            Box::pin(async {})
        }
    }
    if in_helper() {
        chaldea_pty_runtime::fake::ignore_sigchld().unwrap();
        let comps = TestComponents {
            participants: vec![Box::new(ReadSignals)],
            ..TestComponents::default()
        };
        helper(fg(), comps, Arc::default(), Duration::ZERO, |c| {
            c.serving();
            c.stop();
        });
    }
    let (mut h, _dir) = parent("sigchld_default_while_serving", &[]);
    let c = h.expect("checks", WAIT);
    assert_eq!(field(&c, "sigchld"), "Default", "{c}");
    assert_eq!(field(&c, "nocldwait"), "false", "{c}");
    h.wait(WAIT);
}

// ---- PR8-CL-20 ----

/// A foreground daemon's stderr carries `tracing` lines with their secrets redacted.
#[test]
fn foreground_stderr_is_redacted() {
    if in_helper() {
        async fn log_secret(_ctx: WriteCtx, _req: Request<Body>) -> Result<Response, ApiError> {
            tracing::info!(
                field = "chdt_FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
                "marker {TOKEN}"
            );
            Ok(StatusCode::OK.into_response())
        }
        let comps = TestComponents {
            mounts: Some(post_events(Mount::write(events_spec(), log_secret))),
            ..TestComponents::default()
        };
        helper(fg(), comps, Arc::default(), Duration::ZERO, |c| {
            let port = c.serving();
            let (code, _) =
                request(port, "POST", "/events", Some(&token_of(&c.dir)), Some("{}")).unwrap();
            assert_eq!(code, 200);
            c.stop();
        });
    }
    let (mut h, dir) = parent("foreground_stderr_is_redacted", &[]);
    h.expect("returned", WAIT);
    h.wait(WAIT);
    let err = h.stderr();
    assert!(err.contains("marker chd_<redacted>"), "{err}");
    assert!(err.contains("chdt_<redacted>"), "{err}");
    assert!(!err.contains(TOKEN) && !err.contains("chdt_FFFF"), "{err}");
    let owner = token_of(&dir.path);
    assert!(!err.contains(&owner), "{err}");
}
