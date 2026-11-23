//! `process::lookup` and the ancestry walk's table on Windows (`docs/design/hook-ingest.md`
//! §2.5): one Toolhelp snapshot supplies every parent pid and `szExeFile`; each link's handle
//! is opened, checked with `sys::wait`, read for its creation time, and closed in turn.
//!
//! The table's decisions run over a [`WinOs`], so they are unit-tested on every OS with a
//! scripted one; only [`native`]'s calls reach the OS.

use std::collections::HashMap;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use super::kill::ERROR_INVALID_PARAMETER;
use crate::backend::ProcessIdentity;
use crate::process::{Lookup, ProcessEntry, Table, normalize};

/// What a failed `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE)` means:
/// `ERROR_INVALID_PARAMETER` is no such pid; anything else (access denied) is unreadable.
pub(crate) fn open_failure(e: &io::Error) -> Lookup {
    if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER) {
        Lookup::Gone
    } else {
        Lookup::Unreadable
    }
}

/// What `sys::wait(h, 0)` says about the process: `None` while it runs; signalled (exited,
/// even while a handle keeps its object) is gone; a failed wait is unreadable.
pub(crate) fn wait_result(r: &io::Result<bool>) -> Option<Lookup> {
    match r {
        Ok(false) => None,
        Ok(true) => Some(Lookup::Gone),
        Err(_) => Some(Lookup::Unreadable),
    }
}

/// The OS calls of one lookup after the snapshot.
pub(crate) trait WinOs {
    type Handle;
    fn open(&self, pid: u32) -> io::Result<Self::Handle>;
    /// `sys::wait(h, 0)`: true when signalled.
    fn wait(&self, h: &Self::Handle) -> io::Result<bool>;
    /// The creation time, in 100 ns units since 1601-01-01 UTC.
    fn creation_time(&self, h: &Self::Handle) -> io::Result<u64>;
}

/// One snapshot row: the parent pid and the normalized image name.
pub(crate) type Row = (u32, String);

/// One snapshot of the process list and the system time read just before it.
pub(crate) struct Snapshot<O> {
    os: O,
    /// `None`: the snapshot failed, and every lookup is unreadable.
    rows: Option<HashMap<u32, Row>>,
    /// The system time read before the snapshot was taken (100 ns since 1601).
    taken: u64,
}

impl<O> Snapshot<O> {
    pub(crate) fn new(os: O, taken: u64, rows: Option<HashMap<u32, Row>>) -> Snapshot<O> {
        Snapshot { os, rows, taken }
    }
}

impl<O: WinOs> Table for Snapshot<O> {
    fn lookup(&mut self, pid: u32) -> Lookup {
        let Some(rows) = &self.rows else {
            return Lookup::Unreadable;
        };
        // Absent from the snapshot: the process it would be exited before it, and a
        // process holding the pid now was created after it.
        let Some((ppid, name)) = rows.get(&pid) else {
            return Lookup::Gone;
        };
        let h = match self.os.open(pid) {
            Ok(h) => h,
            Err(e) => return open_failure(&e),
        };
        if let Some(l) = wait_result(&self.os.wait(&h)) {
            return l;
        }
        let Ok(start) = self.os.creation_time(&h) else {
            return Lookup::Unreadable;
        };
        // An ambiguity check. A handle created after the time sampled before the snapshot
        // may belong to a later process that took the pid after the row's process exited, so
        // the row and the handle may be two processes; the walk fails closed. It also refuses
        // a process created between that sample and the snapshot, whose row is its own. Both
        // are system-time stamps, so a clock step can refuse a live process or let a reuse
        // through, as for the walk's start-time check (§2.5); a reuse within the clock's
        // resolution of the sample is not seen.
        if start > self.taken {
            return Lookup::Unreadable;
        }
        Lookup::Live(ProcessEntry {
            identity: ProcessIdentity {
                pid,
                start_time: start.to_string(),
                group: String::new(),
            },
            ppid: *ppid,
            name: name.clone(),
        })
    }
}

/// The system time now, as a `FILETIME` value (100 ns units since 1601-01-01 UTC).
pub(crate) fn filetime_now() -> u64 {
    /// 1601-01-01 to 1970-01-01 in 100 ns units.
    const EPOCH_DIFF: u64 = 116_444_736_000_000_000;
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() / 100);
    EPOCH_DIFF.saturating_add(u64::try_from(since).unwrap_or(u64::MAX))
}

/// Snapshot rows from Toolhelp entries.
pub(crate) fn rows(entries: impl IntoIterator<Item = (u32, u32, String)>) -> HashMap<u32, Row> {
    entries
        .into_iter()
        .map(|(pid, ppid, exe)| (pid, (ppid, normalize(&exe))))
        .collect()
}

#[cfg(windows)]
pub(crate) use self::native::{lookup, table};

#[cfg(windows)]
mod native {
    use std::io;
    use std::os::windows::io::{AsHandle, OwnedHandle};

    use super::{Snapshot, WinOs, filetime_now, rows};
    use crate::process::{Lookup, Table};
    use crate::windows::{QUERY_SYNC, sys};

    pub(crate) struct Native;

    impl WinOs for Native {
        type Handle = OwnedHandle;
        fn open(&self, pid: u32) -> io::Result<OwnedHandle> {
            sys::open_process(pid, QUERY_SYNC)
        }
        fn wait(&self, h: &OwnedHandle) -> io::Result<bool> {
            sys::wait(h.as_handle(), 0)
        }
        fn creation_time(&self, h: &OwnedHandle) -> io::Result<u64> {
            sys::creation_time(h.as_handle())
        }
    }

    pub(crate) fn table() -> Snapshot<Native> {
        let taken = filetime_now();
        let entries = sys::process_snapshot()
            .ok()
            .map(|list| rows(list.into_iter().map(|e| (e.pid, e.ppid, e.exe))));
        Snapshot::new(Native, taken, entries)
    }

    pub(crate) fn lookup(pid: u32) -> Lookup {
        table().lookup(pid)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn wait_result_mapping() {
        assert_eq!(wait_result(&Ok(false)), None);
        assert_eq!(wait_result(&Ok(true)), Some(Lookup::Gone));
        assert_eq!(
            wait_result(&Err(io::Error::from_raw_os_error(6))),
            Some(Lookup::Unreadable)
        );
        assert_eq!(
            wait_result(&Err(io::Error::other("WaitForSingleObject returned 0x80"))),
            Some(Lookup::Unreadable)
        );
        assert_eq!(
            open_failure(&io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER)),
            Lookup::Gone
        );
        // ERROR_ACCESS_DENIED.
        assert_eq!(
            open_failure(&io::Error::from_raw_os_error(5)),
            Lookup::Unreadable
        );
    }

    /// Scripted processes by pid: `(signalled, creation time)`, or an open error code.
    #[derive(Default)]
    struct Scripted {
        procs: HashMap<u32, Result<(bool, u64), i32>>,
        opened: RefCell<Vec<u32>>,
    }

    impl WinOs for &Scripted {
        type Handle = (bool, u64);
        fn open(&self, pid: u32) -> io::Result<(bool, u64)> {
            self.opened.borrow_mut().push(pid);
            match self.procs.get(&pid) {
                Some(Ok(p)) => Ok(*p),
                Some(Err(code)) => Err(io::Error::from_raw_os_error(*code)),
                None => Err(io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER)),
            }
        }
        fn wait(&self, h: &(bool, u64)) -> io::Result<bool> {
            Ok(h.0)
        }
        fn creation_time(&self, h: &(bool, u64)) -> io::Result<u64> {
            Ok(h.1)
        }
    }

    const TAKEN: u64 = 1_000;

    fn snapshot(os: &Scripted) -> Snapshot<&Scripted> {
        let r = rows([
            (40, 30, "Chaldea.exe".to_owned()),
            (30, 20, "codex.exe".to_owned()),
        ]);
        Snapshot::new(os, TAKEN, Some(r))
    }

    #[test]
    fn a_snapshot_row_and_its_handle_make_one_entry() {
        let mut os = Scripted::default();
        os.procs.insert(40, Ok((false, 900)));
        let mut t = snapshot(&os);
        let Lookup::Live(e) = t.lookup(40) else {
            panic!("not live")
        };
        assert_eq!(
            (e.identity.pid, e.identity.start_time.as_str()),
            (40, "900")
        );
        assert_eq!((e.ppid, e.name.as_str()), (30, "chaldea"));
        // Absent from the snapshot: gone, without an open.
        assert_eq!(t.lookup(99), Lookup::Gone);
        assert_eq!(*os.opened.borrow(), [40]);
        // Signalled, or refused: gone, unreadable.
        let mut os = Scripted::default();
        os.procs.insert(40, Ok((true, 900)));
        os.procs.insert(30, Err(5));
        let mut t = snapshot(&os);
        assert_eq!(t.lookup(40), Lookup::Gone);
        assert_eq!(t.lookup(30), Lookup::Unreadable);
        // A failed snapshot: unreadable.
        let os = Scripted::default();
        let mut t = Snapshot::new(&os, TAKEN, None);
        assert_eq!(t.lookup(40), Lookup::Unreadable);
    }

    /// Pid 40 exited after the snapshot and a later process took it before the open: the
    /// handle's creation time is after the snapshot, so its row describes another process.
    #[test]
    fn a_pid_reused_after_the_snapshot_is_unreadable() {
        let mut os = Scripted::default();
        os.procs.insert(40, Ok((false, TAKEN + 1)));
        let mut t = snapshot(&os);
        assert_eq!(t.lookup(40), Lookup::Unreadable);
        // Created at the snapshot's time: the same generation.
        let mut os = Scripted::default();
        os.procs.insert(40, Ok((false, TAKEN)));
        let mut t = snapshot(&os);
        assert!(matches!(t.lookup(40), Lookup::Live(_)));
    }

    #[test]
    fn filetime_now_counts_from_1601() {
        // 2020-01-01T00:00:00Z as a FILETIME value.
        let y2020 = 132_223_104_000_000_000;
        assert!(filetime_now() > y2020);
    }
}
