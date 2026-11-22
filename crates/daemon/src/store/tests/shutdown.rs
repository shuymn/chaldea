//! Concurrent and gated shutdowns: one intake transition, readers closed before completion,
//! each caller bounded by its own deadline, no join of a running writer (PR6-CL-22, CL-24).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::helpers::{Gates, store_with};
use super::support::{registered, soon, thread};
use crate::store::{ReadError, ShutdownReport, Store, SubmitError};

const WAIT: Duration = Duration::from_secs(10);

fn in_thread(s: &Store, deadline: Duration) -> mpsc::Receiver<ShutdownReport> {
    let (tx, rx) = mpsc::channel();
    let s = s.clone();
    std::thread::spawn(move || {
        let _ = tx.send(s.shutdown(Instant::now() + deadline));
    });
    rx
}

/// Shutdown A holds the idle readers it took and has not closed them yet; shutdown B must
/// not report the readers returned (their handles are still open).
#[test]
fn concurrent_shutdown_waits_for_readers_another_caller_holds() {
    let gates = Gates::default();
    let (_dir, s, _) = store_with(gates.faults());
    let (reached, release) = gates.arm("drain_taken");
    let a = in_thread(&s, Duration::from_secs(30));
    reached.recv_timeout(WAIT).expect("A took the idle readers");
    let b = in_thread(&s, Duration::from_secs(1))
        .recv_timeout(WAIT)
        .expect("B returns by its deadline");
    drop(release);
    let a = a.recv_timeout(Duration::from_secs(40)).expect("A returns");
    assert!(!b.readers_returned, "{b:?}");
    assert!(b.readers_abandoned > 0, "{b:?}");
    assert!(a.readers_returned && a.writer_exited, "{a:?}");
}

/// When `submit` reports `Closed`, `read` does too: intake closes in one transition.
#[test]
fn intake_closes_for_submit_and_read_together() {
    let gates = Gates::default();
    let (_dir, s, _) = store_with(gates.faults());
    let (reached, release) = gates.arm("intake_closed");
    let a = in_thread(&s, Duration::from_secs(30));
    reached.recv_timeout(WAIT).expect("shutdown closed intake");
    let submitted = s.submit(soon(), |_| Ok(())).err();
    let read = s.read(soon(), |_| Ok(()));
    drop(release);
    a.recv_timeout(Duration::from_secs(40))
        .expect("shutdown returns");
    assert!(
        matches!(submitted, Some(SubmitError::Closed)),
        "{submitted:?}"
    );
    assert!(matches!(read, Err(ReadError::Closed)), "{read:?}");
}

/// A caller with a shorter deadline returns by it while another caller waits for a writer
/// blocked in a job.
#[test]
fn shorter_shutdown_returns_by_its_deadline() {
    let gates = Gates::default();
    let (_dir, s, _) = store_with(gates.faults());
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let job = s
        .submit(soon(), move |_| {
            started_tx.send(()).unwrap();
            go_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            Ok(())
        })
        .unwrap();
    started_rx.recv_timeout(WAIT).unwrap();
    let (reached, release) = gates.arm("readers_closed");
    let a = in_thread(&s, Duration::from_secs(30));
    reached.recv_timeout(WAIT).expect("A closed the readers");
    let b = in_thread(&s, Duration::from_secs(1))
        .recv_timeout(Duration::from_secs(5))
        .expect("B returns by its deadline");
    assert!(!b.writer_exited, "{b:?}");
    drop(release);
    go_tx.send(()).unwrap();
    job.blocking_wait().unwrap();
    let a = a.recv_timeout(Duration::from_secs(40)).expect("A returns");
    assert!(a.writer_exited, "{a:?}");
}

/// The writer published its exit but its thread has not returned: shutdown reports the
/// exit without joining the thread, so it returns by its deadline.
#[test]
fn shutdown_does_not_join_a_running_writer() {
    let gates = Gates::default();
    let (_dir, s, _) = store_with(gates.faults());
    s.submit(soon(), |tx| {
        Ok(tx.append(vec![registered(thread(1), false)])?)
    })
    .unwrap()
    .blocking_wait()
    .unwrap();
    let (_reached, release) = gates.arm("exit_published");
    let r = in_thread(&s, Duration::from_secs(2))
        .recv_timeout(Duration::from_secs(8))
        .expect("shutdown returns by its deadline");
    drop(release);
    assert!(r.writer_exited, "{r:?}");
}
