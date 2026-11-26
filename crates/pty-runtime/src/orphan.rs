//! The unowned kill of a crashed daemon's recorded processes
//! (`docs/design/supervisor-recovery.md` §2.2): each recorded identity is signalled only by
//! verified identity, with no process list, group, session, or job call, and what was observed
//! of it is reported per identity. The daemon owns no old session or job after a crash, so this
//! loop is apart from the owned kill engines (`unix::kill`, `windows::kill`).
//!
//! The macOS loop runs over [`OrphanOs`] (`bsd_info`, `short_zombie`, `kill`, `now`, `sleep`),
//! the Windows loop over `windows::kill::ProcessOs`; the unit tests script both and the clock.

use std::time::{Duration, Instant};

use crate::backend::ProcessIdentity;

/// What a check observed of one recorded identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fate {
    /// A read showed the pid gone or a zombie, or its handle was signalled.
    Gone,
    /// The pid holds a process with another start: the recorded one ended.
    Replaced,
    /// The last read before the deadline showed the recorded start, or the last wait on its
    /// handle returned unsignalled.
    Alive,
    /// The last read was refused (or cut between its two calls).
    Unreadable,
    /// No read or wait was made before the deadline.
    NotRead,
    /// The last wait on its handle failed.
    WaitFailed,
    /// Never read or signalled: pid 0, the daemon's own, 1 (macOS), above `i32::MAX`
    /// (macOS), 4 (Windows), or a start that does not parse.
    Invalid,
}

/// One fate per identity, in input order, identical identities collapsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanReport {
    pub fates: Vec<(ProcessIdentity, Fate)>,
}

/// The longest step between two reads (macOS) or one wait on one handle (Windows).
const STEP: Duration = Duration::from_millis(25);

/// Signals each of `identities` by verified identity until `deadline` and reports what it
/// observed. No native call starts at or after `deadline`; one in flight at it may finish.
#[must_use]
pub fn kill_recorded(identities: &[ProcessIdentity], deadline: Instant) -> OrphanReport {
    #[cfg(target_os = "macos")]
    {
        unix::kill_recorded(
            &unix::RealOrphanOs,
            identities,
            std::process::id(),
            deadline,
        )
    }
    #[cfg(windows)]
    {
        windows::kill_recorded(
            &crate::windows::child::RealProcessOs,
            identities,
            std::process::id(),
            deadline,
        )
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        // No native backend: nothing is read or signalled.
        let me = std::process::id();
        let (fates, _) = prepare(identities, |id| accept(id, me, 0, u32::MAX));
        OrphanReport { fates }
    }
}

/// The parsed start of a valid identity: not pid 0, `me`, or `reserved`, at most `max_pid`,
/// and a start that parses as `u64` (empty included as invalid).
fn accept(id: &ProcessIdentity, me: u32, reserved: u32, max_pid: u32) -> Option<u64> {
    if id.pid == 0 || id.pid == me || id.pid == reserved || id.pid > max_pid {
        return None;
    }
    id.start_time.parse::<u64>().ok()
}

/// The identities in input order without repeats, each `Invalid` when `valid` refuses it and
/// `NotRead` until observed otherwise; and the indices of the valid ones with their parsed
/// start.
#[allow(
    clippy::type_complexity,
    reason = "a local pair: the report's rows and the work list"
)]
fn prepare(
    identities: &[ProcessIdentity],
    valid: impl Fn(&ProcessIdentity) -> Option<u64>,
) -> (Vec<(ProcessIdentity, Fate)>, Vec<(usize, u32, u64)>) {
    let mut rows: Vec<(ProcessIdentity, Fate)> = Vec::new();
    let mut work = Vec::new();
    for id in identities {
        if rows.iter().any(|(r, _)| r == id) {
            continue;
        }
        match valid(id) {
            Some(start) => {
                work.push((rows.len(), id.pid, start));
                rows.push((id.clone(), Fate::NotRead));
            }
            None => rows.push((id.clone(), Fate::Invalid)),
        }
    }
    (rows, work)
}

#[cfg(target_os = "macos")]
pub(crate) mod unix {
    //! The macOS loop: rounds of HUP, TERM, KILL, each signal only right after a read of the
    //! pid showed the recorded start.

    use std::time::{Duration, Instant};

    use super::{Fate, OrphanReport, STEP, accept, prepare};
    use crate::backend::ProcessIdentity;
    use crate::unix::child::{pid_of, signal_of};
    use crate::unix::kill::Sig;
    use crate::unix::sys::BsdInfo;
    use crate::unix::table::{Lookup, classify};

    /// The OS calls of the macOS loop, each one native call. It has no list, `getsid`, or
    /// group call: the loop reaches only the recorded pids.
    pub(crate) trait OrphanOs {
        fn bsd_info(&self, pid: u32) -> Result<BsdInfo, i32>;
        fn short_zombie(&self, pid: u32) -> Result<bool, i32>;
        /// Errors are ignored: the next read observes the target.
        fn kill(&self, pid: u32, sig: Sig);
        fn now(&self) -> Instant;
        fn sleep(&self, d: Duration);
    }

    pub(crate) struct RealOrphanOs;

    impl OrphanOs for RealOrphanOs {
        fn bsd_info(&self, pid: u32) -> Result<BsdInfo, i32> {
            crate::unix::sys::bsd_info(pid)
        }

        fn short_zombie(&self, pid: u32) -> Result<bool, i32> {
            crate::unix::sys::short_zombie(pid)
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

    struct Target {
        row: usize,
        pid: u32,
        start: u64,
        /// What the last read showed: `NotRead` before any, `Alive` (the recorded start
        /// live) or `Unreadable` while unresolved, `Gone` or `Replaced` once resolved.
        fate: Fate,
    }

    impl Target {
        fn resolved(&self) -> bool {
            matches!(self.fate, Fate::Gone | Fate::Replaced)
        }
    }

    struct Loop<'a, O: OrphanOs> {
        os: &'a O,
        deadline: Instant,
    }

    impl<O: OrphanOs> Loop<'_, O> {
        fn expired(&self) -> bool {
            self.os.now() >= self.deadline
        }

        /// One read: `None` when the deadline came before its first call; a read cut between
        /// its two calls counts as refused.
        fn read(&self, pid: u32) -> Option<Lookup> {
            if self.expired() {
                return None;
            }
            let info = self.os.bsd_info(pid);
            Some(classify(info, || {
                if self.expired() {
                    Err(libc::ETIMEDOUT)
                } else {
                    self.os.short_zombie(pid)
                }
            }))
        }

        /// Reads `t` and records what the read showed; whether it showed the recorded start
        /// live. `None`: the deadline came first.
        fn observe(&self, t: &mut Target) -> Option<bool> {
            let r = self.read(t.pid)?;
            match r {
                Lookup::Gone | Lookup::Zombie(_) => t.fate = Fate::Gone,
                Lookup::Live(p) if p.start == t.start => {
                    t.fate = Fate::Alive;
                    return Some(true);
                }
                Lookup::Live(_) => t.fate = Fate::Replaced,
                Lookup::Unreadable => t.fate = Fate::Unreadable,
            }
            Some(false)
        }

        /// A round: each unresolved target read, and signalled only when that read showed the
        /// recorded start, with only the clock check in between.
        fn round(&self, targets: &mut [Target], sig: Sig) {
            for t in targets.iter_mut().filter(|t| !t.resolved()) {
                match self.observe(t) {
                    None => return,
                    Some(true) => {
                        if self.expired() {
                            return;
                        }
                        self.os.kill(t.pid, sig);
                    }
                    Some(false) => {}
                }
            }
        }

        /// Reads every unresolved target every `STEP` until `until` (capped at the deadline)
        /// or until none is unresolved.
        fn poll(&self, targets: &mut [Target], until: Instant) {
            let until = until.min(self.deadline);
            loop {
                if targets.iter().all(Target::resolved) {
                    return;
                }
                let now = self.os.now();
                if now >= until {
                    return;
                }
                self.os.sleep(STEP.min(until - now));
                for t in targets.iter_mut().filter(|t| !t.resolved()) {
                    if self.observe(t).is_none() {
                        return;
                    }
                }
            }
        }
    }

    pub(crate) fn kill_recorded<O: OrphanOs>(
        os: &O,
        identities: &[ProcessIdentity],
        me: u32,
        deadline: Instant,
    ) -> OrphanReport {
        let max = u32::try_from(i32::MAX).unwrap_or(u32::MAX);
        let (mut rows, work) = prepare(identities, |id| accept(id, me, 1, max));
        let mut targets: Vec<Target> = work
            .into_iter()
            .map(|(row, pid, start)| Target {
                row,
                pid,
                start,
                fate: Fate::NotRead,
            })
            .collect();
        let l = Loop { os, deadline };
        if !targets.is_empty() {
            let t0 = os.now();
            let b = deadline.saturating_duration_since(t0);
            if b > Duration::ZERO {
                if b > Duration::from_millis(100) {
                    l.round(&mut targets, Sig::Hup);
                    l.poll(&mut targets, t0 + (b / 4).min(Duration::from_millis(500)));
                    l.round(&mut targets, Sig::Term);
                    l.poll(&mut targets, t0 + (b / 2).min(Duration::from_secs(2)));
                }
                l.round(&mut targets, Sig::Kill);
                l.poll(&mut targets, deadline);
            }
        }
        for t in &targets {
            rows[t.row].1 = t.fate;
        }
        OrphanReport { fates: rows }
    }
}

#[cfg(any(windows, test))]
pub(crate) mod windows {
    //! The Windows loop: one verified open per identity, every held handle terminated before
    //! any wait, then rounds of clipped waits.

    use std::time::Instant;

    use super::{Fate, OrphanReport, STEP, accept, prepare};
    use crate::backend::ProcessIdentity;
    use crate::windows::kill::{Opened, ProcessOs, budget, expired, open_verified};

    /// The System process.
    const SYSTEM_PID: u32 = 4;

    /// A held handle and the last wait's result on it.
    struct HeldTarget<H> {
        row: usize,
        handle: H,
        /// `NotRead` before any wait, `Alive` after a wait that timed out, and `Gone` or
        /// `WaitFailed` once a wait ended it.
        fate: Fate,
    }

    impl<H> HeldTarget<H> {
        /// A wait ended (signalled or failed): no further wait on this handle.
        fn done(&self) -> bool {
            matches!(self.fate, Fate::Gone | Fate::WaitFailed)
        }
    }

    pub(crate) fn kill_recorded<O: ProcessOs>(
        os: &O,
        identities: &[ProcessIdentity],
        me: u32,
        deadline: Instant,
    ) -> OrphanReport {
        let (mut rows, work) = prepare(identities, |id| accept(id, me, SYSTEM_PID, u32::MAX));
        let mut held: Vec<HeldTarget<O::Held>> = Vec::new();
        for (row, pid, start) in work {
            // A cut open leaves it and every later row `NotRead`.
            rows[row].1 = match open_verified(os, pid, start, deadline) {
                Opened::Held(handle) => {
                    held.push(HeldTarget {
                        row,
                        handle,
                        fate: Fate::NotRead,
                    });
                    continue;
                }
                Opened::Gone => Fate::Gone,
                Opened::Stale => Fate::Replaced,
                Opened::Unverifiable => Fate::Unreadable,
                Opened::Cut => break,
            };
        }
        for t in &held {
            if expired(os, deadline) {
                break;
            }
            let _ = os.terminate(&t.handle);
        }
        while held.iter().any(|t| !t.done()) {
            let mut waited = false;
            for t in held.iter_mut().filter(|t| !t.done()) {
                let Some(wait) = budget(os, deadline, STEP) else {
                    break;
                };
                waited = true;
                t.fate = match os.signalled(&t.handle, wait) {
                    Ok(true) => Fate::Gone,
                    Ok(false) => Fate::Alive,
                    Err(_) => Fate::WaitFailed,
                };
            }
            if !waited {
                break;
            }
        }
        for t in held {
            rows[t.row].1 = t.fate;
        }
        OrphanReport { fates: rows }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod unix_tests;
#[cfg(test)]
mod windows_tests;
