//! The coordinator (design §2.8) in process: PR8-CL-4 (a clean stop releases), CL-5 (order and
//! ends), CL-23 (only our record), CL-26 (isolated runs register nothing). Console ends keep
//! the lock, so their tests are helper processes (`helpers.rs`).

use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::super::faults::{Faults, Point};
use super::super::files::{LockProbe, probe};
use super::super::gate::{ConsoleEvent, any_registered};
use super::super::{FailReason, LockDisposition, names};
use super::{Recorder, TestComponents, WAIT, foreground, free_port, start, state};
use crate::store::test_faults::{CommitFault, Faults as StoreFaults};

/// PR8-CL-4: a stop with every condition met releases the lock in process.
#[test]
fn exited_writer_releases() {
    let dir = state();
    let run = start(
        foreground(&dir.path, Some(free_port())),
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

/// PR8-CL-5: HTTP, the participants in order, then the store, each with its end measured back
/// from the cutoff.
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
        foreground(&dir.path, Some(free_port())),
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
        ["http", "participant:first", "participant:second", "store"]
    );
    assert_eq!(marks[0].1, Some(c - Duration::from_secs(5)));
    assert_eq!(marks[1].1, Some(c - Duration::from_secs(3)));
    assert_eq!(marks[2].1, Some(c - Duration::from_secs(3)));
    assert_eq!(marks[3].1, Some(c));
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
        foreground(&dir.path, Some(free_port())),
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
        foreground(&dir.path, Some(free_port())),
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
        foreground(&a.path, Some(free_port())),
        TestComponents::default(),
        Arc::default(),
    );
    let rb = start(
        foreground(&b.path, Some(free_port())),
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
        foreground(&dir.path, Some(free_port())),
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
        foreground(&dir.path, Some(free_port())),
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
        foreground(&dir.path, Some(free_port())),
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
        foreground(&dir.path, Some(free_port())),
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
        foreground(&dir.path, Some(free_port())),
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
            foreground(&dir.path, Some(free_port())),
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
            foreground(&dir.path, Some(free_port())),
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
        ApiError, CommandEffect, CommandHandler, CommandRegistry, EffectRun, Intent, Prepared,
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
        fn run(&self, _run: EffectRun) -> BoxFuture<'static, CommandResult> {
            let gate = self.0.lock().unwrap().take();
            Box::pin(async move {
                if let Some((started, release)) = gate {
                    let _ = started.send(());
                    let _ = release.await;
                }
                CommandResult {
                    http_status: 200,
                    body: "{}".to_owned(),
                }
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
        foreground(&dir.path, Some(free_port())),
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

/// A re-read that would first succeed after the 500 ms budget issues no deletion: the wait
/// is capped at what remains and the deadline is checked before each attempt.
#[test]
fn no_deletion_after_the_record_budget() {
    use std::sync::atomic::Ordering;
    let dir = state();
    let faults = Arc::new(Faults::default());
    *faults.record_retry.lock().unwrap() = Some(Duration::from_millis(300));
    let run = start(
        foreground(&dir.path, Some(free_port())),
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
