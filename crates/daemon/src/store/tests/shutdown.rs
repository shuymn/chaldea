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

/// Sets the fault from inside a running job, so the writer panics right after it, outside
/// any job, and publishes its exit on its own.
fn end_writer_on_its_own(s: &Store) {
    let faults = s.inner.faults.clone();
    s.submit(soon(), move |_| {
        faults
            .panic_outside_job
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    })
    .unwrap()
    .blocking_wait()
    .unwrap();
}

/// Waits (bounded) until the writer has published its exit.
fn wait_for_exit(s: &Store) {
    let deadline = Instant::now() + WAIT;
    while crate::store::lock(&s.inner.shared.stop).exit.is_none() {
        assert!(
            Instant::now() < deadline,
            "the writer never published its exit"
        );
        std::thread::yield_now();
    }
}

/// When `submit` reports `Closed`, `read` does too: intake closes in one transition.
#[test]
fn intake_closes_for_submit_and_read_together() {
    let gates = Gates::default();
    let (_dir, s) = store_with(gates.faults());
    let (reached, release) = gates.arm("intake_transition");
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
    let (_dir, s) = store_with(gates.faults());
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
    let (_dir, s) = store_with(gates.faults());
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
    let (_dir, s) = store_with(gates.faults());
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

/// The writer ended on its own (a panic outside a job) while the owner is between closing
/// intake and finishing: a later call that reports the exit finds notifications closed.
#[test]
fn later_report_of_an_early_writer_exit_has_closed_notifications() {
    use tokio::sync::broadcast::error::TryRecvError;

    let gates = Gates::default();
    let (_dir, s) = store_with(gates.faults());
    let mut notifications = s.subscribe();
    end_writer_on_its_own(&s);
    let (reached, release) = gates.arm("intake_transition");
    let a = in_thread(&s, Duration::from_secs(30));
    reached.recv_timeout(WAIT).expect("the owner closed intake");
    let b = in_thread(&s, Duration::from_secs(2))
        .recv_timeout(WAIT)
        .expect("a later call returns by its deadline");
    let notification = notifications.try_recv();
    drop(release);
    a.recv_timeout(Duration::from_secs(40))
        .expect("the owner returns");
    assert!(b.writer_exited, "{b:?}");
    assert!(
        matches!(notification, Err(TryRecvError::Closed)),
        "{notification:?}"
    );
}

/// The owner returns before the writer thread ends: it still gives up the thread's handle
/// (detaching it), so no store clone keeps it.
#[test]
fn owner_detaches_a_writer_still_running() {
    let gates = Gates::default();
    let (_dir, s) = store_with(gates.faults());
    let (reached, release) = gates.arm("writer_exiting");
    let a = in_thread(&s, Duration::from_millis(500))
        .recv_timeout(WAIT)
        .expect("the owner returns by its deadline");
    reached
        .recv_timeout(WAIT)
        .expect("the writer reached its last step");
    let kept = s.inner.writer.lock().unwrap().is_some();
    drop(release);
    assert!(!a.writer_exited, "{a:?}");
    assert!(!kept, "the owner left the writer's handle in the store");
    let later = s.shutdown(Instant::now() + WAIT);
    assert!(later.writer_exited, "{later:?}");
}

/// A later call that timed out reports only what was published by its deadline, even when
/// the writer exits before it reads the state.
#[test]
fn later_call_reports_only_progress_by_its_deadline() {
    let gates = Gates::default();
    let (_dir, s) = store_with(gates.faults());
    let (exiting, release_writer) = gates.arm("writer_exiting");
    let a = in_thread(&s, Duration::from_millis(300))
        .recv_timeout(WAIT)
        .expect("the owner returns by its deadline");
    assert!(!a.writer_exited, "{a:?}");
    exiting
        .recv_timeout(WAIT)
        .expect("the writer is held at its last step");
    let (waited, release_b) = gates.arm("observe_waited");
    let b = in_thread(&s, Duration::from_millis(300));
    waited.recv_timeout(WAIT).expect("the later call timed out");
    drop(release_writer);
    wait_for_exit(&s);
    drop(release_b);
    let b = b.recv_timeout(WAIT).expect("the later call returns");
    assert!(!b.writer_exited, "{b:?}");
    assert_eq!(b.checkpoint, crate::store::Checkpoint::NotRun, "{b:?}");
}

/// The owner closes notifications outside the intake locks: a receiver's waker, run inline
/// when the last notification sender drops, can take the queue-sender lock (as `submit`
/// does) without waiting for the owner.
#[test]
fn closing_notifications_leaves_the_intake_locks_free() {
    use std::future::Future;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Wake, Waker};

    use crate::store::Shared;

    struct Probe {
        shared: Arc<Shared>,
        result: Mutex<mpsc::Sender<bool>>,
    }
    impl Wake for Probe {
        fn wake(self: Arc<Self>) {
            let free = self.shared.sender.try_lock().is_ok();
            let _ = self.result.lock().unwrap().send(free);
        }
    }

    let gates = Gates::default();
    let (_dir, s) = store_with(gates.faults());
    let mut notifications = s.subscribe();
    let (result_tx, result_rx) = mpsc::channel();
    let waker = Waker::from(Arc::new(Probe {
        shared: s.inner.shared.clone(),
        result: Mutex::new(result_tx),
    }));
    let mut recv = Box::pin(notifications.recv());
    assert!(
        recv.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    // The writer ends on its own and drops its notification sender, so the owner's drop of
    // the store's sender is the last one and wakes the receiver inline.
    let (exiting, release) = gates.arm("writer_exiting");
    end_writer_on_its_own(&s);
    exiting
        .recv_timeout(WAIT)
        .expect("the writer dropped its sender");
    drop(release);
    let report = in_thread(&s, Duration::from_secs(5))
        .recv_timeout(WAIT)
        .expect("the owner returns");
    let free = result_rx
        .recv_timeout(WAIT)
        .expect("the receiver was woken");
    assert!(free, "the waker found the queue-sender lock held");
    assert!(report.writer_exited, "{report:?}");
    assert!(matches!(
        recv.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Ready(Err(_))
    ));
}

/// An owner whose exit wait timed out detaches the writer's handle even once the thread's
/// main function has returned: it does not wait in `join` for thread-local destructors.
#[test]
fn timed_out_owner_detaches_a_finished_writer() {
    use std::cell::RefCell;

    struct Held {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Drop for Held {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            let _ = self.release.recv_timeout(Duration::from_secs(30));
        }
    }
    thread_local! {
        static HELD: RefCell<Option<Held>> = const { RefCell::new(None) };
    }

    let gates = Gates::default();
    let (_dir, s) = store_with(gates.faults());
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_dtor, release_rx) = mpsc::channel();
    s.submit(soon(), move |_| {
        HELD.with(|h| {
            *h.borrow_mut() = Some(Held {
                entered: entered_tx,
                release: release_rx,
            });
        });
        Ok(())
    })
    .unwrap()
    .blocking_wait()
    .unwrap();
    let (exiting, release_writer) = gates.arm("writer_exiting");
    let (waited, release_owner) = gates.arm("owner_waited");
    let a = in_thread(&s, Duration::from_millis(300));
    exiting
        .recv_timeout(WAIT)
        .expect("the writer is held at its last step");
    waited
        .recv_timeout(WAIT)
        .expect("the owner's exit wait timed out");
    drop(release_writer);
    entered_rx
        .recv_timeout(WAIT)
        .expect("the writer runs its thread-local destructor");
    let deadline = Instant::now() + WAIT;
    while !s
        .inner
        .writer
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(std::thread::JoinHandle::is_finished)
    {
        assert!(
            Instant::now() < deadline,
            "the writer's main never returned"
        );
        std::thread::yield_now();
    }
    drop(release_owner);
    let a = a.recv_timeout(Duration::from_secs(5));
    drop(release_dtor);
    let a = a.expect("the owner returns without joining the writer");
    assert!(!a.writer_exited, "{a:?}");
}

/// A later call that reaches the shared state only after its deadline, by when both reports
/// have been published, still leaves out what was published after its deadline.
#[test]
fn later_call_past_its_deadline_leaves_out_late_reports() {
    let gates = Gates::default();
    let (_dir, s) = store_with(gates.faults());
    let (exiting, release_writer) = gates.arm("writer_exiting");
    let a = in_thread(&s, Duration::from_millis(300))
        .recv_timeout(WAIT)
        .expect("the owner returns by its deadline");
    assert!(!a.writer_exited, "{a:?}");
    exiting
        .recv_timeout(WAIT)
        .expect("the writer is held at its last step");
    // The later call's deadline has passed before the writer publishes its exit.
    let (begun, release_b) = gates.arm("observe_begin");
    let b = in_thread(&s, Duration::ZERO);
    begun
        .recv_timeout(WAIT)
        .expect("the later call is about to observe");
    drop(release_writer);
    wait_for_exit(&s);
    drop(release_b);
    let b = b.recv_timeout(WAIT).expect("the later call returns");
    assert!(!b.writer_exited, "{b:?}");
    assert_eq!(b.checkpoint, crate::store::Checkpoint::NotRun, "{b:?}");
}

/// The writer ended on its own while the owner has claimed the shutdown but not yet closed
/// notifications: a later call does not report the exit, since a receiver is still open.
#[test]
fn later_call_waits_for_the_notification_close_to_report_the_exit() {
    use tokio::sync::broadcast::error::TryRecvError;

    let gates = Gates::default();
    let (_dir, s) = store_with(gates.faults());
    let mut notifications = s.subscribe();
    end_writer_on_its_own(&s);
    wait_for_exit(&s);
    let (claimed, release) = gates.arm("owner_claimed");
    let a = in_thread(&s, Duration::from_secs(30));
    claimed
        .recv_timeout(WAIT)
        .expect("the owner claimed shutdown");
    let b = in_thread(&s, Duration::from_millis(300))
        .recv_timeout(WAIT)
        .expect("a later call returns by its deadline");
    let notification = notifications.try_recv();
    drop(release);
    let a = a.recv_timeout(WAIT).expect("the owner returns");
    assert!(
        !b.writer_exited || matches!(notification, Err(TryRecvError::Closed)),
        "{b:?}, notification {notification:?}"
    );
    assert!(a.writer_exited, "{a:?}");
}
