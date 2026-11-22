//! The writer: poisoning, disk full, commit failures, the failed state, a writer that ends,
//! and a writer blocked past the shutdown deadline (PR6-CL-4, CL-15, CL-22).

use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chaldea_core::ids::StreamId;

use super::helpers::{Gates, store_with};
use super::support::{
    batch, bound, launch, new_generation, registered, reserved, soon, started, thread,
};
use crate::store::writer::{CommitFault, Faults};
use crate::store::{JobError, Store, StoreError, SubmitError, WriteError, WriteTx};

const A: &str = "/work/a.jsonl";

fn run<R: Send + 'static>(
    s: &Store,
    f: impl FnOnce(&mut WriteTx<'_>) -> Result<R, JobError> + Send + 'static,
) -> Result<R, WriteError> {
    s.submit(soon(), f).unwrap().blocking_wait()
}

fn count_events(s: &Store) -> u64 {
    s.read(soon(), |r| {
        Ok(r.head(StreamId::Host, 0)?.head_seq + r.head(StreamId::Thread(thread(1)), 0)?.head_seq)
    })
    .unwrap()
}

fn shutdown(s: &Store) {
    let r = s.shutdown(Instant::now() + Duration::from_secs(20));
    assert!(r.writer_exited, "{r:?}");
}

/// A `CHECK` failure ends only the statement; the closure catches it, writes again, and
/// returns `Ok`: the job still rolls back, earlier insert included.
#[test]
fn caught_statement_error_still_rolls_back() {
    let (_dir, s, _) = store_with(Faults::default());
    let reply = run(&s, |tx| {
        tx.append(vec![registered(thread(1), false)])?;
        // thread_registered on a thread stream breaks the membership CHECK.
        let mut bad = registered(thread(2), false);
        bad.stream_id = StreamId::Thread(thread(2));
        let err = tx.append(vec![bad]).expect_err("membership CHECK");
        assert!(matches!(err, StoreError::Constraint(_)), "{err}");
        assert!(!tx.raw().is_autocommit(), "a CHECK ends only the statement");
        let again = tx.append(vec![registered(thread(3), false)]);
        assert!(again.is_err(), "a poisoned WriteTx refuses further writes");
        Ok(())
    });
    assert!(
        matches!(
            reply,
            Err(WriteError::RolledBack(StoreError::Constraint(_)))
        ),
        "{reply:?}"
    );
    assert_eq!(count_events(&s), 0);
    shutdown(&s);
}

/// A `RAISE(ROLLBACK)` ends the transaction; the closure catches it, writes again, and
/// returns `Ok`: nothing autocommits and the earlier insert is gone.
#[test]
fn caught_transaction_error_does_not_autocommit() {
    let (_dir, s, _) = store_with(Faults::default());
    let (t, l) = (thread(1), launch(1));
    run(&s, move |tx| {
        tx.append(vec![registered(t, false)])?;
        tx.append(vec![
            reserved(t, l),
            started(t, l),
            bound(t, l, "s1", Some(A)),
        ])?;
        Ok(())
    })
    .unwrap();
    let before = count_events(&s);
    let reply = run(&s, move |tx| {
        tx.append(vec![registered(thread(2), false)])?;
        // The projection names path B, so the Rust binding check passes; the trigger reads
        // the events, where the bound path is A, and ends the transaction.
        tx.raw()
            .execute(
                "UPDATE proj_threads SET transcript_path = '/work/b.jsonl' WHERE thread_id = ?1",
                [t.to_string()],
            )
            .unwrap();
        let mut b = batch(t, l, "/work/b.jsonl", None);
        b.new_generation = Some(new_generation("f1"));
        let err = tx.append_transcript(b).expect_err("events_generation");
        assert!(
            matches!(&err, StoreError::Constraint(m) if m.contains("events_generation")),
            "{err}"
        );
        assert!(
            tx.raw().is_autocommit(),
            "RAISE(ROLLBACK) ended the transaction"
        );
        // Without poisoning this insert would commit on its own.
        let _ = tx.append(vec![registered(thread(3), false)]);
        Ok(())
    });
    assert!(
        matches!(
            reply,
            Err(WriteError::RolledBack(StoreError::Constraint(_)))
        ),
        "{reply:?}"
    );
    assert_eq!(count_events(&s), before);
    #[expect(
        clippy::redundant_closure_for_method_calls,
        reason = "the method path is not general over the ReadTx lifetime"
    )]
    let threads = s.read(soon(), |r| r.threads()).unwrap();
    assert_eq!(threads.len(), 1, "{threads:?}");
    assert_eq!(threads[0].transcript_path.as_deref(), Some(A));
    shutdown(&s);
}

/// `SQLITE_FULL` during a job rolls it back (`DiskFull`); the next job succeeds once space
/// exists.
#[test]
fn disk_full_rolls_back_and_recovers() {
    let (_dir, s, _) = store_with(Faults::default());
    run(&s, |tx| Ok(tx.append(vec![registered(thread(1), false)])?)).unwrap();
    let reply = run(&s, |tx| {
        let pages: i64 = tx
            .raw()
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        tx.raw()
            .pragma_update(None, "max_page_count", pages)
            .unwrap();
        tx.append(vec![registered(thread(2), false)])?;
        let big = super::support::raw_unknown(
            StreamId::Host,
            "big",
            crate::store::prepare_content(vec![7u8; 1 << 20], "application/octet-stream")
                .unwrap()
                .content,
        );
        let mut big = big;
        big.blobs = vec![super::support::blob_input(&vec![7u8; 1 << 20])];
        tx.append(vec![big])?;
        Ok(())
    });
    assert!(
        matches!(reply, Err(WriteError::RolledBack(StoreError::DiskFull))),
        "{reply:?}"
    );
    assert_eq!(count_events(&s), 1);
    run(&s, |tx| {
        tx.raw()
            .pragma_update(None, "max_page_count", 1_073_741_823)
            .unwrap();
        Ok(tx.append(vec![registered(thread(2), false)])?)
    })
    .unwrap();
    assert_eq!(count_events(&s), 2);
    shutdown(&s);
}

/// `COMMIT` failing with the transaction still open rolls back; the next job succeeds.
#[test]
fn commit_failure_with_open_transaction_rolls_back() {
    let faults = Faults::default();
    *faults.commit.lock().unwrap() = Some(CommitFault::BusyBeforeCommit);
    let (_dir, s, _) = store_with(faults);
    let reply = run(&s, |tx| Ok(tx.append(vec![registered(thread(1), false)])?));
    assert!(
        matches!(reply, Err(WriteError::RolledBack(StoreError::Busy))),
        "{reply:?}"
    );
    assert_eq!(count_events(&s), 0);
    run(&s, |tx| Ok(tx.append(vec![registered(thread(1), false)])?)).unwrap();
    assert_eq!(count_events(&s), 1);
    shutdown(&s);
}

/// `COMMIT` failing after the transaction ended is `CommitUnknown`; the writer is retired
/// and later submissions fail.
#[test]
fn commit_failure_with_ended_transaction_is_unknown() {
    let gates = Gates::default();
    let faults = gates.faults();
    *faults.commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    let (_dir, s, _) = store_with(faults);
    // Hold the writer at the start of its failed state: admission must already be refused
    // when the reply arrives.
    let (_reached, release) = gates.arm("fail_begin");
    let reply = run(&s, |tx| Ok(tx.append(vec![registered(thread(1), false)])?));
    assert!(
        matches!(reply, Err(WriteError::CommitUnknown(_))),
        "{reply:?}"
    );
    let next = s.submit(soon(), |_| Ok(())).err();
    drop(release);
    assert!(matches!(next, Some(SubmitError::Failed)), "{next:?}");
    // Reads continue; this fault committed for real.
    assert_eq!(count_events(&s), 1);
    shutdown(&s);
}

/// In the failed state every queued job gets `Failed` and new submissions are refused.
#[test]
fn failed_state_drains_the_queue() {
    let faults = Faults::default();
    *faults.commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    let (_dir, s, _) = store_with(faults);
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let first = s
        .submit(soon(), move |tx| {
            go_rx.recv_timeout(Duration::from_secs(20)).unwrap();
            Ok(tx.append(vec![registered(thread(1), false)])?)
        })
        .unwrap();
    let queued: Vec<_> = (2..5)
        .map(|n| {
            s.submit(soon(), move |tx| {
                Ok(tx.append(vec![registered(thread(n), false)])?)
            })
            .unwrap()
        })
        .collect();
    go_tx.send(()).unwrap();
    assert!(matches!(
        first.blocking_wait(),
        Err(WriteError::CommitUnknown(_))
    ));
    for q in queued {
        assert!(matches!(q.blocking_wait(), Err(WriteError::Failed(_))));
    }
    assert!(matches!(
        s.submit(soon(), |_| Ok(())),
        Err(SubmitError::Failed)
    ));
    assert_eq!(count_events(&s), 1);
    shutdown(&s);
}

/// A writer that ends outside a job resolves queued tickets `WriterGone` and refuses new
/// submissions.
#[test]
fn writer_gone_resolves_tickets() {
    let (_dir, s, _) = store_with(Faults::default());
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let first = s
        .submit(soon(), move |_| {
            started_tx.send(()).unwrap();
            go_rx.recv_timeout(Duration::from_secs(20)).unwrap();
            Ok(())
        })
        .unwrap();
    let queued = s.submit(soon(), |_| Ok(())).unwrap();
    // The writer is inside the first job, so the panic fires after it, outside any job.
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    s.inner
        .faults
        .panic_outside_job
        .store(true, Ordering::SeqCst);
    go_tx.send(()).unwrap();
    first.blocking_wait().unwrap();
    assert!(matches!(
        queued.blocking_wait(),
        Err(WriteError::WriterGone)
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match s.submit(soon(), |_| Ok(())) {
            Err(SubmitError::Closed) => break,
            Ok(t) => assert!(matches!(t.blocking_wait(), Err(WriteError::WriterGone))),
            Err(e) => panic!("{e:?}"),
        }
        assert!(Instant::now() < deadline, "submissions are still accepted");
    }
    let r = s.shutdown(Instant::now() + Duration::from_secs(20));
    assert!(r.writer_exited, "{r:?}");
}

/// A job blocked past the shutdown deadline: `writer_exited` false while every reader
/// returned.
#[test]
fn writer_blocked_past_the_deadline() {
    let (_dir, s, _) = store_with(Faults::default());
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let job = s
        .submit(soon(), move |_| {
            started_tx.send(()).unwrap();
            go_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            Ok(())
        })
        .unwrap();
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let r = s.shutdown(Instant::now() + Duration::from_secs(1));
    assert!(r.readers_returned, "{r:?}");
    assert!(!r.writer_exited, "{r:?}");
    assert_eq!(r.checkpoint, crate::store::Checkpoint::NotRun);
    go_tx.send(()).unwrap();
    job.blocking_wait().unwrap();
    let r = s.shutdown(Instant::now() + Duration::from_secs(20));
    assert!(r.writer_exited, "{r:?}");
}
