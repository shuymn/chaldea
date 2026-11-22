//! Process-table lookups (`docs/design/pty-unix.md` §2.5): one process as libproc reports
//! it, with zombie, gone, and unreadable kept apart.

use super::sys;

/// A live process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Proc {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    pub(crate) pgid: u32,
    pub(crate) start: u64,
}

/// The outcome of one `PROC_PIDTBSDINFO` (arg 1) lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lookup {
    Live(Proc),
    /// An exited, unreaped process, with its start.
    Zombie(u64),
    /// `ESRCH`.
    Gone,
    /// `EPERM` or any other failure: the process may exist.
    Unreadable,
}

pub(crate) fn lookup(pid: u32) -> Lookup {
    match sys::bsd_info(pid) {
        Ok(i) if i.zombie => Lookup::Zombie(i.start),
        Ok(i) => Lookup::Live(Proc {
            pid: i.pid,
            ppid: i.ppid,
            pgid: i.pgid,
            start: i.start,
        }),
        Err(libc::ESRCH) => Lookup::Gone,
        Err(_) => Lookup::Unreadable,
    }
}
