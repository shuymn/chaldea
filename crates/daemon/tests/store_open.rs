//! Open, epoch, header and schema checks, two stores on one file, long paths, and shutdown
//! (PR6-CL-17, CL-20, CL-21, CL-22, CL-24).

mod support;

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chaldea_core::ids::{IdempotencyKey, StreamId};
use chaldea_daemon::store::{Checkpoint, OpenError, ReadError, Store, SubmitError};
use rusqlite::{Connection, OpenFlags};
use support::{FixedClock, T0, TestStore, config, registered, shutdown_checked, soon, thread};

const WAIT: Duration = Duration::from_secs(30);

fn open(path: &Path) -> Result<Store, OpenError> {
    Store::open(config(path.to_path_buf(), FixedClock::new(T0)))
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

/// Undoes schema v3 (the two `proj_inputs` columns), so a v3 database reads as v2.
const V3_DOWN: &str = "ALTER TABLE proj_inputs DROP COLUMN last_write_before; \
     ALTER TABLE proj_inputs DROP COLUMN attempt_seq;";

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
                supported: 3
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
        Err(OpenError::SchemaMismatch { version: 3, names }) => {
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
    shutdown_checked(&store);
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
    assert_eq!(info.user_version, 3);
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
    shutdown_checked(&other);
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
    shutdown_checked(&store);
    let store = open(&path).unwrap_or_else(|e| panic!("reopen at {bytes} bytes: {e}"));
    assert_eq!(host_head(&store), 5, "{bytes} bytes");
    shutdown_checked(&store);
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
    shutdown_checked(&store);
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
        shutdown_checked(&store);
    }
}

/// A database whose text encoding is not UTF-8 is refused before any change; it would
/// count payload bytes in another encoding.
#[test]
fn refuses_a_non_utf8_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chaldea.db");
    {
        let c = Connection::open(&path).unwrap();
        c.execute_batch("PRAGMA encoding='UTF-16le'; CREATE TABLE old(x); DROP TABLE old;")
            .unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    match open(&path) {
        Err(e) => {
            assert!(matches!(e, OpenError::Encoding(_)), "{e}");
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        Ok(store) => {
            let result = store
                .submit(soon(), |tx| {
                    Ok(tx.append(vec![registered(thread(1), false)])?)
                })
                .unwrap()
                .blocking_wait();
            shutdown_checked(&store);
            assert!(result.is_ok(), "{result:?}");
        }
    }
}

/// Configuration out of range is an open error, not a panic, and touches nothing.
#[test]
fn oversized_configuration_returns_an_open_error() {
    type Change = fn(&mut chaldea_daemon::store::StoreConfig);
    let cases: [(&str, Change); 5] = [
        ("busy_timeout", |c| {
            c.busy_timeout = Duration::from_millis(u64::from(i32::MAX.unsigned_abs()) + 1);
        }),
        ("readers max", |c| c.readers = usize::MAX),
        ("readers 0", |c| c.readers = 0),
        ("writer_queue max", |c| c.writer_queue = usize::MAX),
        ("writer_queue 0", |c| c.writer_queue = 0),
    ];
    for (what, change) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chaldea.db");
        let mut cfg = config(path.clone(), FixedClock::new(T0));
        change(&mut cfg);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Store::open(cfg)));
        assert!(
            matches!(result, Ok(Err(OpenError::Config(_)))),
            "{what}: {:?}",
            result.map(|r| r.map(|_| ()))
        );
        assert!(!path.exists(), "{what}: the file was created");
    }
}

/// After shutdown, commit notifications close even while a clone lives.
#[test]
fn shutdown_closes_commit_notifications_with_a_live_clone() {
    use tokio::sync::broadcast::error::TryRecvError;

    let s = TestStore::new();
    let clone = s.store().clone();
    let mut notifications = clone.subscribe();
    assert!(clone.shutdown(soon()).writer_exited);
    assert!(matches!(
        notifications.try_recv(),
        Err(TryRecvError::Closed)
    ));
}

/// A v1 store (a v2 store without the v2 trigger, at `user_version` 1, which the v1
/// fingerprint accepts) opens as v2 with every row unchanged, consistent projections, and
/// the trigger in force (`docs/design/hook-ingest.md` §2.12).
#[test]
fn v1_upgrades_to_v2() {
    use chaldea_core::payload::Content;
    use chaldea_daemon::store::{
        BufferedHook, HookReply, NewHookDelivery, NewRouting, StoreError, WriteError,
    };
    use support::{bound, bound_thread, launch, raw_unknown, reserved};

    let mut s = TestStore::new();
    let (t1, l1) = (thread(1), launch(1));
    let (t2, l2) = (thread(2), launch(2));
    bound_thread(&s, t1, l1, Some("/work/t/a.jsonl"));
    s.append(vec![registered(t2, false)]).unwrap();
    s.append(vec![reserved(t2, l2)]).unwrap();
    let reply = |b: &str| HookReply {
        http_status: 200,
        body: b.into(),
    };
    let deliveries = vec![
        NewHookDelivery {
            launch_id: l1,
            emit_id: uuid::Uuid::from_u128(1),
            receipt_ordinal: 1,
            first_reply: reply(""),
            routing: NewRouting::Appended(vec![raw_unknown(
                StreamId::Thread(t1),
                "hook:1",
                Content::text("one"),
            )]),
        },
        NewHookDelivery {
            launch_id: l2,
            emit_id: uuid::Uuid::from_u128(2),
            receipt_ordinal: 1,
            first_reply: reply(""),
            routing: NewRouting::Buffered(BufferedHook {
                body: b"{\"hook_event_name\":\"Stop\"}".to_vec(),
                meta: "{\"v\":1}".into(),
            }),
        },
    ];
    for d in deliveries {
        s.run(move |tx| Ok(tx.hook_delivery_record(d)?)).unwrap();
    }
    s.shutdown();
    Connection::open(s.path())
        .unwrap()
        .execute_batch(&format!(
            "DROP TRIGGER events_session_bound_once; {V3_DOWN} PRAGMA user_version = 1"
        ))
        .unwrap();
    let before = support::http::dump(&s.path());

    s.reopen();
    assert_eq!(support::http::dump(&s.path()), before, "a v1 row changed");
    assert_eq!(s.read(|r| Ok(r.schema_info()?.user_version)), 3);
    let diff = s.run(|tx| Ok(tx.verify_projections()?)).unwrap();
    assert!(diff.is_empty(), "{diff:?}");
    // The v2 trigger is in force: a second binding of l1 is refused.
    let r = s.append(vec![bound(t1, l1, "s2", None)]);
    assert!(
        matches!(&r, Err(WriteError::RolledBack(StoreError::Constraint(m))) if m.contains("session_bound")),
        "{r:?}"
    );
}

/// PR10b-CL-22 (supervisor-send §2.9 E1): a v2 database, whose projections predate the
/// zero-byte attempt rule, opens as v3 with projections equal to a replay of its events: a
/// zero-byte attempt gives the launch its write position back, one followed by a
/// `human_write` does not.
#[test]
fn v2_upgrades_to_v3() {
    use chaldea_core::event::TypedEvent;
    use chaldea_core::ids::{Actor, LaunchId};
    use chaldea_core::input::{InputInput, InputReason, InputState};
    use chaldea_core::payload::{
        Content, HumanWrite, HumanWriteOrigin, InputCreated, InputEncoding, InputInitialState,
        InputOrigin, InputStateChanged,
    };
    use support::{bound_thread, event, input, launch};

    let mut s = TestStore::new();
    let (t, l) = (thread(1), launch(1));
    bound_thread(&s, t, l, None);
    let stream = StreamId::Thread(t);
    let created = |i| {
        event(
            stream,
            &format!("created:{i}"),
            TypedEvent::InputCreated(InputCreated {
                input_id: i,
                launch_id: Some(l),
                initial_state: InputInitialState::Queued,
                text: Content::text("hi"),
                encoding: InputEncoding::BracketedPaste,
                origin: InputOrigin::Cli,
                created_by: Actor::Human,
            }),
        )
    };
    let moved = |i, n: &str, from, to, by: InputInput| {
        event(
            stream,
            &format!("moved:{i}:{n}"),
            TypedEvent::InputStateChanged(InputStateChanged {
                input_id: i,
                from,
                to,
                reason: InputReason::from(&by),
                sent_after_seq: None,
                by: None,
            }),
        )
    };
    let human = |n: &str, l: LaunchId| {
        event(
            stream,
            &format!("human:{n}"),
            TypedEvent::HumanWrite(HumanWrite {
                launch_id: l,
                origin: HumanWriteOrigin::Key,
                approval_id: None,
            }),
        )
    };
    let (a, b) = (input(1), input(2));
    let open = |i| {
        moved(
            i,
            "open",
            InputState::Queued,
            InputState::Sending,
            InputInput::GateOpened { sent_after_seq: 0 },
        )
    };
    let cancel = |i| {
        moved(
            i,
            "cancel",
            InputState::Sending,
            InputState::Cancelled,
            InputInput::RecheckFailed,
        )
    };
    s.append(vec![created(a), open(a), cancel(a)]).unwrap();
    let last = |s: &TestStore| s.read(move |r| Ok(r.launch(l)?.unwrap().last_write_seq));
    assert_eq!(last(&s), None, "a zero-byte attempt restores the position");
    let hw = s.append(vec![created(b), open(b), human("1", l)]).unwrap();
    let human_seq = hw[2].envelope().seq;
    s.append(vec![cancel(b)]).unwrap();
    assert_eq!(
        last(&s),
        Some(human_seq),
        "a later write keeps its position"
    );
    s.shutdown();
    // The v2 file: no attempt columns, projection version 1, and the launch's position as
    // v2's rule left it (moved by the attempt).
    Connection::open(s.path())
        .unwrap()
        .execute_batch(&format!(
            "{V3_DOWN} PRAGMA user_version = 2; \
             UPDATE meta SET value = '1' WHERE key = 'projection_version'; \
             UPDATE proj_launches SET last_write_seq = 999"
        ))
        .unwrap();
    s.reopen();
    #[allow(
        clippy::redundant_closure_for_method_calls,
        reason = "the method path is not general over the lifetime"
    )]
    let info = s.read(|r| r.schema_info());
    assert_eq!(
        (info.user_version, info.projection_version.as_str()),
        (3, "3")
    );
    let diff = s.run(|tx| Ok(tx.verify_projections()?)).unwrap();
    assert!(diff.is_empty(), "{diff:?}");
    assert_eq!(last(&s), Some(human_seq));
}
