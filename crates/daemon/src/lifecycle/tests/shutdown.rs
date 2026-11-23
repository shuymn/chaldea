//! The coordinator (design §2.8) in process: PR8-CL-4 (a clean stop releases), CL-5 (order and
//! ends), CL-10 (console end), CL-23 (only our record), CL-26 (isolated runs register nothing).

use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::super::faults::Faults;
use super::super::files::{LockProbe, probe};
use super::super::gate::{ConsoleEvent, SignalName, StopCause, any_registered};
use super::super::{FailReason, LockDisposition, names};
use super::{Recorder, TestComponents, WAIT, foreground, free_port, start, state};
use crate::store::test_faults::Faults as StoreFaults;

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

fn store_shutdown_called(sf: &StoreFaults) -> bool {
    sf.marks.lock().unwrap().contains(&"owner_claimed")
}

/// PR8-CL-10: a console end while serving, and one while HTTP drains after a signal, returns
/// within 200 ms holding the lock, without calling `Store::shutdown`.
#[test]
fn console_end_exits_without_store_shutdown() {
    // While serving.
    let dir = state();
    let sf = Arc::new(StoreFaults::default());
    let faults = Arc::new(Faults::default());
    *faults.store_faults.lock().unwrap() = Some(sf.clone());
    let run = start(
        foreground(&dir.path, Some(free_port())),
        TestComponents::default(),
        faults,
    );
    run.serving();
    let gate = run.gate();
    let t0 = Instant::now();
    gate.crash_exit(ConsoleEvent::Close);
    let r = run.join();
    assert!(
        t0.elapsed() < Duration::from_millis(200),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(
        (r.code.code(), r.reason, r.lock),
        (
            1,
            Some(FailReason::ConsoleEnd),
            LockDisposition::HeldUntilExit
        )
    );
    assert!(!store_shutdown_called(&sf));

    // After a signal, while HTTP drains a connection that sent half a head.
    let dir = state();
    let sf = Arc::new(StoreFaults::default());
    let faults = Arc::new(Faults::default());
    *faults.store_faults.lock().unwrap() = Some(sf.clone());
    let run = start(
        foreground(&dir.path, Some(free_port())),
        TestComponents::default(),
        faults.clone(),
    );
    let port = run.serving();
    let mut half = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    std::io::Write::write_all(&mut half, b"GET /api/threads HTTP/1.1\r\n").unwrap();
    let gate = run.gate();
    std::thread::sleep(Duration::from_millis(100));
    gate.request(StopCause::Signal(SignalName::Term));
    assert!(super::wait_for(WAIT, || faults
        .marks()
        .iter()
        .any(|m| m.0 == "http")));
    std::thread::sleep(Duration::from_millis(100));
    let t0 = Instant::now();
    gate.crash_exit(ConsoleEvent::Close);
    let r = run.join();
    assert!(
        t0.elapsed() < Duration::from_millis(200),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(
        (r.reason, r.lock),
        (Some(FailReason::ConsoleEnd), LockDisposition::HeldUntilExit)
    );
    assert!(!store_shutdown_called(&sf));
    drop(half);
}

/// PR8-CL-10: a console end while `Store::shutdown` blocks on a held reader returns within
/// 200 ms instead of waiting for `C + 250 ms`; the lock is not released.
#[test]
fn console_end_during_store_shutdown() {
    let dir = state();
    let faults = Arc::new(Faults::default());
    let run = start(
        foreground(&dir.path, Some(free_port())),
        TestComponents::default(),
        faults.clone(),
    );
    run.serving();
    let store = run.store();
    let (hold_tx, hold_rx) = mpsc::channel::<()>();
    let (in_tx, in_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        store
            .read(Instant::now() + WAIT, move |_tx| {
                in_tx.send(()).unwrap();
                let _ = hold_rx.recv_timeout(WAIT);
                Ok(())
            })
            .ok();
    });
    in_rx.recv_timeout(WAIT).unwrap();
    let gate = run.gate();
    gate.request(StopCause::Signal(SignalName::Term));
    assert!(super::wait_for(WAIT, || faults
        .marks()
        .iter()
        .any(|m| m.0 == "store")));
    std::thread::sleep(Duration::from_millis(200));
    let t0 = Instant::now();
    gate.crash_exit(ConsoleEvent::Close);
    let r = run.join();
    assert!(
        t0.elapsed() < Duration::from_millis(200),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(
        (r.reason, r.lock),
        (Some(FailReason::ConsoleEnd), LockDisposition::HeldUntilExit)
    );
    assert_eq!(probe(&dir.path).unwrap(), LockProbe::Held);
    hold_tx.send(()).unwrap();
    reader.join().unwrap();
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
