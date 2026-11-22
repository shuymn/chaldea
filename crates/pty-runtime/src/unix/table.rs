//! Process-table lookups (`docs/design/pty-unix.md` §2.5): one process as libproc reports
//! it, with zombie, gone, and unreadable kept apart.

use super::sys::{self, BsdInfo};

/// A live process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Proc {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    pub(crate) pgid: u32,
    pub(crate) start: u64,
}

/// The outcome of one lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lookup {
    Live(Proc),
    /// An exited, unreaped process, with its start (0 when only the short record, which
    /// has no start, could be read: another user's zombie).
    Zombie(u64),
    /// `ESRCH`.
    Gone,
    /// `EPERM` or any other failure, and not a confirmed zombie: the process may exist.
    Unreadable,
}

pub(crate) fn lookup(pid: u32) -> Lookup {
    classify(sys::bsd_info(pid), || sys::short_zombie(pid))
}

/// `PROC_PIDTBSDINFO` (arg 1) first; when it is refused, the short record, which another
/// user's process allows, tells a zombie apart. Anything not established stays
/// `Unreadable`.
fn classify(info: Result<BsdInfo, i32>, short: impl FnOnce() -> Result<bool, i32>) -> Lookup {
    match info {
        Ok(i) if i.zombie => Lookup::Zombie(i.start),
        Ok(i) => Lookup::Live(Proc {
            pid: i.pid,
            ppid: i.ppid,
            pgid: i.pgid,
            start: i.start,
        }),
        Err(libc::ESRCH) => Lookup::Gone,
        Err(_) => match short() {
            Ok(true) => Lookup::Zombie(0),
            _ => Lookup::Unreadable,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_record_is_a_zombie_only_when_the_short_record_says_so() {
        let eperm = || Err(libc::EPERM);
        assert_eq!(classify(eperm(), || Ok(true)), Lookup::Zombie(0));
        assert_eq!(classify(eperm(), || Ok(false)), Lookup::Unreadable);
        assert_eq!(classify(eperm(), || Err(libc::EPERM)), Lookup::Unreadable);
        assert_eq!(classify(eperm(), || Err(libc::ESRCH)), Lookup::Unreadable);
        assert_eq!(classify(Err(libc::ESRCH), || Ok(true)), Lookup::Gone);
    }

    #[test]
    fn short_record_reads_another_users_process_and_a_zombie() {
        let _s = crate::unix::tests::serial();
        // pid 1 belongs to root: the full record is refused, the short one is not.
        assert_eq!(sys::bsd_info(1).map(|i| i.pid), Err(libc::EPERM));
        assert_eq!(sys::short_zombie(1), Ok(false));
        assert_eq!(lookup(1), Lookup::Unreadable);
        let mut child =
            crate::with_spawn_lock(|| std::process::Command::new("/usr/bin/true").spawn()).unwrap();
        let pid = child.id();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while super::super::child::waitid_nowait(pid).unwrap().is_none() {
            assert!(std::time::Instant::now() < end);
            std::thread::yield_now();
        }
        assert_eq!(sys::short_zombie(pid), Ok(true));
        child.wait().unwrap();
        assert_eq!(sys::short_zombie(pid), Err(libc::ESRCH));
    }
}
