//! The leader process (`docs/design/pty-unix.md` §2.4): ownership of its wait status, exit
//! observation without reaping, reaping in `Drop`, and the tree kill over the real OS.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, WaitOptions};

use super::kill::{self, KillState, Leader, Os, Sig};
use super::sys;
use super::table::{self, Lookup};
use crate::backend::{ChildExit, KillReport, ProcessIdentity, PtyChild};

/// The step of `wait_timeout`'s polling.
const WAIT_STEP: Duration = Duration::from_millis(10);

/// What the ownership check needs from the OS; tests script it.
pub(crate) trait ChildOs {
    /// `waitid(P_PID, pid, WEXITED | WNOHANG | WNOWAIT)`.
    fn waitid(&self, pid: u32) -> io::Result<Option<ChildExit>>;
    fn lookup(&self, pid: u32) -> Lookup;
    fn now(&self) -> Instant;
}

pub(crate) struct RealChildOs;

impl ChildOs for RealChildOs {
    fn waitid(&self, pid: u32) -> io::Result<Option<ChildExit>> {
        waitid_nowait(pid)
    }

    fn lookup(&self, pid: u32) -> Lookup {
        table::lookup(pid)
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

fn pid_of(pid: u32) -> io::Result<Pid> {
    i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::from_raw_os_error(libc::ESRCH))
}

pub(crate) fn waitid_nowait(pid: u32) -> io::Result<Option<ChildExit>> {
    let status = rustix::process::waitid(
        WaitId::Pid(pid_of(pid)?),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
    )?;
    Ok(status.map(|s| {
        if s.exited() {
            ChildExit {
                code: s.exit_status(),
                signal: None,
            }
        } else {
            ChildExit {
                code: None,
                signal: s.terminating_signal(),
            }
        }
    }))
}

/// Ownership of the leader's wait status. `lost` is never cleared.
pub(crate) struct Ownership {
    pub(crate) pid: u32,
    pub(crate) start: Option<u64>,
    lost: AtomicBool,
}

impl Ownership {
    pub(crate) fn new(pid: u32, start: Option<u64>) -> Ownership {
        Ownership {
            pid,
            start,
            lost: AtomicBool::new(false),
        }
    }

    pub(crate) fn lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    pub(crate) fn mark_lost(&self) {
        self.lost.store(true, Ordering::Release);
    }

    /// The fresh check of §2.4: `lost` short-circuits with no call; a passed deadline is
    /// not owned for this call; `ECHILD`, or another start at the pid, sets `lost`.
    pub(crate) fn owned(&self, os: &impl ChildOs, deadline: Option<Instant>) -> bool {
        self.check(os, deadline).0
    }

    /// [`Ownership::owned`], with the leader's record when the check read one, so the
    /// kill engine applies that read like any other (§2.5).
    pub(crate) fn check(
        &self,
        os: &impl ChildOs,
        deadline: Option<Instant>,
    ) -> (bool, Option<Lookup>) {
        if self.lost() {
            return (false, None);
        }
        if deadline.is_some_and(|d| os.now() >= d) {
            return (false, None);
        }
        match os.waitid(self.pid) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::ECHILD) => {
                self.mark_lost();
                return (false, None);
            }
            Err(_) => return (false, None),
        }
        let Some(start) = self.start else {
            return (true, None);
        };
        if deadline.is_some_and(|d| os.now() >= d) {
            return (false, None);
        }
        let r = os.lookup(self.pid);
        let owned = match r {
            Lookup::Live(p) if p.start == start => true,
            Lookup::Zombie(s) if s == start => true,
            _ => {
                self.mark_lost();
                false
            }
        };
        (owned, Some(r))
    }
}

pub(crate) struct UnixChild {
    own: Ownership,
    identity: ProcessIdentity,
    exit: OnceLock<ChildExit>,
    kill: Mutex<KillState>,
    me: u32,
    /// Every kill sees an expired deadline (tests: the zero-budget teardown).
    #[cfg(test)]
    pub(crate) kill_expired: AtomicBool,
}

impl UnixChild {
    pub(crate) fn new(pid: u32, start: Option<u64>) -> UnixChild {
        UnixChild {
            own: Ownership::new(pid, start),
            identity: ProcessIdentity {
                pid,
                start_time: start.map(|s| s.to_string()).unwrap_or_default(),
                group: pid.to_string(),
            },
            exit: OnceLock::new(),
            kill: Mutex::new(KillState::default()),
            me: std::process::id(),
            #[cfg(test)]
            kill_expired: AtomicBool::new(false),
        }
    }

    /// Whether a `kill_tree` call holds the kill lock (tests: CL-11).
    #[cfg(test)]
    pub(crate) fn kill_lock_held(&self) -> bool {
        matches!(
            self.kill.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        )
    }
}

impl PtyChild for UnixChild {
    fn identity(&self) -> &ProcessIdentity {
        &self.identity
    }

    fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<ChildExit>> {
        if let Some(e) = self.exit.get() {
            return Ok(Some(*e));
        }
        let end = Instant::now() + timeout;
        loop {
            match waitid_nowait(self.own.pid) {
                Ok(Some(e)) => return Ok(Some(*self.exit.get_or_init(|| e))),
                Ok(None) => {}
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
                Err(e) if e.raw_os_error() == Some(libc::ECHILD) => {
                    self.own.mark_lost();
                    return Err(io::Error::other("child reaped outside the backend"));
                }
                Err(e) => return Err(e),
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            std::thread::sleep(WAIT_STEP.min(left));
        }
    }

    fn kill_tree(&self, extra: &[ProcessIdentity], deadline: Instant) -> KillReport {
        #[cfg(test)]
        super::trace::record("kill_tree");
        #[cfg(test)]
        let deadline = if self.kill_expired.load(Ordering::Acquire) {
            Instant::now()
        } else {
            deadline
        };
        let leader = Leader {
            pid: self.own.pid,
            start: self.own.start,
            me: self.me,
        };
        kill::kill_tree(
            &RealOs { own: &self.own },
            leader,
            extra,
            deadline,
            &self.kill,
        )
    }
}

impl Drop for UnixChild {
    fn drop(&mut self) {
        if self.own.owned(&RealChildOs, None)
            && let Ok(pid) = pid_of(self.own.pid)
        {
            #[cfg(test)]
            super::trace::record("reap");
            // Reaps an exited leader; returns at once for a live one, which stays unreaped.
            let _ = rustix::process::waitpid(Some(pid), WaitOptions::NOHANG);
        }
    }
}

/// The kill engine over the real OS.
struct RealOs<'a> {
    own: &'a Ownership,
}

fn signal_of(sig: Sig) -> Signal {
    match sig {
        Sig::Hup => Signal::HUP,
        Sig::Term => Signal::TERM,
        Sig::Kill => Signal::KILL,
    }
}

impl Os for RealOs<'_> {
    fn owned(&self, deadline: Instant) -> (bool, Option<Lookup>) {
        self.own.check(&RealChildOs, Some(deadline))
    }

    fn lost(&self) -> bool {
        self.own.lost()
    }

    fn pid_count(&self) -> io::Result<usize> {
        sys::pid_count()
    }

    fn pid_list(&self, capacity: usize) -> io::Result<(Vec<u32>, bool)> {
        sys::pid_list(capacity)
    }

    fn lookup(&self, pid: u32) -> Lookup {
        table::lookup(pid)
    }

    fn getsid(&self, pid: u32) -> Result<u32, i32> {
        let p = pid_of(pid).map_err(|_| libc::ESRCH)?;
        match rustix::process::getsid(Some(p)) {
            Ok(sid) => u32::try_from(sid.as_raw_pid()).map_err(|_| libc::EINVAL),
            Err(e) => Err(e.raw_os_error()),
        }
    }

    fn killpg(&self, pgid: u32, sig: Sig) {
        if let Ok(p) = pid_of(pgid) {
            let _ = rustix::process::kill_process_group(p, signal_of(sig));
        }
    }

    fn kill(&self, pid: u32, sig: Sig) {
        if let Ok(p) = pid_of(pid) {
            let _ = rustix::process::kill_process(p, signal_of(sig));
        }
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

#[cfg(test)]
mod tests {
    //! The real `owned()` over a scripted `ChildOs`.

    use std::cell::{Cell, RefCell};

    use super::*;
    use crate::unix::table::Proc;

    const L: u32 = 100;
    const START: u64 = 1_000;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Call {
        Waitid,
        Lookup,
        Now,
    }

    struct Script {
        base: Instant,
        now: Cell<Duration>,
        waitid: RefCell<Box<dyn Fn() -> io::Result<Option<ChildExit>>>>,
        lookup: Cell<Lookup>,
        calls: RefCell<Vec<Call>>,
        /// Clock advance of each `waitid`.
        waitid_cost: Cell<Duration>,
    }

    impl Script {
        fn new() -> Script {
            Script {
                base: Instant::now(),
                now: Cell::new(Duration::ZERO),
                waitid: RefCell::new(Box::new(|| Ok(None))),
                lookup: Cell::new(live(START)),
                calls: RefCell::new(Vec::new()),
                waitid_cost: Cell::new(Duration::ZERO),
            }
        }

        fn deadline(&self, ms: u64) -> Option<Instant> {
            Some(self.base + Duration::from_millis(ms))
        }
    }

    fn live(start: u64) -> Lookup {
        Lookup::Live(Proc {
            pid: L,
            ppid: 1,
            pgid: L,
            start,
        })
    }

    impl ChildOs for Script {
        fn waitid(&self, _pid: u32) -> io::Result<Option<ChildExit>> {
            self.calls.borrow_mut().push(Call::Waitid);
            self.now.set(self.now.get() + self.waitid_cost.get());
            (self.waitid.borrow())()
        }

        fn lookup(&self, _pid: u32) -> Lookup {
            self.calls.borrow_mut().push(Call::Lookup);
            self.lookup.get()
        }

        fn now(&self) -> Instant {
            self.calls.borrow_mut().push(Call::Now);
            self.base + self.now.get()
        }
    }

    #[test]
    fn matching_generation_is_owned() {
        let os = Script::new();
        let own = Ownership::new(L, Some(START));
        assert!(own.owned(&os, os.deadline(1_000)));
        os.lookup.set(Lookup::Zombie(START));
        assert!(own.owned(&os, os.deadline(1_000)));
        assert!(!own.lost());
    }

    #[test]
    fn lost_makes_no_wait_call() {
        let os = Script::new();
        let own = Ownership::new(L, Some(START));
        own.mark_lost();
        assert!(!own.owned(&os, os.deadline(1_000)));
        assert!(!own.owned(&os, None));
        assert!(os.calls.borrow().is_empty(), "{:?}", os.calls.borrow());
    }

    #[test]
    fn echild_sets_lost() {
        let os = Script::new();
        *os.waitid.borrow_mut() = Box::new(|| Err(io::Error::from_raw_os_error(libc::ECHILD)));
        let own = Ownership::new(L, Some(START));
        assert!(!own.owned(&os, os.deadline(1_000)));
        assert!(own.lost());
    }

    #[test]
    fn waitable_replacement_is_rejected() {
        let os = Script::new();
        // `waitid` succeeds: some child of ours has pid L, but it started later.
        os.lookup.set(live(START + 1));
        let own = Ownership::new(L, Some(START));
        assert!(!own.owned(&os, os.deadline(1_000)));
        assert!(own.lost());
        for gone in [Lookup::Gone, Lookup::Unreadable, Lookup::Zombie(START + 1)] {
            let own = Ownership::new(L, Some(START));
            os.lookup.set(gone);
            assert!(!own.owned(&os, os.deadline(1_000)), "{gone:?}");
            assert!(own.lost(), "{gone:?}");
        }
    }

    #[test]
    fn expired_deadline_is_not_owned_and_keeps_lost_clear() {
        let os = Script::new();
        let own = Ownership::new(L, Some(START));
        os.now.set(Duration::from_millis(1_000));
        assert!(!own.owned(&os, os.deadline(1_000)));
        assert_eq!(*os.calls.borrow(), vec![Call::Now]);
        assert!(!own.lost());
        // The deadline passes inside `waitid`: no lookup follows.
        let os = Script::new();
        os.waitid_cost.set(Duration::from_millis(2_000));
        assert!(!own.owned(&os, os.deadline(1_000)));
        assert_eq!(*os.calls.borrow(), vec![Call::Now, Call::Waitid, Call::Now]);
        assert!(!own.lost());
        assert!(own.owned(&Script::new(), None));
    }
}
