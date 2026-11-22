//! Open, epoch, header and schema checks, two stores on one file, long paths, and shutdown
//! (PR6-CL-17, CL-20, CL-21, CL-22, CL-24).

mod support;

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chaldea_core::ids::{IdempotencyKey, StreamId};
use chaldea_daemon::store::{Checkpoint, OpenError, ReadError, Store, SubmitError};
use rusqlite::{Connection, OpenFlags};
use support::{FixedClock, T0, TestStore, config, registered, soon, thread};

const WAIT: Duration = Duration::from_secs(30);

fn open(path: &Path) -> Result<Store, OpenError> {
    Store::open(config(path.to_path_buf(), FixedClock::new(T0)))
}

fn close(store: &Store) {
    let report = store.shutdown(Instant::now() + Duration::from_secs(20));
    assert!(report.writer_exited, "{report:?}");
}

fn host_head(store: &Store) -> u64 {
    store
        .read(soon(), |r| Ok(r.head(StreamId::Host, 0)?.head_seq))
        .unwrap()
}

fn raw_epoch(path: &Path) -> String {
    let raw = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    raw.query_row("SELECT value FROM meta WHERE key = 'epoch'", [], |r| {
        r.get(0)
    })
    .unwrap()
}

/// The epoch is created once, cached equal to its row, and survives reopen.
#[test]
fn epoch_survives_reopen() {
    let mut s = TestStore::new();
    let epoch = s.store().epoch().clone();
    assert_eq!(raw_epoch(&s.path()), epoch.0);
    s.append(vec![registered(thread(1), false)]).unwrap();
    s.reopen();
    assert_eq!(s.store().epoch(), &epoch);
    assert_eq!(raw_epoch(&s.path()), epoch.0);
}

/// A file that is not a database is refused and left as it was.
#[test]
fn refuses_not_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chaldea.db");
    let bytes = vec![b'x'; 4096];
    std::fs::write(&path, &bytes).unwrap();
    let r = open(&path);
    assert!(matches!(r, Err(OpenError::NotADatabase)), "{:?}", r.err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

/// Another application's rollback-journal database is refused before Chaldea changes a byte:
/// the header's journal mode (bytes 18–19) and the whole file stay, and no WAL appears.
#[test]
fn refuses_foreign_rollback_journal_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chaldea.db");
    {
        let raw = Connection::open(&path).unwrap();
        raw.execute_batch(
            "PRAGMA application_id = 1234; CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);",
        )
        .unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    assert_eq!(&before[18..20], &[1, 1], "a rollback-journal database");
    let r = open(&path);
    assert!(
        matches!(
            r,
            Err(OpenError::Foreign {
                application_id: 1234
            })
        ),
        "{:?}",
        r.err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let mut wal = path.clone().into_os_string();
    wal.push("-wal");
    assert!(!PathBuf::from(wal).exists());
}

/// A schema version above the binary's is refused.
#[test]
fn refuses_newer_schema() {
    let mut s = TestStore::new();
    s.shutdown();
    Connection::open(s.path())
        .unwrap()
        .execute_batch("PRAGMA user_version = 99")
        .unwrap();
    let r = open(&s.path());
    assert!(
        matches!(
            r,
            Err(OpenError::NewerSchema {
                found: 99,
                supported: 1
            })
        ),
        "{:?}",
        r.err()
    );
}

/// A dropped trigger is a changed schema, named in the error.
#[test]
fn refuses_altered_schema() {
    let mut s = TestStore::new();
    s.shutdown();
    Connection::open(s.path())
        .unwrap()
        .execute_batch("DROP TRIGGER events_no_update")
        .unwrap();
    match open(&s.path()) {
        Err(OpenError::SchemaMismatch { version: 1, names }) => {
            assert!(names.contains(&"events_no_update".to_owned()), "{names:?}");
        }
        other => panic!("{:?}", other.err()),
    }
}

/// An extra table whose name only looks like SQLite's internal prefix is not hidden.
#[test]
fn refuses_an_extra_table() {
    let mut s = TestStore::new();
    s.shutdown();
    Connection::open(s.path())
        .unwrap()
        .execute_batch("CREATE TABLE sqliteXextra (x INTEGER)")
        .unwrap();
    match open(&s.path()) {
        Err(OpenError::SchemaMismatch { names, .. }) => {
            assert!(names.contains(&"sqliteXextra".to_owned()), "{names:?}");
        }
        other => panic!("{:?}", other.err()),
    }
}

/// An empty file (e.g. left by a failed first open) is initialized as fresh.
#[test]
fn uninitialized_file_is_reused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chaldea.db");
    std::fs::File::create(&path).unwrap();
    let store = open(&path).unwrap();
    store
        .submit(soon(), |tx| {
            Ok(tx.append(vec![registered(thread(1), false)])?)
        })
        .unwrap()
        .blocking_wait()
        .unwrap();
    assert_eq!(host_head(&store), 1);
    close(&store);
}

/// Both runners run the bundled SQLite.
#[test]
fn sqlite_version_is_bundled() {
    let s = TestStore::new();
    #[allow(
        clippy::redundant_closure_for_method_calls,
        reason = "the method path is not general over the lifetime"
    )]
    let info = s.read(|r| r.schema_info());
    assert_eq!(info.sqlite_version, "3.54.1");
    assert_eq!(info.user_version, 1);
    assert_eq!(info.application_id, 0x4348_4C44);
}

/// Two stores on one file serialize their writes; each job sees the other's committed rows.
#[test]
fn second_store_serializes_writes() {
    let s = TestStore::new();
    let other = open(&s.path()).unwrap();
    let stores = [s.store().clone(), other.clone()];
    let workers: Vec<_> = stores
        .into_iter()
        .enumerate()
        .map(|(w, store)| {
            std::thread::spawn(move || {
                for n in 0..20u128 {
                    let id = u128::try_from(w).unwrap() * 100 + n + 1;
                    let e = registered(thread(id), false);
                    let (head, seq) = store
                        .submit(soon(), move |tx| {
                            let head = tx.read().head(StreamId::Host, 0)?.head_seq;
                            let a = tx.append(vec![e])?;
                            Ok((head, a[0].envelope().seq))
                        })
                        .unwrap()
                        .blocking_wait()
                        .unwrap();
                    assert_eq!(seq, head + 1);
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    // Each store reads every row the other committed.
    for store in [s.store(), &other] {
        let page = store
            .read(soon(), |r| r.events_after(StreamId::Host, 0, 100))
            .unwrap();
        let seqs: Vec<u64> = page.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (1..=40).collect::<Vec<u64>>());
        let key = IdempotencyKey(format!("register:{}", thread(101)));
        assert!(
            store
                .read(soon(), |r| r.event_by_key(StreamId::Host, &key))
                .unwrap()
                .is_some()
        );
    }
    close(&other);
}

/// The nested multibyte directory for the long-path tests: at least `bytes` UTF-8 bytes in
/// total, with levels of `per_level` Japanese characters.
fn nested(base: &Path, bytes: usize, per_level: usize, max_bytes: usize) -> PathBuf {
    let level = "長い経路の名前を試験する日本語のディレクトリ"
        .chars()
        .cycle()
        .take(per_level)
        .collect::<String>();
    let mut dir = base.to_path_buf();
    let file = "chaldea.db-wal".len() + 1;
    while dir.as_os_str().len() + file < bytes {
        let next = dir.join(&level);
        if next.as_os_str().len() + file > max_bytes {
            break;
        }
        dir = next;
    }
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| {
        panic!(
            "create {} ({} bytes): {e}",
            dir.display(),
            dir.as_os_str().len()
        )
    });
    dir
}

/// A store under a deep multibyte path opens, writes its WAL and SHM, and reopens.
#[test]
fn long_multibyte_path() {
    let tmp = tempfile::tempdir().unwrap();
    // Windows: past the default win32 VFS limit (1040 bytes). Unix: the unix VFS caps a path
    // at 512 bytes (MAX_PATHNAME), so stay just below it.
    let dir = if cfg!(windows) {
        nested(tmp.path(), 1200, 40, usize::MAX)
    } else {
        nested(tmp.path(), 480, 10, 480)
    };
    let path = dir.join("chaldea.db");
    let bytes = path.as_os_str().len();
    if cfg!(windows) {
        let units = path.to_string_lossy().encode_utf16().count();
        assert!(bytes > 1100 && units > 300, "{bytes} bytes, {units} units");
    }
    let store = open(&path).unwrap_or_else(|e| panic!("open at {bytes} bytes: {e}"));
    for n in 0..5 {
        store
            .submit(soon(), move |tx| {
                Ok(tx.append(vec![registered(thread(n + 1), false)])?)
            })
            .unwrap()
            .blocking_wait()
            .unwrap_or_else(|e| panic!("write at {bytes} bytes: {e}"));
    }
    let paths = store.paths();
    assert!(paths.wal.exists(), "no -wal at {bytes} bytes");
    assert!(paths.shm.exists(), "no -shm at {bytes} bytes");
    close(&store);
    let store = open(&path).unwrap_or_else(|e| panic!("reopen at {bytes} bytes: {e}"));
    assert_eq!(host_head(&store), 5, "{bytes} bytes");
    close(&store);
}

/// On Windows the store selects the `win32-longpath` VFS: the default VFS cannot open the path.
#[cfg(windows)]
#[test]
fn long_path_vfs_is_selected() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(nested(tmp.path(), 1100, 40, usize::MAX)).unwrap();
    let path = dir.join("chaldea.db");
    let bytes = path.as_os_str().len();
    assert!(bytes > 1040, "{bytes} bytes");
    let plain = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .and_then(|c| c.execute_batch("CREATE TABLE t (x INTEGER)"));
    assert!(
        plain.is_err(),
        "the default win32 VFS opened a {bytes}-byte path; the negative control is wrong"
    );
    let _ = std::fs::remove_file(&path);
    let store = open(&path).unwrap_or_else(|e| panic!("Store::open at {bytes} bytes: {e}"));
    store
        .submit(soon(), |tx| {
            Ok(tx.append(vec![registered(thread(1), false)])?)
        })
        .unwrap()
        .blocking_wait()
        .unwrap_or_else(|e| panic!("write at {bytes} bytes: {e}"));
    close(&store);
}

/// After shutdown no clone can submit or read.
#[test]
fn shutdown_stops_every_clone() {
    let mut s = TestStore::new();
    let clone = s.store().clone();
    s.shutdown();
    assert!(matches!(
        clone.submit(soon(), |_| Ok(())).err(),
        Some(SubmitError::Closed)
    ));
    assert_eq!(
        clone.read(soon(), |_| Ok(())).err(),
        Some(ReadError::Closed)
    );
}

/// Holds a reader until the returned sender is used; returns once it is held.
fn hold_reader(store: &Store) -> (mpsc::Sender<()>, mpsc::Receiver<bool>) {
    let store = store.clone();
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = store.read(soon(), |r| {
            let _ = r.head(StreamId::Host, 0)?;
            held_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(WAIT);
            Ok(())
        });
        let _ = done_tx.send(r.is_ok());
    });
    held_rx.recv_timeout(WAIT).expect("the reader is held");
    (release_tx, done_rx)
}

/// A reader held past the deadline: the report says so, and states the writer as observed.
#[test]
fn deadline_with_a_held_reader() {
    let mut s = TestStore::new();
    s.append(vec![registered(thread(1), false)]).unwrap();
    let store = s.store.take().unwrap();
    let (release, done) = hold_reader(&store);
    let report = store.shutdown(Instant::now() + Duration::from_secs(1));
    assert!(!report.readers_returned, "{report:?}");
    assert_eq!(report.readers_abandoned, 1, "{report:?}");
    if report.writer_exited {
        assert_ne!(report.checkpoint, Checkpoint::NotRun, "{report:?}");
    } else {
        assert_eq!(report.checkpoint, Checkpoint::NotRun, "{report:?}");
    }
    release.send(()).unwrap();
    assert!(done.recv_timeout(WAIT).unwrap());
    // Reap the writer before the directory goes.
    let report = store.shutdown(Instant::now() + Duration::from_secs(20));
    assert!(report.writer_exited, "{report:?}");
}

/// A reader returned before the deadline: `shutdown` waited for it.
#[test]
fn shutdown_waits_for_lent_readers() {
    let mut s = TestStore::new();
    let store = s.store.take().unwrap();
    let (release, done) = hold_reader(&store);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let _ = release.send(());
    });
    let report = store.shutdown(Instant::now() + Duration::from_secs(20));
    assert!(done.recv_timeout(WAIT).unwrap());
    assert!(report.readers_returned, "{report:?}");
    assert_eq!(report.readers_abandoned, 0);
    assert!(report.writer_exited, "{report:?}");
    assert_eq!(report.checkpoint, Checkpoint::Complete);
}

/// After shutdown no handle keeps a file open: on Windows the files delete; on macOS a new
/// store reopens them.
#[test]
fn shutdown_releases_every_file() {
    let mut s = TestStore::new();
    s.append(vec![registered(thread(1), false)]).unwrap();
    let _ = s.read(|r| r.head(StreamId::Host, 0));
    let paths = s.store().paths();
    let report = s.shutdown();
    assert!(
        report.readers_returned && report.writer_exited,
        "{report:?}"
    );
    if cfg!(windows) {
        for p in [&paths.db, &paths.wal, &paths.shm] {
            if p.exists() {
                std::fs::remove_file(p)
                    .unwrap_or_else(|e| panic!("delete {} after shutdown: {e}", p.display()));
            }
        }
        assert!(!paths.db.exists());
    } else {
        let store = open(&paths.db).unwrap();
        assert_eq!(host_head(&store), 1);
        close(&store);
    }
}
