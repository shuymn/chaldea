//! The startup task (design §2.7) in process: PR8-CL-2 (database links), CL-6, CL-7, CL-8,
//! CL-21 (a store error never reissues), CL-22 (ports), CL-25 (failures and stops at step
//! boundaries).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use chaldea_core::command::ReceiptStatus;
use chaldea_core::ids::CommandId;

use super::super::faults::{Faults, Point};
use super::super::files::{LockProbe, ReplaceStage};
use super::super::{FailReason, LockDisposition, PORT_RANGE, Reconciler, names};
use super::{
    RecordingReady, TestComponents, WAIT, detached, foreground, free_port, get_status, open_store,
    request, start, state, token_of,
};
use crate::http::{ApiError, Mount, Mounts, WriteCtx, spec};
use crate::store::test_faults::{CommitFault, Faults as StoreFaults};
use crate::store::{JobError, Store};

const CMD: CommandId = CommandId::from_u128(0x8a);

pub(super) fn close(store: &Store) {
    let r = store.shutdown(Instant::now() + Duration::from_secs(10));
    assert!(r.writer_exited && r.readers_returned, "{r:?}");
}

pub(super) fn receipt(dir: &Path) -> Option<ReceiptStatus> {
    let store = open_store(dir);
    let r = store
        .read(Instant::now() + Duration::from_secs(5), |tx| {
            tx.receipt(CMD)
        })
        .unwrap();
    close(&store);
    r.map(|r| r.status)
}

/// `POST /events` mounted with a handler that begins the receipt of `CMD` (an intent job).
pub(super) fn intent_mounts() -> Mounts {
    async fn h(ctx: WriteCtx, _req: Request<Body>) -> Result<Response, ApiError> {
        ctx.write(|tx, auth| {
            let a = auth.ok_or_else(|| JobError::Refused("no principal".into()))?;
            tx.receipt_begin(CMD, a.token_id, [7; 32])?;
            Ok(())
        })
        .await?;
        Ok(StatusCode::OK.into_response())
    }
    Mounts::mvp().with(Mount::write(spec(&Method::POST, "/events").unwrap(), h))
}

/// A reconciler parked inside its job (on the writer thread) until released.
fn parked_reconciler(park: Arc<super::super::faults::Park>) -> Reconciler {
    Reconciler {
        name: "parked",
        job: Box::new(move |_tx| {
            park.hold_blocking();
            Ok(0)
        }),
    }
}

/// PR8-CL-6: no listener exists while a reconciler runs; once it commits, the daemon serves.
#[test]
fn no_listener_during_reconciliation() {
    let dir = state();
    let port = free_port();
    let park = Arc::new(super::super::faults::Park::default());
    let comps = TestComponents {
        extra: vec![parked_reconciler(park.clone())],
        ..TestComponents::default()
    };
    let run = start(foreground(&dir.path, Some(port)), comps, Arc::default());
    assert!(park.wait_reached(WAIT));
    let refused = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(2),
    );
    assert!(refused.is_err(), "a listener existed during reconciliation");
    park.release();
    assert_eq!(run.serving(), port);
    assert_eq!(
        get_status(port, "/api/threads", Some(&token_of(&dir.path))).unwrap(),
        200
    );
    run.stop();
    assert_eq!(run.join().code.code(), 0);
}

/// PR8-CL-7: `SubmitError::Failed` at step 9 is a fatal store outcome.
#[test]
fn submit_failed_is_fatal() {
    let dir = state();
    let store_faults = Arc::new(StoreFaults::default());
    let faults = Arc::new(Faults::default());
    *faults.store_faults.lock().unwrap() = Some(store_faults.clone());
    let park = faults.park(Point::Before(9));
    let run = start(
        foreground(&dir.path, Some(free_port())),
        TestComponents::default(),
        faults,
    );
    assert!(park.wait_reached(WAIT));
    // Retire the writer: a commit whose outcome is unknown puts it in the failed state.
    *store_faults.commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    let store = run.store();
    let r = super::settle(
        store
            .submit(Instant::now() + Duration::from_secs(5), |_tx| Ok(()))
            .unwrap(),
    );
    assert!(r.is_err());
    park.release();
    let r = run.join();
    assert_eq!((r.code.code(), r.reason), (1, Some(FailReason::StoreFatal)));
}

/// PR8-CL-8: an intent job that commits and then meets `CommitUnknown` while serving stops
/// the daemon; the receipt is durably `in_progress`; the next start reconciles it.
#[test]
fn commit_unknown_stops_and_restart_reconciles() {
    let dir = state();
    let store_faults = Arc::new(StoreFaults::default());
    let faults = Arc::new(Faults::default());
    *faults.store_faults.lock().unwrap() = Some(store_faults.clone());
    let comps = TestComponents {
        mounts: Some(intent_mounts()),
        ..TestComponents::default()
    };
    let run = start(foreground(&dir.path, Some(free_port())), comps, faults);
    let port = run.serving();
    *store_faults.commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    let (code, _) = request(
        port,
        "POST",
        "/events",
        Some(&token_of(&dir.path)),
        Some("{}"),
    )
    .unwrap();
    assert_eq!(code, 503);
    let r = run.join();
    assert_eq!((r.code.code(), r.reason), (1, Some(FailReason::StoreFatal)));
    assert_eq!(receipt(&dir.path), Some(ReceiptStatus::InProgress));

    let again = start(
        foreground(&dir.path, Some(free_port())),
        TestComponents::default(),
        Arc::default(),
    );
    again.serving();
    again.stop();
    assert_eq!(again.join().code.code(), 0);
    assert_eq!(receipt(&dir.path), Some(ReceiptStatus::UnknownAfterCrash));
}

/// PR8-CL-2: a link at the database or any sidecar refuses the start before the store
/// opens; the link's target is unchanged.
#[test]
fn refuses_linked_database_and_sidecars() {
    for f in names::DB_FILES {
        let dir = state();
        let elsewhere = tempfile::tempdir().unwrap();
        // A first run creates the directory; the link is placed before the second.
        let run = start(
            foreground(&dir.path, Some(free_port())),
            TestComponents::default(),
            Arc::default(),
        );
        run.serving();
        run.stop();
        run.join();
        for g in names::DB_FILES {
            let _ = std::fs::remove_file(dir.path.join(g));
        }
        chaldea_pty_runtime::fake::make_link(&dir.path.join(f), elsewhere.path()).unwrap();
        let run = start(
            foreground(&dir.path, Some(free_port())),
            TestComponents::default(),
            Arc::default(),
        );
        let r = run.join();
        assert_eq!(r.reason, Some(FailReason::StoreOpen), "{f}");
        assert!(r.lock == LockDisposition::Released, "{f}: {r:?}");
        if f != names::DB {
            assert!(!dir.path.join(names::DB).exists(), "{f}: the store opened");
        }
        assert!(
            std::fs::read_dir(elsewhere.path())
                .unwrap()
                .next()
                .is_none(),
            "{f}: the target changed"
        );
    }
}

/// PR8-CL-21: a failing read while verifying an existing token fails the start and never
/// replaces the file.
#[test]
fn token_store_error_does_not_reissue() {
    let dir = state();
    let run = start(
        foreground(&dir.path, Some(free_port())),
        TestComponents::default(),
        Arc::default(),
    );
    run.serving();
    run.stop();
    run.join();
    let before = std::fs::read(dir.path.join(names::OWNER_TOKEN)).unwrap();
    let faults = Arc::new(Faults::default());
    faults.token_read_fails.store(true, Ordering::SeqCst);
    let run = start(
        foreground(&dir.path, Some(free_port())),
        TestComponents::default(),
        faults,
    );
    let r = run.join();
    assert_eq!((r.code.code(), r.reason), (1, Some(FailReason::StoreOpen)));
    assert_eq!(
        std::fs::read(dir.path.join(names::OWNER_TOKEN)).unwrap(),
        before
    );
}

// ---- PR8-CL-22: ports ----

#[test]
fn range_exhaustion() {
    let dir = state();
    let faults = Arc::new(Faults::default());
    faults.bind_fails.store(true, Ordering::SeqCst);
    let run = start(
        foreground(&dir.path, None),
        TestComponents::default(),
        faults.clone(),
    );
    let log = run.log.clone();
    let r = run.join();
    assert_eq!(r.reason, Some(FailReason::PortUnavailable));
    let text = log.text();
    assert!(
        text.contains("no free port in 7373–7472 on 127.0.0.1"),
        "{text}"
    );
    assert!(text.contains("--port <n>"), "{text}");
    assert!(!dir.path.join(names::PORT).exists());
    assert_eq!((*PORT_RANGE.start(), *PORT_RANGE.end()), (7373, 7472));
    // Every one of the 100 ports was tried before the range was reported exhausted.
    assert_eq!(faults.bind_attempts.load(Ordering::SeqCst), 100);
}

#[test]
fn port_write_failure_before_rename() {
    let dir = state();
    let faults = Arc::new(Faults::default());
    faults.fail_replace(names::PORT, ReplaceStage::Write);
    let run = start(
        foreground(&dir.path, Some(free_port())),
        TestComponents::default(),
        faults,
    );
    let r = run.join();
    assert_eq!((r.code.code(), r.reason), (1, Some(FailReason::Internal)));
    assert!(!dir.path.join(names::PORT).exists());
    assert!(!dir.path.join(names::RECORD).exists());
}

/// A failed directory sync after the rename is a warning: the start succeeds and the next
/// start uses the new port.
#[test]
fn port_committed_after_rename() {
    let dir = state();
    let port = free_port();
    let faults = Arc::new(Faults::default());
    faults.fail_replace(names::PORT, ReplaceStage::DirSync);
    let run = start(
        foreground(&dir.path, Some(port)),
        TestComponents::default(),
        faults,
    );
    run.serving();
    assert!(run.log.text().contains("may not survive a power loss"));
    run.stop();
    assert_eq!(run.join().code.code(), 0);
    let run = start(
        foreground(&dir.path, None),
        TestComponents::default(),
        Arc::default(),
    );
    assert_eq!(run.serving(), port);
    run.stop();
    run.join();
}

/// Saved port A, `--port B`, the `serving` record fails after the port rename: the message
/// says B was saved; the next start without `--port` serves on B.
#[test]
fn record_failure_after_port_rename() {
    let dir = state();
    let (a, b) = (free_port(), free_port());
    let run = start(
        foreground(&dir.path, Some(a)),
        TestComponents::default(),
        Arc::default(),
    );
    run.serving();
    run.stop();
    run.join();
    let faults = Arc::new(Faults::default());
    let park = faults.park(Point::AfterPortRename);
    let run = start(
        foreground(&dir.path, Some(b)),
        TestComponents::default(),
        faults.clone(),
    );
    assert!(park.wait_reached(WAIT));
    faults.fail_replace(names::RECORD, ReplaceStage::Write);
    park.release();
    let log = run.log.clone();
    let r = run.join();
    assert_eq!(r.reason, Some(FailReason::Internal));
    let text = log.text();
    assert!(
        text.contains(&format!("port {b} was saved for")) && text.contains("re-pair"),
        "{text}"
    );
    let run = start(
        foreground(&dir.path, None),
        TestComponents::default(),
        Arc::default(),
    );
    assert_eq!(run.serving(), b);
    run.stop();
    run.join();
}

// ---- PR8-CL-25: failures and stops at step boundaries ----

/// What one failing step must produce.
type Setup = Box<dyn Fn(&Path, &Faults) -> TestComponents>;

struct Case {
    step: u8,
    reason: FailReason,
    store_existed: bool,
    setup: Setup,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            step: 6,
            reason: FailReason::PortFile,
            store_existed: false,
            setup: Box::new(|d, _| {
                chaldea_pty_runtime::host::create_private_dir(d).unwrap();
                std::fs::write(
                    d.join(names::MARKER),
                    super::super::state_dir::MARKER_CONTENT,
                )
                .unwrap();
                std::fs::write(d.join(names::PORT), "x\n").unwrap();
                TestComponents::default()
            }),
        },
        Case {
            step: 7,
            reason: FailReason::Internal,
            store_existed: false,
            setup: Box::new(|_, f| {
                f.fail_replace(names::RECORD, ReplaceStage::Write);
                TestComponents::default()
            }),
        },
        Case {
            step: 8,
            reason: FailReason::StoreOpen,
            store_existed: false,
            setup: Box::new(|_, f| {
                let sf = StoreFaults::default();
                sf.fail_open_after_init.store(true, Ordering::SeqCst);
                *f.store_faults.lock().unwrap() = Some(Arc::new(sf));
                TestComponents::default()
            }),
        },
        Case {
            step: 9,
            reason: FailReason::StoreOpen,
            store_existed: true,
            setup: Box::new(|_, _| TestComponents {
                extra: vec![Reconciler {
                    name: "refusing",
                    job: Box::new(|_| Err(JobError::Refused("injected".into()))),
                }],
                ..TestComponents::default()
            }),
        },
        Case {
            step: 10,
            reason: FailReason::PortUnavailable,
            store_existed: true,
            setup: Box::new(|_, f| {
                f.bind_fails.store(true, Ordering::SeqCst);
                TestComponents::default()
            }),
        },
        Case {
            step: 12,
            reason: FailReason::StoreOpen,
            store_existed: true,
            setup: Box::new(|d, f| {
                chaldea_pty_runtime::host::create_private_dir(d).unwrap();
                std::fs::write(
                    d.join(names::MARKER),
                    super::super::state_dir::MARKER_CONTENT,
                )
                .unwrap();
                let mut file =
                    chaldea_pty_runtime::host::create_private_file(&d.join(names::OWNER_TOKEN))
                        .unwrap();
                std::io::Write::write_all(
                    &mut file,
                    format!("chd_{}\n", "A".repeat(43)).as_bytes(),
                )
                .unwrap();
                f.token_read_fails.store(true, Ordering::SeqCst);
                TestComponents::default()
            }),
        },
        Case {
            step: 13,
            reason: FailReason::Internal,
            store_existed: true,
            setup: Box::new(|_, f| {
                f.fail_replace(names::PORT, ReplaceStage::Write);
                TestComponents::default()
            }),
        },
    ]
}

/// Each failing step: its reason, the store shut down when one existed, no record left, the
/// `port` file unchanged, and the verdict sent only after the lock decision.
#[test]
fn failure_at_each_step() {
    for case in cases() {
        let dir = state();
        let faults = Arc::new(Faults::default());
        let comps = (case.setup)(&dir.path, &faults);
        let port_before = std::fs::read(dir.path.join(names::PORT)).ok();
        let ready = RecordingReady::new(&dir.path);
        let run = start(
            detached(&dir.path, Some(free_port()), &ready),
            comps,
            faults,
        );
        let log = run.log.clone();
        let r = run.join();
        let step = case.step;
        assert_eq!(r.reason, Some(case.reason), "step {step}: {}", log.text());
        assert_eq!(r.code.code(), 1, "step {step}");
        assert_eq!(r.lock, LockDisposition::Released, "step {step}");
        // A detached daemon logs to `daemon.log` from step 6 until the lock decision.
        let text =
            log.text() + &std::fs::read_to_string(dir.path.join(names::LOG)).unwrap_or_default();
        assert_eq!(
            text.contains("store shutdown:"),
            case.store_existed,
            "step {step}: {text}"
        );
        assert!(
            !dir.path.join(names::RECORD).exists(),
            "step {step}: record left"
        );
        assert_eq!(
            std::fs::read(dir.path.join(names::PORT)).ok(),
            port_before,
            "step {step}: port changed"
        );
        let lines = ready.lines();
        assert_eq!(lines.len(), 1, "step {step}: {lines:?}");
        let v = &lines[0];
        assert!(
            v.line
                .starts_with(&format!("fail {} ", case.reason.as_str())),
            "step {step}: {v:?}"
        );
        assert_eq!(
            v.lock,
            Some(LockProbe::Free),
            "step {step}: verdict before the lock decision"
        );
        assert!(
            !v.record_present,
            "step {step}: verdict before the record was removed"
        );
        assert!(*ready.closed.lock().unwrap());
    }
}

/// Step 14: a readiness write error is ignored; the daemon serves.
#[test]
fn ready_write_error_is_ignored() {
    static CALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    struct Broken;
    impl super::ReadyWriter for Broken {
        fn write_line(&mut self, line: &str) -> std::io::Result<()> {
            assert!(line.starts_with("ready "), "{line}");
            CALLED.store(true, Ordering::SeqCst);
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn close(self: Box<Self>) {}
    }
    let dir = state();
    let port = free_port();
    let cfg = super::DaemonConfig {
        state_dir: dir.path.clone(),
        port: Some(port),
        mode: super::RunMode::DetachedChild {
            ready: Box::new(|| Ok(Box::new(Broken) as Box<dyn super::ReadyWriter>)),
            via: super::DetachVia::Setsid,
        },
        budgets: super::Budgets::DEFAULT,
    };
    let run = start(cfg, TestComponents::default(), Arc::default());
    let gate = run.gate();
    assert!(super::wait_for(WAIT, || get_status(
        port,
        "/api/threads",
        None
    )
    .is_ok()));
    // Step 14 called the writer (its error ignored) before the daemon served on.
    assert!(super::wait_for(WAIT, || CALLED.load(Ordering::SeqCst)));
    gate.request(super::StopCause::Signal(super::SignalName::Term));
    assert_eq!(run.join().code.code(), 0);
}

/// A stop at step 9 and one after step 14, in both modes, exit 0; a detached child's verdict
/// is `fail stopped` before readiness and `ready` after it.
#[test]
fn stop_before_and_after_ready() {
    for is_detached in [false, true] {
        for before in [true, false] {
            let dir = state();
            let port = free_port();
            let ready = RecordingReady::new(&dir.path);
            let cfg = if is_detached {
                detached(&dir.path, Some(port), &ready)
            } else {
                foreground(&dir.path, Some(port))
            };
            let faults = Arc::new(Faults::default());
            let park = before.then(|| faults.park(Point::Before(9)));
            let run = start(cfg, TestComponents::default(), faults);
            if let Some(p) = &park {
                assert!(p.wait_reached(WAIT));
                run.stop();
                p.release();
            } else {
                if is_detached {
                    assert!(super::wait_for(WAIT, || !ready.lines().is_empty()));
                } else {
                    run.serving();
                }
                run.stop();
            }
            let r = run.join();
            let what = format!("detached={is_detached} before={before}");
            assert_eq!(r.code.code(), 0, "{what}: {r:?}");
            assert_eq!(r.reason, None, "{what}");
            assert_eq!(r.lock, LockDisposition::Released, "{what}");
            if is_detached {
                let lines = ready.lines();
                assert_eq!(lines.len(), 1, "{what}: {lines:?}");
                if before {
                    assert!(
                        lines[0].line.starts_with("fail stopped "),
                        "{what}: {lines:?}"
                    );
                } else {
                    assert_eq!(
                        lines[0].line,
                        format!("ready {port} {}\n", std::process::id()),
                        "{what}"
                    );
                }
            }
        }
    }
}

/// A stop while step 4 is parked: exit 0, nothing created, verdict `fail stopped`.
#[test]
fn stop_before_the_lock() {
    let dir = state();
    let ready = RecordingReady::new(&dir.path);
    let faults = Arc::new(Faults::default());
    let park = faults.park(Point::Before(4));
    let run = start(
        detached(&dir.path, Some(free_port()), &ready),
        TestComponents::default(),
        faults,
    );
    assert!(park.wait_reached(WAIT));
    run.stop();
    park.release();
    let r = run.join();
    assert_eq!(
        (r.code.code(), r.reason, r.lock),
        (0, None, LockDisposition::NeverTaken)
    );
    assert!(!dir.path.exists(), "the state directory was created");
    let lines = ready.lines();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].line.starts_with("fail stopped "), "{lines:?}");
}

/// Saved port A, `--port B`, a stop between the port rename and step 14: exit 0, the verdict
/// `fail stopped port_saved=B` with the notice, and the next start serves on B.
#[test]
fn stop_after_port_rename() {
    let dir = state();
    let (a, b) = (free_port(), free_port());
    let run = start(
        foreground(&dir.path, Some(a)),
        TestComponents::default(),
        Arc::default(),
    );
    run.serving();
    run.stop();
    run.join();
    let ready = RecordingReady::new(&dir.path);
    let faults = Arc::new(Faults::default());
    let park = faults.park(Point::AfterPortRename);
    let run = start(
        detached(&dir.path, Some(b), &ready),
        TestComponents::default(),
        faults,
    );
    assert!(park.wait_reached(WAIT));
    run.stop();
    park.release();
    let r = run.join();
    assert_eq!((r.code.code(), r.reason), (0, None));
    let lines = ready.lines();
    assert_eq!(lines.len(), 1);
    let l = &lines[0].line;
    assert!(
        l.starts_with(&format!("fail stopped port_saved={b} ")) && l.contains("re-pair"),
        "{l}"
    );
    let run = start(
        foreground(&dir.path, None),
        TestComponents::default(),
        Arc::default(),
    );
    assert_eq!(run.serving(), b);
    run.stop();
    run.join();
}

/// A private directory with a complete marker and nothing else.
pub(super) fn marked(dir: &Path) {
    chaldea_pty_runtime::host::create_private_dir(dir).unwrap();
    let mut f = chaldea_pty_runtime::host::create_private_file(&dir.join(names::MARKER)).unwrap();
    std::io::Write::write_all(&mut f, super::super::state_dir::MARKER_CONTENT).unwrap();
}

/// PR8-CL-2: a database or sidecar with a second hard link (here from another directory) is
/// refused before the store opens it; nothing of the second state directory is opened.
#[test]
fn refuses_hard_linked_database_files() {
    let a = state();
    let run = start(
        foreground(&a.path, Some(free_port())),
        TestComponents::default(),
        Arc::default(),
    );
    run.serving();
    run.stop();
    assert_eq!(run.join().code.code(), 0);
    for name in [names::DB, "chaldea.db-wal"] {
        let b = state();
        marked(&b.path);
        let source = a.path.join(format!("{name}.source"));
        if name == names::DB {
            std::fs::hard_link(a.path.join(names::DB), b.path.join(name)).unwrap();
        } else {
            std::fs::write(&source, b"").unwrap();
            std::fs::hard_link(&source, b.path.join(name)).unwrap();
        }
        let run = start(
            foreground(&b.path, Some(free_port())),
            TestComponents::default(),
            Arc::default(),
        );
        let log = run.log.clone();
        let r = run.join();
        assert_eq!(
            (r.code.code(), r.reason),
            (1, Some(FailReason::StoreOpen)),
            "{name}: {}",
            log.text()
        );
        assert!(log.text().contains("hard link"), "{name}: {}", log.text());
        for other in names::DB_FILES {
            if other != name {
                assert!(!b.path.join(other).exists(), "{name}: {other} was created");
            }
        }
    }
}

/// PR8-CL-6: a receipt left `in_progress` is `unknown_after_crash` for the first request the
/// next daemon serves.
#[test]
fn first_request_reads_the_reconciled_receipt() {
    async fn read_receipt(
        ctx: crate::http::ReadCtx,
        _req: Request<Body>,
    ) -> Result<Response, ApiError> {
        let status = ctx
            .read(|tx, _auth| {
                tx.receipt(CMD)
                    .map_err(|_| ApiError::internal())
                    .map(|r| r.map(|r| format!("{:?}", r.status)))
            })
            .await?;
        Ok((StatusCode::OK, status.unwrap_or_default()).into_response())
    }
    let dir = state();
    let run = start(
        foreground(&dir.path, Some(free_port())),
        TestComponents::default(),
        Arc::default(),
    );
    run.serving();
    run.stop();
    run.join();
    let store = open_store(&dir.path);
    super::settle(
        store
            .submit(Instant::now() + WAIT, |tx| {
                let t = chaldea_core::ids::TokenId::from_u128(0x6a);
                Ok(tx.receipt_begin(CMD, t, [7; 32])?)
            })
            .unwrap(),
    )
    .unwrap();
    close(&store);
    let comps = TestComponents {
        mounts: Some(Mounts::mvp().with(Mount::read(
            spec(&Method::GET, "/api/threads").unwrap(),
            read_receipt,
        ))),
        ..TestComponents::default()
    };
    let run = start(
        foreground(&dir.path, Some(free_port())),
        comps,
        Arc::default(),
    );
    let port = run.serving();
    let (code, body) = request(
        port,
        "GET",
        "/api/threads",
        Some(&token_of(&dir.path)),
        None,
    )
    .unwrap();
    assert_eq!(code, 200, "{body}");
    assert!(body.ends_with("UnknownAfterCrash"), "{body}");
    run.stop();
    run.join();
}

/// D12: a start that moved the saved port A to `--port B` says the pairing origin changed.
#[test]
fn port_override_says_to_re_pair() {
    let dir = state();
    let (a, b) = (free_port(), free_port());
    let run = start(
        foreground(&dir.path, Some(a)),
        TestComponents::default(),
        Arc::default(),
    );
    run.serving();
    run.stop();
    run.join();
    let run = start(
        foreground(&dir.path, Some(b)),
        TestComponents::default(),
        Arc::default(),
    );
    assert_eq!(run.serving(), b);
    let text = run.log.text();
    assert!(
        text.contains(&format!("port {b} replaces the saved port {a}"))
            && text.contains("chaldea open"),
        "{text}"
    );
    run.stop();
    run.join();
}

/// The lifecycle refuses `port: Some(0)` (the CLI's parser already does): nothing binds an
/// ephemeral port or saves `0`.
#[test]
fn port_zero_is_refused() {
    let dir = state();
    let run = start(
        foreground(&dir.path, Some(0)),
        TestComponents::default(),
        Arc::default(),
    );
    let log = run.log.clone();
    let r = run.join();
    assert_eq!(
        (r.code.code(), r.reason),
        (1, Some(FailReason::PortUnavailable)),
        "{}",
        log.text()
    );
    assert!(log.text().contains("port 0"), "{}", log.text());
    assert!(!dir.path.join(names::PORT).exists());
}

/// The job state a kill-on-close job without breakaway reports.
fn in_a_job() -> chaldea_pty_runtime::host::JobState {
    chaldea_pty_runtime::host::JobState {
        in_job: true,
        immediate: Some(chaldea_pty_runtime::host::JobLimits {
            kill_on_close: true,
            breakaway_ok: false,
            silent_breakaway_ok: false,
        }),
    }
}

/// PR8-CL-12 (step 2, the child decides): a detached child in any job fails with `parent_job`
/// before the state directory, the lock, or anything else; its verdict says so.
#[test]
fn detached_child_in_a_job_fails_parent_job() {
    let dir = state();
    let ready = RecordingReady::new(&dir.path);
    let faults = Arc::new(Faults::default());
    *faults.job_state.lock().unwrap() = Some(in_a_job());
    let r = start(
        detached(&dir.path, None, &ready),
        TestComponents::default(),
        faults,
    )
    .join();
    assert_eq!(
        (r.code.code(), r.reason, r.lock),
        (1, Some(FailReason::ParentJob), LockDisposition::NeverTaken)
    );
    assert!(!dir.path.exists(), "the state directory was created");
    let lines = ready.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].line.starts_with("fail parent_job "), "{lines:?}");
    assert!(lines[0].line.contains("kill_on_close=true"), "{lines:?}");
}

/// PR8-CL-12: a foreground daemon in a job warns, records the job in `daemon.json`'s
/// evidence, and serves.
#[test]
fn foreground_in_a_job_warns_and_serves() {
    let dir = state();
    let faults = Arc::new(Faults::default());
    *faults.job_state.lock().unwrap() = Some(in_a_job());
    let run = start(
        foreground(&dir.path, None),
        TestComponents::default(),
        faults,
    );
    run.serving();
    assert!(
        run.log.text().contains("runs inside a job"),
        "{}",
        run.log.text()
    );
    let record = crate::lifecycle::read_record(&dir.path).unwrap().unwrap();
    let job = record.evidence.job.expect("job evidence");
    assert_eq!(job["in_job"], true, "{job}");
    assert_eq!(job["immediate"]["kill_on_close"], true, "{job}");
    run.stop();
    assert_eq!(run.join().code.code(), 0);
}
