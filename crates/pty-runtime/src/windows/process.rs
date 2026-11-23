//! `process::lookup` and the ancestry walk's table on Windows (`docs/design/hook-ingest.md`
//! §2.5): one Toolhelp snapshot supplies every parent pid and `szExeFile`; each link's handle
//! is opened, checked with `sys::wait`, read for its creation time, and closed in turn.
//!
//! The mappings of an open failure and of a wait result are platform-independent and
//! unit-tested on every OS.

use std::io;

use super::kill::ERROR_INVALID_PARAMETER;
use crate::process::Lookup;

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

#[cfg(windows)]
pub(crate) use self::native::{lookup, table};

#[cfg(windows)]
mod native {
    use std::collections::HashMap;
    use std::os::windows::io::AsHandle;

    use super::{open_failure, wait_result};
    use crate::backend::ProcessIdentity;
    use crate::process::{Lookup, ProcessEntry, Table, normalize};
    use crate::windows::{QUERY_SYNC, sys};

    /// One snapshot of the process list: `pid → (ppid, normalized name)`, or `None` when the
    /// snapshot failed (every lookup is then unreadable).
    pub(crate) struct Snapshot(Option<HashMap<u32, (u32, String)>>);

    pub(crate) fn table() -> Snapshot {
        Snapshot(sys::process_snapshot().ok().map(|list| {
            list.into_iter()
                .map(|e| (e.pid, (e.ppid, normalize(&e.exe))))
                .collect()
        }))
    }

    pub(crate) fn lookup(pid: u32) -> Lookup {
        table().lookup(pid)
    }

    impl Table for Snapshot {
        fn lookup(&mut self, pid: u32) -> Lookup {
            let Some(map) = &self.0 else {
                return Lookup::Unreadable;
            };
            // Absent from the snapshot: the process it would be exited before it, and a
            // process holding the pid now was created after it.
            let Some((ppid, name)) = map.get(&pid) else {
                return Lookup::Gone;
            };
            let h = match sys::open_process(pid, QUERY_SYNC) {
                Ok(h) => h,
                Err(e) => return open_failure(&e),
            };
            if let Some(l) = wait_result(&sys::wait(h.as_handle(), 0)) {
                return l;
            }
            let Ok(start) = sys::creation_time(h.as_handle()) else {
                return Lookup::Unreadable;
            };
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
}

#[cfg(test)]
mod tests {
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
}
