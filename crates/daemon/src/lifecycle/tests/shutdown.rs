//! The coordinator (design §2.8) in process: PR8-CL-4 (a clean stop releases), CL-5 (order and
//! ends), CL-23 (only our record), CL-26 (isolated runs register nothing). Console ends keep
//! the lock, so their tests are helper processes (`helpers.rs`).

use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::super::faults::{Faults, Point};
use super::super::files::{LockProbe, probe};
use super::super::gate::{ConsoleEvent, any_registered};
use super::super::{FailReason, LockDisposition, names};
use super::{Recorder, TestComponents, WAIT, foreground, hold_port, start, state};
use crate::store::test_faults::{CommitFault, Faults as StoreFaults};

/// PR8-CL-4: a stop with every condition met releases the lock in process.
#[test]
fn exited_writer_releases() {
    let dir = state();
    let run = start(
        foreground(&dir.path, Some(hold_port())),
        TestComponents::default(),
        Arc::default(),
    );
    run.serving();
    run.stop();
    let log = run.log.clone();
    let r = run.join();
    assert_eq!(
        (r.code.code(), r.reason, r.lock),
        (0, None, LockDisposition::Released)
    );
    assert!(
        log.text()
            .contains("store shutdown: readers_returned true, writer_exited true"),
        "{}",
        log.text()
    );
    assert_eq!(probe(&dir.path).unwrap(), LockProbe::Free);
}

/// PR8-CL-5: the record's `stopping` rewrite, HTTP, the participants in order, then the
/// store, each with its end measured back from the cutoff (the rewrite's within HTTP's).
#[test]
fn order_and_ends() {
    let dir = state();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let comps = TestComponents {
        participants: vec![
            Box::new(Recorder {
                name: "first",
                seen: seen.clone(),
            }),
            Box::new(Recorder {
                name: "second",
                seen: seen.clone(),
            }),
        ],
        ..TestComponents::default()
    };
    let faults = Arc::new(Faults::default());
    let sf = Arc::new(StoreFaults::default());
    *faults.store_faults.lock().unwrap() = Some(sf.clone());
    let run = start(
        foreground(&dir.path, Some(hold_port())),
        comps,
        faults.clone(),
    );
    run.serving();
    let gate = run.gate();
    run.stop();
    let r = run.join();
    assert_eq!(r.code.code(), 0);
    let c = gate.stopping().unwrap().cutoff;
    // The real `Store::shutdown` received exactly the cutoff.
    assert_eq!(*sf.shutdown_deadline.lock().unwrap(), Some(c));
    let marks: Vec<(String, Option<Instant>)> = faults.marks();
    let names: Vec<&str> = marks.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "stopping",
            "http",
            "participant:first",
            "participant:second",
            "store"
        ]
    );
    assert!(marks[0].1.is_some_and(|e| e <= c - Duration::from_secs(5)));
    assert_eq!(marks[1].1, Some(c - Duration::from_secs(5)));
    assert_eq!(marks[2].1, Some(c - Duration::from_secs(3)));
    assert_eq!(marks[3].1, Some(c - Duration::from_secs(3)));
    assert_eq!(marks[4].1, Some(c));
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.iter().map(|s| s.0).collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert!(seen.iter().all(|s| s.2 == c - Duration::from_secs(3)));
}

/// PR8-CL-23: a record naming another instance is never removed; our own is.
#[test]
fn removes_only_its_own_record() {
    let dir = state();
    let run = start(
        foreground(&dir.path, Some(hold_port())),
        TestComponents::default(),
        Arc::default(),
    );
    run.serving();
    let path = dir.path.join(names::RECORD);
    let ours = std::fs::read_to_string(&path).unwrap();
    let theirs = ours.replace(
        &serde_json::from_str::<serde_json::Value>(&ours).unwrap()["instance"]
            .as_str()
            .unwrap()
            .to_owned(),
        "00000000-0000-4000-8000-0000000000ff",
    );
    std::fs::write(&path, &theirs).unwrap();
    run.stop();
    run.join();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs);

    std::fs::remove_file(&path).unwrap();
    let run = start(
        foreground(&dir.path, Some(hold_port())),
        TestComponents::default(),
        Arc::default(),
    );
    run.serving();
    assert!(path.exists());
    run.stop();
    run.join();
    assert!(!path.exists());
}

/// PR8-CL-26: two parallel isolated runs register no gate and keep their diagnostics apart.
#[test]
fn isolated_runs_register_nothing() {
    let (a, b) = (state(), state());
    let ra = start(
        foreground(&a.path, Some(hold_port())),
        TestComponents::default(),
        Arc::default(),
    );
    let rb = start(
        foreground(&b.path, Some(hold_port())),
        TestComponents::default(),
        Arc::default(),
    );
    let (pa, pb) = (ra.serving(), rb.serving());
    assert!(!any_registered());
    ra.stop();
    rb.stop();
    let (la, lb) = (ra.log.clone(), rb.log.clone());
    ra.join();
    rb.join();
    assert!(!any_registered());
    assert!(la.text().contains(&format!("127.0.0.1:{pa}")));
    assert!(!la.text().contains(&format!("127.0.0.1:{pb}")));
    assert!(lb.text().contains(&format!("127.0.0.1:{pb}")));
}

/// A console end after the lock decision of a run that never took the lock reports
/// `NeverTaken`, not a held lock.
#[test]
fn console_end_after_the_lock_decision_keeps_never_taken() {
    let dir = state();
    let faults = Arc::new(Faults::default());
    let before_lock = faults.park(Point::Before(5));
    let decided = faults.park(Point::AfterLockDecision);
    let run = start(
        foreground(&dir.path, Some(hold_port())),
        TestComponents::default(),
        faults,
    );
    assert!(before_lock.wait_reached(WAIT));
    run.stop();
    before_lock.release();
    assert!(decided.wait_reached(WAIT));
    run.gate().crash_exit(ConsoleEvent::Close);
    decided.release();
    let r = run.join();
    assert_eq!(
        (r.reason, r.lock),
        (Some(FailReason::ConsoleEnd), LockDisposition::NeverTaken)
    );
}

/// PR8-CL-19 (§2.9): a panic before the runtime exists took no lock.
#[test]
fn panic_before_the_runtime_is_never_taken() {
    let dir = state();
    let faults = Arc::new(Faults::default());
    faults
        .panic_before_runtime
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let r = start(
        foreground(&dir.path, Some(hold_port())),
        TestComponents::default(),
        faults,
    )
    .join();
    assert_eq!(
        (r.code.code(), r.reason, r.lock),
        (1, Some(FailReason::Panic), LockDisposition::NeverTaken)
    );
    assert!(!dir.path.exists());
}

/// A step-12 owner-token write still in flight when the start fails is drained with HTTP
/// (the server never served): its fatal outcome reaches the exit reason.
#[test]
fn token_write_is_drained_before_serve() {
    let dir = state();
    let sf = Arc::new(StoreFaults::default());
    let faults = Arc::new(Faults::default());
    *faults.store_faults.lock().unwrap() = Some(sf.clone());
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(Some(release_rx));
    let comps = TestComponents {
        on_build: Some(Box::new(move |ctx| {
            let rx = release_rx.lock().unwrap().take().unwrap();
            // Holds the writer, so the token issue of step 12 waits in the queue past its
            // reply cutoff.
            let ticket = ctx
                .store
                .submit(Instant::now() + WAIT, move |_tx| {
                    entered_tx.send(()).unwrap();
                    let _ = rx.recv_timeout(WAIT);
                    Ok(())
                })
                .unwrap();
            drop(ticket);
        })),
        ..TestComponents::default()
    };
    let run = start(
        foreground(&dir.path, Some(hold_port())),
        comps,
        faults.clone(),
    );
    entered_rx.recv_timeout(WAIT).unwrap();
    // The start fails at step 12 (the issue's reply never came); HTTP drains its observer.
    assert!(
        super::wait_for(WAIT, || faults.marks().iter().any(|m| m.0 == "http")),
        "HTTP was not drained: {:?}\n{}",
        faults.marks(),
        run.log.text()
    );
    // The held job's commit is reported unknown; the writer fails every queued job after it.
    *sf.commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    release_tx.send(()).unwrap();
    let log = run.log.clone();
    let r = run.join();
    assert_eq!(
        (r.code.code(), r.reason),
        (1, Some(FailReason::StoreFatal)),
        "{}",
        log.text()
    );
    let marks: Vec<String> = faults.marks().into_iter().map(|m| m.0).collect();
    let http = marks.iter().position(|m| m == "http").unwrap();
    let store = marks.iter().position(|m| m == "store").unwrap();
    assert!(http < store, "{marks:?}");
}

/// Windows: a reader holding `daemon.json` without delete sharing keeps it from being
/// removed; the stop retries, then names the record in a warning.
#[cfg(windows)]
#[test]
fn record_removal_failure_is_reported() {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    let dir = state();
    let run = start(
        foreground(&dir.path, Some(hold_port())),
        TestComponents::default(),
        Arc::default(),
    );
    run.serving();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(dir.path.join(names::RECORD))
        .unwrap();
    run.stop();
    let log = run.log.clone();
    let faults = run.faults.clone();
    let r = run.join();
    drop(held);
    assert_eq!(r.code.code(), 0, "{}", log.text());
    assert!(
        log.text().contains("daemon.json") && log.text().contains("could not be removed"),
        "{}",
        log.text()
    );
    // The warning comes after retries, not after the first refusal.
    assert!(
        faults
            .record_remove_attempts
            .load(std::sync::atomic::Ordering::SeqCst)
            > 1
    );
}

/// Another instance's record that replaces ours while a removal is retried survives: each
/// attempt removes only a record that still names this run.
#[test]
fn record_retry_spares_a_replacement() {
    let dir = state();
    let faults = Arc::new(Faults::default());
    faults
        .record_remove_fails
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let park = faults.park(Point::RecordRetry);
    let run = start(
        foreground(&dir.path, Some(hold_port())),
        TestComponents::default(),
        faults,
    );
    run.serving();
    let path = dir.path.join(names::RECORD);
    run.stop();
    assert!(park.wait_reached(WAIT));
    let ours = std::fs::read_to_string(&path).unwrap();
    let theirs = ours.replace(
        serde_json::from_str::<serde_json::Value>(&ours).unwrap()["instance"]
            .as_str()
            .unwrap(),
        "00000000-0000-4000-8000-0000000000fe",
    );
    std::fs::write(&path, &theirs).unwrap();
    park.release();
    run.join();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs);
}

/// A removal refused twice succeeds on its third attempt, without a warning; one refused on
/// every attempt is retried, then named in a warning.
#[test]
fn record_removal_is_retried() {
    use std::sync::atomic::Ordering;
    for (fails, removed) in [(2, true), (u32::MAX, false)] {
        let dir = state();
        let faults = Arc::new(Faults::default());
        faults.record_remove_fails.store(fails, Ordering::SeqCst);
        let run = start(
            foreground(&dir.path, Some(hold_port())),
            TestComponents::default(),
            faults.clone(),
        );
        run.serving();
        run.stop();
        let log = run.log.clone();
        let r = run.join();
        assert_eq!(r.code.code(), 0, "{}", log.text());
        let attempts = faults.record_remove_attempts.load(Ordering::SeqCst);
        let path = dir.path.join(names::RECORD);
        if removed {
            assert_eq!(attempts, 3);
            assert!(!path.exists());
            assert!(
                !log.text().contains("could not be removed"),
                "{}",
                log.text()
            );
        } else {
            assert!(attempts > 1, "{attempts}");
            assert!(path.exists());
            assert!(
                log.text().contains("daemon.json") && log.text().contains("could not be removed"),
                "{}",
                log.text()
            );
        }
    }
}

/// A re-read of the record that fails (a Windows sharing violation) is retried within the
/// removal's deadline; one that keeps failing is named in a warning.
#[test]
fn record_verify_failure_is_retried() {
    use std::sync::atomic::Ordering;
    for (fails, removed) in [(1, true), (u32::MAX, false)] {
        let dir = state();
        let faults = Arc::new(Faults::default());
        let run = start(
            foreground(&dir.path, Some(hold_port())),
            TestComponents::default(),
            faults.clone(),
        );
        run.serving();
        faults.record_verify_fails.store(fails, Ordering::SeqCst);
        run.stop();
        let log = run.log.clone();
        let r = run.join();
        assert_eq!(r.code.code(), 0, "{}", log.text());
        let path = dir.path.join(names::RECORD);
        if removed {
            assert!(!path.exists(), "{}", log.text());
        } else {
            assert!(path.exists());
            assert!(
                log.text().contains("daemon.json") && log.text().contains("could not be verified"),
                "{}",
                log.text()
            );
        }
    }
}

/// The command handlers the components built are the ones the server runs, and HTTP's drain
/// waits for their runner: a command whose effect is still running when the stop arrives
/// completes before the store shuts down, and the lock is released.
#[test]
fn built_commands_are_served_and_drained() {
    use chaldea_core::auth::{Action, CommandSpec};
    use chaldea_core::canonical::CanonicalJson;
    use chaldea_core::command::{CommandResult, ReceiptStatus};
    use futures_util::future::BoxFuture;
    use serde_json::value::RawValue;

    use crate::http::{
        ApiError, CommandEffect, CommandHandler, CommandRegistry, EffectOutcome, EffectRun, Intent,
        Prepared,
    };

    static SPECS: &[CommandSpec] = &[CommandSpec {
        name: "test_lifecycle_effect",
        action: Action::Send,
        owner_pr: 8,
    }];
    type Gate = Mutex<Option<(mpsc::Sender<()>, tokio::sync::oneshot::Receiver<()>)>>;
    struct Held(Gate);
    impl CommandHandler for Held {
        fn name(&self) -> &'static str {
            "test_lifecycle_effect"
        }
        // Short, so the request replies (409 `in_progress`) while the effect is still held.
        fn deadline(&self) -> Option<Duration> {
            Some(Duration::from_millis(300))
        }
        fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError> {
            let v: serde_json::Value = serde_json::from_str(args.get()).unwrap();
            Ok(Prepared {
                normalized: CanonicalJson::of(&v).unwrap(),
                intent: Box::new(|_tx, _auth, _ctx| Ok(Intent::Pending(EffectRun::new(())))),
            })
        }
        fn effect(&self) -> Option<&dyn CommandEffect> {
            Some(self)
        }
    }
    impl CommandEffect for Held {
        fn run(&self, _run: EffectRun) -> BoxFuture<'static, EffectOutcome> {
            let gate = self.0.lock().unwrap().take();
            Box::pin(async move {
                if let Some((started, release)) = gate {
                    let _ = started.send(());
                    let _ = release.await;
                }
                EffectOutcome::Done(CommandResult {
                    http_status: 200,
                    body: "{}".to_owned(),
                })
            })
        }
    }

    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let mut commands = CommandRegistry::mvp_plus(SPECS);
    commands.register(Box::new(Held(Mutex::new(Some((started_tx, release_rx))))));
    let dir = state();
    let faults = Arc::new(Faults::default());
    let run = start(
        foreground(&dir.path, Some(hold_port())),
        TestComponents {
            commands: Some(commands),
            ..TestComponents::default()
        },
        faults.clone(),
    );
    let port = run.serving();
    let token = super::token_of(&dir.path);
    let cmd = chaldea_core::ids::CommandId::from_u128(0x8c);
    let body = format!(r#"{{"command_id":"{cmd}","args":{{}}}}"#);
    let client = std::thread::spawn(move || {
        super::request(
            port,
            "POST",
            "/api/commands/test_lifecycle_effect",
            Some(&token),
            Some(&body),
        )
    });
    started_rx
        .recv_timeout(WAIT)
        .expect("the registered handler's effect did not start");
    // The request has ended and its connection left the registry; the held effect's runner
    // is what remains registered.
    let (status, reply) = client.join().unwrap().unwrap();
    assert_eq!(status, 409, "{reply}");
    let registry = faults.http_registry.get().unwrap().clone();
    assert!(
        super::wait_for(WAIT, || registry
            .counts()
            .get(crate::http::TaskKind::Connection)
            == 0),
        "{registry:?}"
    );
    assert_eq!(registry.counts().get(crate::http::TaskKind::Runner), 1);
    run.stop();
    assert!(super::wait_for(WAIT, || faults
        .marks()
        .iter()
        .any(|m| m.0 == "http")));
    // The drain waits for the runner: the store is not shut down while the effect is held.
    assert!(super::wait_for(WAIT, || registry.is_closing()));
    assert_eq!(registry.counts().get(crate::http::TaskKind::Runner), 1);
    assert!(!faults.marks().iter().any(|m| m.0 == "store"));
    let _ = release_tx.send(());
    let log = run.log.clone();
    let r = run.join();
    assert_eq!(
        (r.code.code(), r.lock),
        (0, LockDisposition::Released),
        "{}",
        log.text()
    );
    let marks: Vec<String> = faults.marks().into_iter().map(|m| m.0).collect();
    let at = |w: &str| marks.iter().position(|m| m == w).unwrap();
    assert!(at("http") < at("store"), "{marks:?}");
    let store = super::open_store(&dir.path);
    let receipt = store
        .read(Instant::now() + WAIT, move |tx| tx.receipt(cmd))
        .unwrap()
        .expect("a receipt");
    super::startup::close(&store);
    assert!(
        matches!(receipt.status, ReceiptStatus::Completed(ref c) if c.http_status == 200),
        "{receipt:?}"
    );
}

/// PR8-CL-23: a removal whose re-read returned before the 500 ms budget but reaches the unlink
/// only after it (paused there) issues no deletion: the budget is checked again just before
/// the unlink.
#[test]
fn no_unlink_after_the_record_budget() {
    let dir = state();
    let faults = Arc::new(Faults::default());
    let park = faults.park(Point::AfterRecordVerify);
    let run = start(
        foreground(&dir.path, Some(hold_port())),
        TestComponents::default(),
        faults.clone(),
    );
    run.serving();
    run.stop();
    assert!(park.wait_reached(WAIT));
    // The budget began before the pause was reached.
    std::thread::sleep(Duration::from_millis(600));
    park.release();
    let log = run.log.clone();
    let r = run.join();
    assert_eq!(r.code.code(), 0, "{}", log.text());
    assert!(dir.path.join(names::RECORD).exists(), "{}", log.text());
    assert!(log.text().contains("was left in place"), "{}", log.text());
}

/// PR8-CL-23: the record says `stopping` before HTTP shuts down. The rewrite has a
/// test-set budget of `WAIT` (and the run a shutdown budget long enough to hold it), so a
/// slow write or a held rewrite does not run out of time; `stalled_stopping_rewrite_does_not_hold_http`
/// covers the 500 ms budget. While the rewrite is held, HTTP still answers and the coordinator
/// has not reached HTTP's shutdown; once it reaches it, the record already says `stopping`
/// with this run's instance, and HTTP still answers.
#[test]
fn record_says_stopping_before_http_shuts_down() {
    use super::super::files::{Discovery, discover};
    let dir = state();
    let faults = Arc::new(Faults::default());
    *faults.stopping_rewrite.lock().unwrap() = Some(WAIT);
    let rewriting = faults.park(Point::InsideStopping);
    let before_http = faults.park(Point::BeforeHttp);
    let mut cfg = foreground(&dir.path, Some(hold_port()));
    cfg.budgets.total = 3 * WAIT;
    let run = start(cfg, TestComponents::default(), faults.clone());
    let port = run.serving();
    let Discovery::Serving(serving) = discover(&dir.path) else {
        panic!("not serving: {:?}", discover(&dir.path))
    };
    let token = super::token_of(&dir.path);
    let answers = || super::get_status(port, "/api/threads", Some(&token)).is_ok_and(|s| s == 200);
    run.stop();
    let reached = rewriting.wait_reached(WAIT);
    let http_up = answers();
    let http_began = before_http.wait_reached(Duration::ZERO);
    rewriting.release();
    assert!(reached, "the rewrite did not start");
    assert!(http_up, "HTTP shut down before the rewrite");
    assert!(
        !http_began,
        "HTTP's shutdown began while the rewrite was held"
    );
    assert!(before_http.wait_reached(WAIT));
    let seen = discover(&dir.path);
    let http_up = answers();
    before_http.release();
    let log = run.log.clone();
    let r = run.join();
    assert_eq!(r.code.code(), 0, "{}", log.text());
    assert!(
        matches!(&seen, Discovery::Stopping(s) if s.instance == serving.instance),
        "{seen:?}\n{}",
        log.text()
    );
    assert!(http_up, "HTTP shut down before its shutdown step");
}

/// A `stopping` rewrite that does not finish within its 500 ms does not hold HTTP's
/// shutdown back: shutdown goes on and the log says why.
#[test]
fn stalled_stopping_rewrite_does_not_hold_http() {
    let dir = state();
    let faults = Arc::new(Faults::default());
    let rewriting = faults.park(Point::InsideStopping);
    let run = start(
        foreground(&dir.path, Some(hold_port())),
        TestComponents::default(),
        faults.clone(),
    );
    run.serving();
    run.stop();
    let reached = rewriting.wait_reached(WAIT);
    let http_went_on = super::wait_for(WAIT, || faults.marks().iter().any(|m| m.0 == "http"));
    rewriting.release();
    assert!(reached, "the rewrite did not start");
    assert!(http_went_on, "HTTP waited for the rewrite");
    let log = run.log.clone();
    let r = run.join();
    assert_eq!(r.code.code(), 0, "{}", log.text());
    assert!(
        log.text().contains("was not marked stopping within 500 ms"),
        "{}",
        log.text()
    );
}

/// A `stopping` rewrite that fails is logged, and shutdown goes on to a clean exit.
#[test]
fn failed_stopping_rewrite_is_logged() {
    let dir = state();
    let faults = Arc::new(Faults::default());
    let run = start(
        foreground(&dir.path, Some(hold_port())),
        TestComponents::default(),
        faults.clone(),
    );
    run.serving();
    assert!(super::wait_for(WAIT, || matches!(
        super::super::files::discover(&dir.path),
        super::super::files::Discovery::Serving(_)
    )));
    faults.fail_replace(names::RECORD, super::super::files::ReplaceStage::Write);
    run.stop();
    let log = run.log.clone();
    let r = run.join();
    assert_eq!(
        (r.code.code(), r.lock),
        (0, LockDisposition::Released),
        "{}",
        log.text()
    );
    assert!(
        log.text().contains("could not be marked stopping"),
        "{}",
        log.text()
    );
}

/// A re-read that would first succeed after the 500 ms budget issues no deletion: the wait
/// is capped at what remains and the deadline is checked before each attempt.
#[test]
fn no_deletion_after_the_record_budget() {
    use std::sync::atomic::Ordering;
    let dir = state();
    let faults = Arc::new(Faults::default());
    *faults.record_retry.lock().unwrap() = Some(Duration::from_millis(300));
    let run = start(
        foreground(&dir.path, Some(hold_port())),
        TestComponents::default(),
        faults.clone(),
    );
    run.serving();
    // Two refused re-reads: at 0 ms and at 300 ms; the next would be past 500 ms.
    faults.record_verify_fails.store(2, Ordering::SeqCst);
    run.stop();
    let log = run.log.clone();
    let r = run.join();
    assert_eq!(r.code.code(), 0, "{}", log.text());
    assert!(dir.path.join(names::RECORD).exists(), "{}", log.text());
    assert!(
        log.text().contains("could not be verified"),
        "{}",
        log.text()
    );
}

/// A record file of `instance` in `dir`, serving; returns its bytes.
fn serving_record(dir: &std::path::Path, instance: &str) -> Vec<u8> {
    use super::super::files::{DiscoveryRecord, Evidence, RecordState};
    let bytes = DiscoveryRecord {
        version: 1,
        instance: instance.to_owned(),
        pid: std::process::id(),
        start_time: "1".to_owned(),
        state: RecordState::Serving,
        port: Some(7373),
        url: Some("http://127.0.0.1:7373".to_owned()),
        started_at: "2026-11-23T00:00:00.000Z".to_owned(),
        detached: None,
        evidence: Evidence::default(),
    }
    .to_bytes();
    std::fs::write(dir.join(names::RECORD), &bytes).unwrap();
    bytes
}

/// The `stopping` rewrite's thread starting at or after its cutoff does nothing: it does not
/// read the record, creates no temporary file, and leaves `daemon.json` as it was.
#[test]
fn stopping_worker_started_late_does_nothing() {
    let d = tempfile::tempdir().unwrap();
    let before = serving_record(d.path(), "i");
    let read = std::sync::atomic::AtomicBool::new(false);
    let seen = Mutex::new(Vec::new());
    let hook = |_: &str, stage| {
        seen.lock().unwrap().push(stage);
        Ok(())
    };
    let r = super::super::shutdown::mark_stopping(
        d.path(),
        "i",
        Instant::now(),
        &|| read.store(true, std::sync::atomic::Ordering::SeqCst),
        &hook,
    );
    assert!(r.is_err());
    assert!(
        !read.load(std::sync::atomic::Ordering::SeqCst),
        "read past the cutoff"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "{:?}",
        seen.lock().unwrap()
    );
    assert_eq!(std::fs::read(d.path().join(names::RECORD)).unwrap(), before);
}

/// The `stopping` rewrite's thread paused after its read until its cutoff creates no
/// temporary file and leaves `daemon.json` as it was; with time left it rewrites the record.
#[test]
fn stopping_worker_paused_past_its_cutoff_creates_nothing() {
    let d = tempfile::tempdir().unwrap();
    let before = serving_record(d.path(), "i");
    let seen = Mutex::new(Vec::new());
    let hook = |_: &str, stage| {
        seen.lock().unwrap().push(stage);
        Ok(())
    };
    let end = Instant::now() + Duration::from_millis(200);
    let pause = || std::thread::sleep(end.saturating_duration_since(Instant::now()));
    let r = super::super::shutdown::mark_stopping(d.path(), "i", end, &pause, &hook);
    assert!(r.is_err());
    let stages = seen.lock().unwrap().clone();
    assert!(stages.is_empty(), "{stages:?}");
    let temps: Vec<_> = std::fs::read_dir(d.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(temps.is_empty(), "{temps:?}");
    assert_eq!(std::fs::read(d.path().join(names::RECORD)).unwrap(), before);
    let soon = Instant::now() + WAIT;
    super::super::shutdown::mark_stopping(d.path(), "i", soon, &|| {}, &hook).unwrap();
    assert!(matches!(
        super::super::files::read_record(d.path()),
        Ok(Some(r)) if r.state == super::super::files::RecordState::Stopping
    ));
}

/// The `stopping` rewrite's failing rename is retried, only before the rewrite's cutoff and
/// never for a fresh retry window; the rewrite gives up only at its cutoff. A run that ends at
/// its cutoff with fewer than two renames (a loaded host's temporary write and sync took the
/// time) shows nothing about the retries: it is checked for what it did and repeated with
/// twice the time, from 150 ms.
#[test]
fn stopping_rename_is_not_retried_past_its_cutoff() {
    use super::super::files::ReplaceStage;
    let mut budget = Duration::from_millis(150);
    loop {
        let d = tempfile::tempdir().unwrap();
        let before = serving_record(d.path(), "i");
        let renames = Mutex::new(Vec::new());
        let hook = |_: &str, stage| {
            if stage == ReplaceStage::Rename {
                renames.lock().unwrap().push(Instant::now());
                return Err(std::io::Error::other("injected"));
            }
            Ok(())
        };
        let end = Instant::now() + budget;
        let r = super::super::shutdown::mark_stopping(d.path(), "i", end, &|| {}, &hook);
        let returned = Instant::now();
        let renames = renames.into_inner().unwrap();
        let why = r.expect_err("every rename fails");
        assert!(renames.iter().all(|&t| t < end), "{renames:?} {end:?}");
        assert_eq!(std::fs::read(d.path().join(names::RECORD)).unwrap(), before);
        assert!(
            returned >= end,
            "gave up before its cutoff after {} renames: {why}",
            renames.len()
        );
        // The injected error is the rename's own; any other error before a rename is a failure.
        let cutoff =
            why.contains("its cutoff passed before the rename") || why.contains(" ms ended");
        assert!(
            cutoff || (!renames.is_empty() && why.contains("injected")),
            "{why}"
        );
        if renames.len() >= 2 {
            return;
        }
        assert!(
            budget < Duration::from_secs(4),
            "{} renames before a {budget:?} cutoff: {why}",
            renames.len()
        );
        budget *= 2;
    }
}

/// Step 6's lock rule counts the participants: one that answers `LeftRunning`, and one not
/// done by its end, each keep the instance lock held until exit (`lock_held_at_exit`); a
/// participant that answers `Clean` keeps nothing.
#[test]
fn participant_left_running_keeps_the_lock() {
    use futures_util::future::BoxFuture;

    use super::super::{ParticipantEnd, ShutdownParticipant};

    enum How {
        Answer(ParticipantEnd),
        PastItsEnd,
    }
    struct Ends(How);
    impl ShutdownParticipant for Ends {
        fn name(&self) -> &'static str {
            "ends"
        }
        fn shutdown(&self, _end: Instant) -> BoxFuture<'_, ParticipantEnd> {
            match self.0 {
                How::Answer(e) => Box::pin(async move { e }),
                How::PastItsEnd => Box::pin(std::future::pending()),
            }
        }
    }
    for (how, want) in [
        (
            How::Answer(ParticipantEnd::Clean),
            (None, LockDisposition::Released),
        ),
        (
            How::Answer(ParticipantEnd::LeftRunning),
            (
                Some(FailReason::LockHeldAtExit),
                LockDisposition::HeldUntilExit,
            ),
        ),
        (
            How::PastItsEnd,
            (
                Some(FailReason::LockHeldAtExit),
                LockDisposition::HeldUntilExit,
            ),
        ),
    ] {
        let dir = state();
        let mut cfg = foreground(&dir.path, Some(hold_port()));
        // A participant window of 1 s.
        cfg.budgets.total = Duration::from_secs(5);
        cfg.budgets.startup_join_before = Duration::from_secs(4);
        cfg.budgets.http_before = Duration::from_secs(4);
        cfg.budgets.store_before = Duration::from_secs(3);
        let comps = TestComponents {
            participants: vec![Box::new(Ends(how))],
            ..TestComponents::default()
        };
        let run = start(cfg, comps, Arc::default());
        run.serving();
        run.stop();
        let r = run.join();
        assert_eq!((r.reason, r.lock), want);
        assert_eq!(r.code.code() == 0, want.0.is_none(), "{:?}", r.reason);
    }
}
