//! Gated shutdowns: one intake transition by the owning call, the checkpoint only after the
//! owner finished with the readers, later calls bounded by their own deadlines, and a
//! reported writer exit with closed notifications (PR6-CL-22, CL-24).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::helpers::{Gates, store_with};
use super::support::soon;
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

/// While the owning shutdown still holds the readers it took, a later caller with an
/// expired deadline cannot start the writer's checkpoint.
#[test]
fn checkpoint_waits_for_the_owner_to_finish_with_the_readers() {
    let gates = Gates::default();
    let (_dir, s, _) = store_with(gates.faults());
    let (taken, release_readers) = gates.arm("drain_taken");
    let (checkpoint, release_checkpoint) = gates.arm("checkpoint_begin");
    let a = in_thread(&s, Duration::from_secs(30));
    taken
        .recv_timeout(WAIT)
        .expect("the owner took the idle readers");
    let b = in_thread(&s, Duration::ZERO)
        .recv_timeout(WAIT)
        .expect("a later caller returns by its deadline");
    assert!(!b.readers_returned && !b.writer_exited, "{b:?}");
    // A bounded negative check: the checkpoint must not begin while the owner holds the
    // readers.
    assert!(
        checkpoint.recv_timeout(Duration::from_secs(1)).is_err(),
        "the checkpoint began while the owner held the readers"
    );
    drop(release_readers);
    checkpoint
        .recv_timeout(WAIT)
        .expect("the checkpoint begins after the owner closed the readers");
    drop(release_checkpoint);
    let a = a
        .recv_timeout(Duration::from_secs(40))
        .expect("the owner returns");
    assert!(a.readers_returned && a.writer_exited, "{a:?}");
}

/// A report with `writer_exited` means the writer thread finished, its notification sender
/// included: a subscriber sees `Closed`. Held at the thread's last step, the owner returns
/// by its deadline without the exit; once the thread ends, a later call reports it.
#[test]
fn reported_writer_exit_has_closed_notifications() {
    use tokio::sync::broadcast::error::TryRecvError;

    let gates = Gates::default();
    let (_dir, s, _) = store_with(gates.faults());
    let mut notifications = s.subscribe();
    let (reached, release) = gates.arm("writer_exiting");
    let result = in_thread(&s, Duration::from_secs(2));
    reached.recv_timeout(WAIT).unwrap();

    let report = result.recv_timeout(WAIT).unwrap();
    let notification = notifications.try_recv();
    drop(release);

    assert!(
        !report.writer_exited || matches!(notification, Err(TryRecvError::Closed)),
        "{report:?}, notification {notification:?}"
    );
    let later = s.shutdown(Instant::now() + WAIT);
    assert!(later.writer_exited, "{later:?}");
    assert!(matches!(
        notifications.try_recv(),
        Err(TryRecvError::Closed)
    ));
}
