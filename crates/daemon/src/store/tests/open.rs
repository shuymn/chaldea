//! `open` with fault points, projection versions, and the shutdown order (PR6-CL-17, CL-22).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use chaldea_core::ids::StreamId;

use super::helpers::{open_with, raw};
use super::support::{registered, soon, thread};
use crate::store::writer::Faults;
use crate::store::{Checkpoint, Store, StoreError, SubmitError, WriteError};

fn host_head(s: &Store) -> u64 {
    s.read(soon(), |r| r.head(StreamId::Host, 0))
        .unwrap()
        .head_seq
}

fn append_one(s: &Store, n: u128) -> Result<(), WriteError> {
    s.submit(soon(), move |tx| {
        Ok(tx.append(vec![registered(thread(n), false)]).map(|_| ())?)
    })
    .unwrap()
    .blocking_wait()
}

fn shutdown(s: &Store) {
    let r = s.shutdown(Instant::now() + Duration::from_secs(20));
    assert!(r.writer_exited, "{r:?}");
}

/// A creates the file; B opens, initializes, and commits an event; A then fails after
/// step 5. `open` never deletes a file, so reopening shows B's event.
#[test]
fn concurrent_open_failure_keeps_the_other_stores_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let ran = Arc::new(AtomicBool::new(false));
    let ran2 = ran.clone();
    let faults = Faults {
        after_create: Some(Box::new(move || {
            let b = open_with(&path, Faults::default()).unwrap();
            append_one(&b, 1).unwrap();
            shutdown(&b);
            ran2.store(true, Ordering::SeqCst);
        })),
        ..Faults::default()
    };
    faults.fail_open_after_init.store(true, Ordering::SeqCst);
    let a = open_with(dir.path(), faults);
    assert!(a.is_err(), "the injected failure did not fire");
    assert!(ran.load(Ordering::SeqCst));
    let reopened = open_with(dir.path(), Faults::default()).unwrap();
    assert_eq!(host_head(&reopened), 1, "B's event is gone");
    shutdown(&reopened);
}

/// A projection version other than the stored one, higher or lower, rebuilds at open.
#[test]
fn projection_version_change_rebuilds() {
    let dir = tempfile::tempdir().unwrap();
    let s = open_with(dir.path(), Faults::default()).unwrap();
    append_one(&s, 1).unwrap();
    shutdown(&s);
    let path = dir.path().join("chaldea.db");
    for version in [2, 1] {
        // Tamper with a projection row, so only a rebuild restores it.
        raw(&path)
            .execute("UPDATE proj_threads SET cwd = '/tampered'", [])
            .unwrap();
        let s = open_with(
            dir.path(),
            Faults {
                projection_version: Some(version),
                ..Faults::default()
            },
        )
        .unwrap();
        let (cwd, info) = s
            .read(soon(), |r| {
                Ok((r.thread(thread(1))?.unwrap().cwd, r.schema_info()?))
            })
            .unwrap();
        assert_eq!(cwd, "/work/repo", "version {version} did not rebuild");
        assert_eq!(info.projection_version, version.to_string());
        shutdown(&s);
    }
}

/// After a store of another projection version rebuilt, the first store's next job is
/// refused with `Failed(ProjectionVersion)` and it accepts no more writes.
#[test]
fn stale_projection_version_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let old = open_with(dir.path(), Faults::default()).unwrap();
    append_one(&old, 1).unwrap();
    let new = open_with(
        dir.path(),
        Faults {
            projection_version: Some(2),
            ..Faults::default()
        },
    )
    .unwrap();
    append_one(&new, 2).unwrap();
    let r = append_one(&old, 3);
    assert!(
        matches!(
            r,
            Err(WriteError::Failed(StoreError::ProjectionVersion {
                code: 1,
                ..
            }))
        ),
        "{r:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(old.submit(soon(), |_| Ok(())), Err(SubmitError::Failed)) {
        assert!(Instant::now() < deadline);
    }
    assert_eq!(host_head(&new), 2);
    shutdown(&old);
    shutdown(&new);
}

/// A shutdown that finishes in time closes the readers before the writer starts its
/// checkpoint: a held reader delays the checkpoint call, observed at its boundary.
#[test]
fn in_time_shutdown_checkpoints_after_readers() {
    let dir = tempfile::tempdir().unwrap();
    let s = open_with(dir.path(), Faults::default()).unwrap();
    append_one(&s, 1).unwrap();
    let (held_tx, held_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let reader = {
        let s = s.clone();
        std::thread::spawn(move || {
            s.read(soon(), |r| {
                let h = r.head(StreamId::Host, 0)?;
                held_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
                Ok(h)
            })
            .unwrap()
        })
    };
    held_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let report = Arc::new(Mutex::new(None));
    let shut = {
        let (s, report) = (s.clone(), report.clone());
        std::thread::spawn(move || {
            *report.lock().unwrap() = Some(s.shutdown(Instant::now() + Duration::from_secs(30)));
        })
    };
    // While the reader is held, the writer must not have begun its checkpoint.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !s.inner
            .faults
            .marks
            .lock()
            .unwrap()
            .contains(&"checkpoint_begin"),
        "the checkpoint began while a reader was lent"
    );
    s.inner.faults.mark("reader_released");
    release_tx.send(()).unwrap();
    reader.join().unwrap();
    shut.join().unwrap();
    let report = report.lock().unwrap().clone().unwrap();
    assert!(report.readers_returned, "{report:?}");
    assert!(report.writer_exited, "{report:?}");
    assert_eq!(report.checkpoint, Checkpoint::Complete, "{report:?}");
    // The boundaries this claim is about, in the order they were reached.
    let order = [
        "reader_released",
        "readers_closed",
        "checkpoint_begin",
        "checkpoint_end",
    ];
    let marks: Vec<&str> = s
        .inner
        .faults
        .marks
        .lock()
        .unwrap()
        .iter()
        .copied()
        .filter(|m| order.contains(m))
        .collect();
    assert_eq!(marks, order);
}
