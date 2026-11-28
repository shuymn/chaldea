//! The writer's deadlines, lock, panics, and queue (PR6-CL-14, CL-15).

mod support;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use chaldea_core::ids::{IdempotencyKey, StreamId};
use chaldea_daemon::store::{Store, SubmitError, WriteError};
use support::{FixedClock, T0, TestStore, config, registered, soon, thread};

const WAIT: Duration = Duration::from_secs(30);

fn host_head(s: &TestStore) -> u64 {
    s.read(|r| Ok(r.head(StreamId::Host, 0)?.head_seq))
}

/// A store with its own busy budget and queue size.
fn store_with(busy: Duration, queue: usize) -> TestStore {
    let dir = tempfile::tempdir().unwrap();
    let clock = FixedClock::new(T0);
    let mut cfg = config(dir.path().join("chaldea.db"), clock.clone());
    cfg.busy_timeout = busy;
    cfg.writer_queue = queue;
    let store = Store::open(cfg).unwrap();
    TestStore {
        store: Some(store),
        clock,
        dir,
    }
}

/// Blocks the writer with a job until the returned sender is used (or `WAIT` passes); returns
/// once the job is running.
fn block_writer(s: &TestStore) -> (mpsc::Sender<()>, chaldea_daemon::store::WriteTicket<()>) {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let ticket = s
        .store()
        .submit(soon(), move |_| {
            started_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(WAIT);
            Ok(())
        })
        .unwrap();
    started_rx
        .recv_timeout(WAIT)
        .expect("the blocking job started");
    (release_tx, ticket)
}

/// Holds the write lock from another connection.
fn hold_lock(s: &TestStore) -> rusqlite::Connection {
    let raw = rusqlite::Connection::open(s.path()).unwrap();
    raw.execute_batch("BEGIN IMMEDIATE").unwrap();
    raw
}

/// A job whose deadline passes while it waits in the queue never runs.
#[test]
fn expired_in_queue_changes_nothing() {
    let s = TestStore::new();
    let (release, blocker) = block_writer(&s);
    let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran2 = ran.clone();
    let e = registered(thread(1), false);
    let ticket = s
        .store()
        .submit(Instant::now() + Duration::from_millis(200), move |tx| {
            ran2.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(tx.append(vec![e])?)
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(600));
    release.send(()).unwrap();
    blocker.blocking_wait().unwrap();
    assert!(matches!(ticket.blocking_wait(), Err(WriteError::Expired)));
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(host_head(&s), 0);
}

/// A deadline that passes while the job waits for the lock it then obtains: `Expired`.
#[test]
fn expired_while_waiting_for_the_lock() {
    let s = store_with(Duration::from_secs(10), 16);
    let raw = hold_lock(&s);
    let e = registered(thread(1), false);
    let ticket = s
        .store()
        .submit(Instant::now() + Duration::from_millis(300), move |tx| {
            Ok(tx.append(vec![e])?)
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    raw.execute_batch("COMMIT").unwrap();
    let r = ticket.blocking_wait();
    assert!(matches!(r, Err(WriteError::Expired)), "{r:?}");
    assert_eq!(host_head(&s), 0);
}

/// The lock not obtained within the busy budget is `Busy`, even though the deadline also passed.
#[test]
fn busy_wins_over_the_deadline() {
    let s = store_with(Duration::from_secs(1), 16);
    let raw = hold_lock(&s);
    let e = registered(thread(1), false);
    let ticket = s
        .store()
        .submit(Instant::now() + Duration::from_millis(200), move |tx| {
            Ok(tx.append(vec![e])?)
        })
        .unwrap();
    let r = ticket.blocking_wait();
    raw.execute_batch("COMMIT").unwrap();
    assert!(matches!(r, Err(WriteError::Busy)), "{r:?}");
    assert_eq!(host_head(&s), 0);
}

/// Another connection holding the lock past the busy budget: `Busy`, nothing written, and the
/// next job succeeds once it is released.
#[test]
fn busy_from_another_connection() {
    let s = store_with(Duration::from_secs(1), 16);
    let raw = hold_lock(&s);
    let r = s.append(vec![registered(thread(1), false)]);
    assert!(matches!(r, Err(WriteError::Busy)), "{r:?}");
    raw.execute_batch("COMMIT").unwrap();
    drop(raw);
    assert_eq!(host_head(&s), 0);
    s.append(vec![registered(thread(1), false)]).unwrap();
    assert_eq!(host_head(&s), 1);
}

/// A ticket resolves `Ok` only after `COMMIT`: a new reader sees the event at once.
#[test]
fn ok_is_visible_to_a_new_reader() {
    let s = TestStore::new();
    let t = thread(1);
    s.append(vec![registered(t, false)]).unwrap();
    let key = IdempotencyKey(format!("register:{t}"));
    let e = s.read(move |r| r.event_by_key(StreamId::Host, &key));
    assert_eq!(e.map(|e| e.seq), Some(1));
}

/// A job that panics after writing rolls back whole.
#[test]
fn job_panic_rolls_back() {
    let s = TestStore::new();
    let e = registered(thread(1), false);
    let r = s.run(move |tx| -> Result<(), chaldea_daemon::store::JobError> {
        tx.append(vec![e])?;
        chaldea_pty_runtime::fake::inject_panic("injected job panic");
    });
    assert!(matches!(r, Err(WriteError::Panicked)), "{r:?}");
    assert_eq!(host_head(&s), 0);
    // The writer continues.
    s.append(vec![registered(thread(2), false)]).unwrap();
    assert_eq!(host_head(&s), 1);
}

/// A full queue fails `submit` at once instead of waiting.
#[test]
fn full_queue_fails_at_once() {
    let s = store_with(Duration::from_secs(5), 2);
    let (release, blocker) = block_writer(&s);
    let mut tickets = Vec::new();
    for n in 0..2 {
        let e = registered(thread(n + 1), false);
        tickets.push(
            s.store()
                .submit(soon(), move |tx| Ok(tx.append(vec![e])?))
                .unwrap(),
        );
    }
    let t0 = Instant::now();
    let r = s.store().submit(soon(), |_| Ok(()));
    let elapsed = t0.elapsed();
    assert!(matches!(r, Err(SubmitError::QueueFull)), "{:?}", r.err());
    assert!(
        elapsed < Duration::from_secs(1),
        "submit waited {elapsed:?}"
    );
    release.send(()).unwrap();
    blocker.blocking_wait().unwrap();
    for t in tickets {
        t.blocking_wait().unwrap();
    }
    assert_eq!(host_head(&s), 2);
}

/// A job that waits on another job's ticket would wait for its own writer: the wait is
/// refused at once instead of deadlocking.
#[test]
fn waiting_on_a_ticket_inside_a_job_is_refused() {
    let s = TestStore::new();
    let store = s.store().clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let inner = store.clone();
        let r = store
            .submit(soon(), move |_| {
                let ticket = inner.submit(soon(), |_| Ok(())).unwrap();
                Ok(ticket.blocking_wait())
            })
            .unwrap()
            .blocking_wait();
        let _ = tx.send(r);
    });
    let r = rx.recv_timeout(WAIT).expect("the outer job returned");
    assert!(matches!(r, Ok(Err(WriteError::WouldDeadlock))), "{r:?}");
}
