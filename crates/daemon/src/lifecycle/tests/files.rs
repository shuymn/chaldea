//! PR8-CL-2 (`write_replace`), PR8-CL-3 (the lock, in process), PR8-CL-23 (the liveness rule).

use std::io;
use std::path::Path;
use std::time::Duration;

use chaldea_pty_runtime::host::{self, EntryKind, Privacy};

use super::super::files::{
    Discovery, DiscoveryRecord, Evidence, InstanceLock, LockError, LockProbe, RecordState,
    ReplaceError, ReplaceStage, discover, probe, write_replace,
};
use super::super::names;

const INSTANCE: &str = "00000000-0000-4000-8000-000000000001";

fn dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn ok(_: &str, _: ReplaceStage) -> io::Result<()> {
    Ok(())
}

fn temps(d: &Path) -> Vec<String> {
    std::fs::read_dir(d)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp-"))
        .collect()
}

#[test]
fn replaces_regular_file() {
    let d = dir();
    std::fs::write(d.path().join("port"), "1\n").unwrap();
    let r = write_replace(d.path(), "port", b"7373\n", INSTANCE, &ok).unwrap();
    assert!(r.durability_warning.is_none());
    assert_eq!(std::fs::read(d.path().join("port")).unwrap(), b"7373\n");
    assert!(temps(d.path()).is_empty());
    // An absent name is created.
    write_replace(d.path(), "new", b"x", INSTANCE, &ok).unwrap();
    assert_eq!(std::fs::read(d.path().join("new")).unwrap(), b"x");
}

/// A link at the name is refused; what it points to is unchanged.
#[cfg(unix)]
#[test]
fn refuses_symlink() {
    let d = dir();
    let outside = dir();
    let target = outside.path().join("victim");
    std::fs::write(&target, "keep").unwrap();
    chaldea_pty_runtime::fake::make_link(&d.path().join("port"), &target).unwrap();
    let e = write_replace(d.path(), "port", b"7373\n", INSTANCE, &ok).unwrap_err();
    assert!(matches!(e, ReplaceError::Refused), "{e:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    assert!(temps(d.path()).is_empty());
}

/// A junction (a reparse point) at the name is refused.
#[cfg(windows)]
#[test]
fn refuses_reparse_point() {
    let d = dir();
    let outside = dir();
    chaldea_pty_runtime::fake::make_link(&d.path().join("port"), outside.path()).unwrap();
    let e = write_replace(d.path(), "port", b"7373\n", INSTANCE, &ok).unwrap_err();
    assert!(matches!(e, ReplaceError::Refused), "{e:?}");
    assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
}

#[test]
fn write_failure_removes_the_temp() {
    let d = dir();
    std::fs::write(d.path().join("port"), "1\n").unwrap();
    let fail = |_: &str, s: ReplaceStage| {
        if s == ReplaceStage::Write {
            Err(io::Error::other("injected"))
        } else {
            Ok(())
        }
    };
    assert!(write_replace(d.path(), "port", b"7373\n", INSTANCE, &fail).is_err());
    assert_eq!(std::fs::read(d.path().join("port")).unwrap(), b"1\n");
    assert!(temps(d.path()).is_empty());
}

/// A rename failing for the whole retry window: the target is unchanged, no temporary left.
#[test]
fn rename_failure_keeps_the_target() {
    let d = dir();
    std::fs::write(d.path().join("port"), "1\n").unwrap();
    let fail = |_: &str, s: ReplaceStage| {
        if s == ReplaceStage::Rename {
            Err(io::Error::other("injected"))
        } else {
            Ok(())
        }
    };
    let t0 = std::time::Instant::now();
    assert!(write_replace(d.path(), "port", b"7373\n", INSTANCE, &fail).is_err());
    // Retried every 50 ms for 500 ms.
    assert!(
        t0.elapsed() >= Duration::from_millis(500),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(std::fs::read(d.path().join("port")).unwrap(), b"1\n");
    assert!(temps(d.path()).is_empty());
}

/// A failure after the rename is a durability warning; the new content is committed.
#[test]
fn dir_sync_failure_is_a_warning() {
    let d = dir();
    let fail = |_: &str, s: ReplaceStage| {
        if s == ReplaceStage::DirSync {
            Err(io::Error::other("injected"))
        } else {
            Ok(())
        }
    };
    let r = write_replace(d.path(), "port", b"7373\n", INSTANCE, &fail).unwrap();
    assert!(
        r.durability_warning
            .as_deref()
            .is_some_and(|w| w.contains("may not survive a power loss")),
        "{r:?}"
    );
    assert_eq!(std::fs::read(d.path().join("port")).unwrap(), b"7373\n");
}

/// The temporary is private before anything is written into it, read from its handle's
/// descriptor (macOS mode 0600 and no ACL; Windows owner and DACL).
#[test]
fn temp_descriptor_is_owner_only() {
    let d = dir();
    let tmp = d.path().join(format!(".owner.token.tmp-{INSTANCE}"));
    let seen = std::sync::Mutex::new(None);
    let check = |_: &str, s: ReplaceStage| {
        if s == ReplaceStage::Write {
            let f = host::open_no_follow(&tmp, host::Access::Read).unwrap();
            assert_eq!(
                std::fs::metadata(&tmp).unwrap().len(),
                0,
                "written before the check"
            );
            *seen.lock().unwrap() = Some(host::handle_privacy(&f).unwrap());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(f.metadata().unwrap().permissions().mode() & 0o777, 0o600);
            }
            #[cfg(windows)]
            {
                let (owner, allows) = chaldea_pty_runtime::fake::descriptor_sids(&tmp).unwrap();
                let me = chaldea_pty_runtime::fake::user_sid().unwrap();
                assert_eq!(owner, me);
                let mut a = allows.clone();
                a.sort();
                let mut want = vec![me, "S-1-5-18".to_owned()];
                want.sort();
                assert_eq!(a, want);
            }
        }
        Ok(())
    };
    write_replace(d.path(), "owner.token", b"chd_x\n", INSTANCE, &check).unwrap();
    assert_eq!(*seen.lock().unwrap(), Some(Privacy::Private));
    assert_eq!(
        host::privacy(&d.path().join("owner.token"), EntryKind::File).unwrap(),
        Privacy::Private
    );
}

/// A std reader holding the target open does not block the rename (L4).
#[cfg(windows)]
#[test]
fn replaces_while_a_reader_holds_it() {
    let d = dir();
    std::fs::write(d.path().join("daemon.json"), "old").unwrap();
    let held = std::fs::File::open(d.path().join("daemon.json")).unwrap();
    write_replace(d.path(), "daemon.json", b"new", INSTANCE, &ok).unwrap();
    drop(held);
    assert_eq!(std::fs::read(d.path().join("daemon.json")).unwrap(), b"new");
}

// ---- PR8-CL-3: the lock in process ----

#[test]
fn second_handle_is_held() {
    let d = dir();
    let l = InstanceLock::acquire(d.path(), Duration::ZERO).unwrap();
    assert!(matches!(
        InstanceLock::acquire(d.path(), Duration::from_millis(200)),
        Err(LockError::Held)
    ));
    assert_eq!(probe(d.path()).unwrap(), LockProbe::Held);
    l.release();
}

#[test]
fn release_then_reacquire() {
    let d = dir();
    assert_eq!(probe(d.path()).unwrap(), LockProbe::Missing);
    let l = InstanceLock::acquire(d.path(), Duration::ZERO).unwrap();
    l.release();
    assert_eq!(probe(d.path()).unwrap(), LockProbe::Free);
    let again = InstanceLock::acquire(d.path(), Duration::ZERO).unwrap();
    again.release();
    // The lock file holds no data.
    assert_eq!(
        std::fs::metadata(d.path().join(names::LOCK)).unwrap().len(),
        0
    );
}

/// A lock path that is a directory is an I/O error, not contention.
#[test]
fn io_error_is_not_contention() {
    let d = dir();
    std::fs::create_dir(d.path().join(names::LOCK)).unwrap();
    let t0 = std::time::Instant::now();
    let e = InstanceLock::acquire(d.path(), Duration::from_secs(2)).unwrap_err();
    assert!(matches!(e, LockError::Io(_)), "{e:?}");
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "retried an I/O error"
    );
}

// ---- PR8-CL-23: the liveness rule ----

fn record(pid: u32, start_time: &str) -> DiscoveryRecord {
    DiscoveryRecord {
        version: 1,
        instance: INSTANCE.to_owned(),
        pid,
        start_time: start_time.to_owned(),
        state: RecordState::Serving,
        port: Some(7373),
        url: Some("http://127.0.0.1:7373".to_owned()),
        started_at: "2026-11-23T00:00:00.000Z".to_owned(),
        detached: None,
        evidence: Evidence::default(),
    }
}

fn write_record(d: &Path, r: &DiscoveryRecord) {
    std::fs::write(d.join(names::RECORD), r.to_bytes()).unwrap();
}

#[test]
fn stale_record_with_free_lock_is_not_running() {
    let d = dir();
    let me = host::current_identity().unwrap();
    write_record(d.path(), &record(me.pid, &me.start_time));
    // No lock file at all, then a free one.
    assert_eq!(discover(d.path()), Discovery::NotRunning);
    InstanceLock::acquire(d.path(), Duration::ZERO)
        .unwrap()
        .release();
    assert_eq!(discover(d.path()), Discovery::NotRunning);
    // A client never removes the record.
    assert!(d.path().join(names::RECORD).exists());
}

#[test]
fn held_lock_with_dead_pid_is_unknown() {
    let d = dir();
    let l = InstanceLock::acquire(d.path(), Duration::ZERO).unwrap();
    let me = host::current_identity().unwrap();
    // A live pid with another start time, then a pid no process has.
    write_record(d.path(), &record(me.pid, "1"));
    assert_eq!(discover(d.path()), Discovery::Unknown);
    write_record(d.path(), &record(u32::MAX - 7, &me.start_time));
    assert_eq!(discover(d.path()), Discovery::Unknown);
    // The live identity is `Serving`.
    write_record(d.path(), &record(me.pid, &me.start_time));
    assert!(matches!(discover(d.path()), Discovery::Serving(_)));
    l.release();
}

/// A lock released after the record was read (its daemon removed the record and exited) is
/// not running: discovery checks the lock again after the read.
#[test]
fn lock_released_after_the_read_is_not_running() {
    let d = dir();
    let l = InstanceLock::acquire(d.path(), Duration::ZERO).unwrap();
    let me = host::current_identity().unwrap();
    write_record(d.path(), &record(me.pid, &me.start_time));
    let l = std::cell::Cell::new(Some(l));
    let got = super::super::files::discover_paused(d.path(), || {
        if let Some(l) = l.take() {
            l.release();
        }
    });
    assert_eq!(got, Discovery::NotRunning);
}

/// A record reached through a link is never used.
#[test]
fn linked_record_is_refused() {
    let d = dir();
    let other = dir();
    let l = InstanceLock::acquire(d.path(), Duration::ZERO).unwrap();
    let me = host::current_identity().unwrap();
    #[cfg(unix)]
    {
        write_record(other.path(), &record(me.pid, &me.start_time));
        chaldea_pty_runtime::fake::make_link(
            &d.path().join(names::RECORD),
            &other.path().join(names::RECORD),
        )
        .unwrap();
    }
    #[cfg(windows)]
    {
        let _ = &me;
        chaldea_pty_runtime::fake::make_link(&d.path().join(names::RECORD), other.path()).unwrap();
    }
    assert_eq!(discover(d.path()), Discovery::Unknown);
    l.release();
}

/// An `AlreadyExists` after this call created its temporary (here injected at the write) still
/// removes the temporary: cleanup follows what the call created, not the error's kind.
#[test]
fn already_exists_after_create_removes_the_temp() {
    let d = dir();
    let hook = |_: &str, s: ReplaceStage| {
        if s == ReplaceStage::Write {
            Err(io::Error::from(io::ErrorKind::AlreadyExists))
        } else {
            Ok(())
        }
    };
    let e = write_replace(d.path(), "port", b"7373\n", INSTANCE, &hook).unwrap_err();
    assert!(matches!(e, ReplaceError::Io(_)), "{e:?}");
    assert!(temps(d.path()).is_empty(), "{:?}", temps(d.path()));
    assert!(!d.path().join("port").exists());
}

/// Creates a FIFO with `/usr/bin/mkfifo` (no writer ever opens it).
#[cfg(target_os = "macos")]
pub(super) fn mkfifo(path: &Path) {
    let mut child = super::super::spawn_locked(
        std::process::Command::new("/usr/bin/mkfifo")
            .args(["-m", "600"])
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    )
    .unwrap();
    let mut status = None;
    let exited = super::wait_for(Duration::from_secs(10), || {
        status = child.try_wait().unwrap();
        status.is_some()
    });
    if !exited {
        super::kill_and_reap(&mut child);
        panic!("mkfifo did not exit within 10 s");
    }
    assert!(status.unwrap().success());
}

/// Runs `f` on a thread and returns its result if it finishes within 2 s; otherwise opens
/// `fifo` for writing (which releases a reader blocked in `open`) and returns `None`.
#[cfg(target_os = "macos")]
pub(super) fn without_blocking<T: Send + 'static>(
    fifo: &Path,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(v) => {
            t.join().unwrap();
            Some(v)
        }
        Err(_) => {
            // Non-blocking (`O_NONBLOCK`, 0x0004 on macOS): with no reader left the open fails
            // instead of waiting for one.
            use std::os::unix::fs::OpenOptionsExt;
            let _w = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(0x0004)
                .open(fifo);
            let _ = rx.recv_timeout(Duration::from_secs(10));
            None
        }
    }
}

/// A FIFO at `port`, `daemon.json`, or `owner.token` is refused at once instead of blocking
/// the opener.
#[cfg(target_os = "macos")]
#[test]
fn fifo_entries_do_not_block_readers() {
    use super::super::files::{check_token_file, read_port, read_record};
    for name in [names::PORT, names::RECORD, names::OWNER_TOKEN] {
        let d = dir();
        let fifo = d.path().join(name);
        mkfifo(&fifo);
        let p = d.path().to_path_buf();
        let refused = without_blocking(&fifo, move || match name {
            names::PORT => read_port(&p).is_err(),
            names::RECORD => read_record(&p).is_err(),
            _ => check_token_file(&p).is_err(),
        });
        assert_eq!(refused, Some(true), "{name}");
    }
}
