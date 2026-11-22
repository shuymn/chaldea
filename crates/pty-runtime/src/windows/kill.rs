//! The Windows tree kill (`docs/design/pty-windows.md` §2.5): job termination plus the
//! verified recorded descendants of this call, death proven from the job's own count and
//! the handles held. It runs over a [`JobOs`], so the unit tests drive it with a synthetic
//! process table and clock on every OS.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use crate::backend::{KillReport, ProcessIdentity};

pub(crate) const METHOD: &str = "windows_job";
pub(crate) const METHOD_INCOMPLETE: &str = "windows_job_incomplete";
pub(crate) const METHOD_BUSY: &str = "windows_busy";

/// The longest sleep: lock polls and rounds without progress.
const STEP: Duration = Duration::from_millis(10);
/// The longest wait on one handle in a round whose job count read 0.
const PROOF_WAIT: Duration = Duration::from_millis(100);
const LIST_FIRST: usize = 64;
const LIST_MAX: usize = 4096;

/// Win32 codes the engine distinguishes.
const ERROR_ACCESS_DENIED: i32 = 5;
pub(crate) const ERROR_INVALID_PARAMETER: i32 = 87;
/// The System process.
const SYSTEM_PID: u32 = 4;

/// The rights of an extra's handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// `PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE`.
    Kill,
    /// `PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE`: waited for, not killed.
    Wait,
}

/// One pid-list query: complete, or the buffer was too small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PidList {
    Complete(Vec<u32>),
    TooSmall,
}

/// What the kill engine needs from the OS; each method is one OS call.
pub(crate) trait JobOs {
    type Held;
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
    fn terminate_job(&self) -> io::Result<()>;
    fn active_processes(&self) -> io::Result<u32>;
    fn job_pids(&self, capacity: usize) -> io::Result<PidList>;
    fn open_process(&self, pid: u32, access: Access) -> io::Result<Self::Held>;
    fn creation_time(&self, h: &Self::Held) -> io::Result<u64>;
    fn in_job(&self, h: &Self::Held) -> io::Result<bool>;
    /// A zero wait polls; the engine passes whole milliseconds.
    fn signalled(&self, h: &Self::Held, wait: Duration) -> io::Result<bool>;
    /// One termination call; its result is never proof (the proof rounds observe the
    /// handle's signal).
    fn terminate(&self, h: &Self::Held) -> io::Result<()>;
}

/// The leader: its identity and the handle the child holds.
pub(crate) struct Leader<'a, H> {
    pub(crate) identity: &'a ProcessIdentity,
    pub(crate) handle: &'a H,
    /// The daemon's own pid, never opened.
    pub(crate) me: u32,
}

/// Verified extras' handles: the only state kept across calls.
pub(crate) struct KillState<H> {
    held: HashMap<(u32, u64), H>,
}

impl<H> Default for KillState<H> {
    fn default() -> Self {
        KillState {
            held: HashMap::new(),
        }
    }
}

impl<H> KillState<H> {
    #[cfg(test)]
    pub(crate) fn held_count(&self) -> usize {
        self.held.len()
    }
}

/// The result of one `open_verified`.
pub(crate) enum Opened<H> {
    Held(H),
    Gone,
    Stale,
    Unverifiable,
    /// The deadline came between two of its calls.
    Cut,
}

fn code(e: &io::Error) -> Option<i32> {
    e.raw_os_error()
}

fn expired(os: &impl JobOs, deadline: Instant) -> bool {
    os.now() >= deadline
}

/// `min(STEP, deadline − now)`; nothing when the deadline has passed.
fn pace(os: &impl JobOs, deadline: Instant) {
    let left = deadline.saturating_duration_since(os.now());
    let d = STEP.min(left);
    if !d.is_zero() {
        os.sleep(d);
    }
}

/// A handle wait of at most `cap` within the deadline, from one clock read, floored to
/// whole milliseconds: `None` when the deadline has passed (no call), a zero wait (a poll)
/// when less than 1 ms remains.
fn budget(os: &impl JobOs, deadline: Instant, cap: Duration) -> Option<Duration> {
    let now = os.now();
    if now >= deadline {
        return None;
    }
    let left = (deadline - now).min(cap);
    Some(Duration::from_millis(
        u64::try_from(left.as_millis()).unwrap_or(u64::MAX),
    ))
}

/// Opens `pid` and checks its creation time on that same handle, checking the clock before
/// each call.
pub(crate) fn open_verified<O: JobOs>(
    os: &O,
    pid: u32,
    start: u64,
    deadline: Instant,
) -> Opened<O::Held> {
    if expired(os, deadline) {
        return Opened::Cut;
    }
    let h = match os.open_process(pid, Access::Kill) {
        Ok(h) => h,
        Err(e) if code(&e) == Some(ERROR_ACCESS_DENIED) => {
            if expired(os, deadline) {
                return Opened::Cut;
            }
            match os.open_process(pid, Access::Wait) {
                Ok(h) => h,
                Err(e) if code(&e) == Some(ERROR_INVALID_PARAMETER) => return Opened::Gone,
                Err(_) => return Opened::Unverifiable,
            }
        }
        Err(e) if code(&e) == Some(ERROR_INVALID_PARAMETER) => return Opened::Gone,
        Err(_) => return Opened::Unverifiable,
    };
    if expired(os, deadline) {
        return Opened::Cut;
    }
    match os.creation_time(&h) {
        Ok(t) if t == start => Opened::Held(h),
        Ok(_) => Opened::Stale,
        Err(_) => Opened::Unverifiable,
    }
}

/// A listed pid's identity, only if its handle confirms membership (the pid may name a
/// new process); an empty start when it cannot be read.
pub(crate) fn member_identity<O: JobOs>(
    os: &O,
    pid: u32,
    group: &str,
    deadline: Instant,
) -> Option<ProcessIdentity> {
    if expired(os, deadline) {
        return None;
    }
    let h = os.open_process(pid, Access::Wait).ok()?;
    if expired(os, deadline) || !os.in_job(&h).ok()? {
        return None;
    }
    let start = if expired(os, deadline) {
        String::new()
    } else {
        os.creation_time(&h)
            .map(|t| t.to_string())
            .unwrap_or_default()
    };
    Some(ProcessIdentity {
        pid,
        start_time: start,
        group: group.to_owned(),
    })
}

/// The job's pids, doubling the capacity on `TooSmall` up to `LIST_MAX`; `None` on an
/// error, an overflow, or the deadline.
pub(crate) fn list_members<O: JobOs>(os: &O, deadline: Instant) -> Option<Vec<u32>> {
    let mut capacity = LIST_FIRST;
    loop {
        if expired(os, deadline) {
            return None;
        }
        match os.job_pids(capacity).ok()? {
            PidList::Complete(v) => return Some(v),
            PidList::TooSmall => {
                capacity *= 2;
                if capacity > LIST_MAX {
                    return None;
                }
            }
        }
    }
}

/// The supplied `extra` of one call, split.
struct Supplied {
    /// Well-formed identities, deduplicated, in the order passed.
    wanted: Vec<(u32, u64, ProcessIdentity)>,
    /// Malformed entries, deduplicated, in the order passed.
    malformed: Vec<ProcessIdentity>,
}

fn split(extra: &[ProcessIdentity], me: u32) -> Supplied {
    let mut seen = HashSet::new();
    let mut s = Supplied {
        wanted: Vec::new(),
        malformed: Vec::new(),
    };
    for id in extra {
        if !seen.insert(id.clone()) {
            continue;
        }
        let start = id.start_time.parse::<u64>().ok();
        match start {
            Some(t) if id.pid != 0 && id.pid != SYSTEM_PID && id.pid != me => {
                s.wanted.push((id.pid, t, id.clone()));
            }
            _ => s.malformed.push(id.clone()),
        }
    }
    s
}

fn held_identity(pid: u32, start: u64) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start_time: start.to_string(),
        group: String::new(),
    }
}

/// One entry per pid, in group order; inside the unresolved group the smallest start wins.
fn dedup(groups: [Vec<ProcessIdentity>; 5]) -> Vec<ProcessIdentity> {
    let [leader, held, members, mut unresolved, malformed] = groups;
    unresolved.sort_by_key(|i| (i.pid, i.start_time.parse::<u64>().unwrap_or(u64::MAX)));
    let mut pids = HashSet::new();
    let mut out = Vec::new();
    for id in leader
        .into_iter()
        .chain(held)
        .chain(members)
        .chain(unresolved)
        .chain(malformed)
    {
        if pids.insert(id.pid) {
            out.push(id);
        }
    }
    out
}

fn report(method: &str, tree_dead: bool, survivors: Vec<ProcessIdentity>) -> KillReport {
    KillReport {
        method: method.to_owned(),
        tree_dead,
        survivors,
    }
}

fn lock_state<H>(m: &Mutex<KillState<H>>) -> Option<MutexGuard<'_, KillState<H>>> {
    match m.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// The steps of §2.5.
pub(crate) fn kill_tree<O: JobOs>(
    os: &O,
    leader: &Leader<'_, O::Held>,
    extra: &[ProcessIdentity],
    deadline: Instant,
    state: &Mutex<KillState<O::Held>>,
) -> KillReport {
    let supplied = split(extra, leader.me);
    let supplied_ids = || {
        supplied
            .wanted
            .iter()
            .map(|w| w.2.clone())
            .collect::<Vec<_>>()
    };
    let busy = || {
        report(
            METHOD_BUSY,
            false,
            dedup([
                vec![leader.identity.clone()],
                Vec::new(),
                Vec::new(),
                supplied_ids(),
                supplied.malformed.clone(),
            ]),
        )
    };

    // 1. Lock.
    let mut st = if expired(os, deadline) {
        let Some(st) = lock_state(state) else {
            return busy();
        };
        let held = st.held.keys().map(|&(p, t)| held_identity(p, t)).collect();
        return report(
            METHOD_INCOMPLETE,
            false,
            dedup([
                vec![leader.identity.clone()],
                held,
                Vec::new(),
                supplied_ids(),
                supplied.malformed.clone(),
            ]),
        );
    } else {
        loop {
            if let Some(st) = lock_state(state) {
                break st;
            }
            if expired(os, deadline) {
                return busy();
            }
            pace(os, deadline);
        }
    };

    let mut call = Call {
        os,
        deadline,
        leader,
        resolved: HashSet::new(),
        leader_dead: false,
        last_count: None,
        snapshot: None,
        snapshot_taken: false,
        malformed: !supplied.malformed.is_empty(),
    };
    let wanted: Vec<(u32, u64)> = supplied.wanted.iter().map(|w| (w.0, w.1)).collect();
    for w in &wanted {
        if st.held.contains_key(w) {
            call.resolved.insert(*w);
        }
    }

    // 2. Open.
    for w in &wanted {
        if !call.resolved.contains(w) {
            call.open(&mut st, *w);
        }
    }

    // 3. Opening check: an already dead tree is proven with no termination call.
    if call.opening_check(&mut st, &wanted) {
        return report(METHOD, true, Vec::new());
    }

    // 4. Terminate. Failures are not recorded as proof either way: step 5 reads.
    if !call.expired() {
        let _ = os.terminate_job();
    }
    call.terminate_held(&st);

    // 5. Prove, in rounds.
    while !call.expired() {
        let mut progress = false;
        // 5.1 Extras pass.
        for w in &wanted {
            if !call.resolved.contains(w) && call.open(&mut st, *w) {
                progress = true;
            }
        }
        // One termination per held handle, newly opened ones included.
        call.terminate_held(&st);
        // 5.2 Job.
        if call.expired() {
            break;
        }
        let count = os.active_processes();
        call.last_count = count.as_ref().ok().copied();
        match count {
            Ok(0) => {
                call.snapshot = None;
                if call.wait_handles(&mut st) {
                    progress = true;
                }
                if call.proven(&st, &wanted) {
                    return report(METHOD, true, Vec::new());
                }
            }
            Ok(_) => {
                if !call.expired() {
                    let _ = os.terminate_job();
                }
                // 5.4 Member snapshot, once, in the first round with a nonzero count.
                if !call.snapshot_taken {
                    call.snapshot_taken = true;
                    let group = format!("job:{}", leader.identity.pid);
                    call.snapshot = list_members(os, deadline).map(|pids| {
                        pids.into_iter()
                            .filter_map(|p| member_identity(os, p, &group, deadline))
                            .collect()
                    });
                }
            }
            Err(_) => {}
        }
        // 5.3 Pace.
        if !progress {
            pace(os, deadline);
        }
    }

    // 6. At the deadline.
    let method = if call.last_count == Some(0) {
        METHOD
    } else {
        METHOD_INCOMPLETE
    };
    let leader_group = if call.leader_dead {
        Vec::new()
    } else {
        vec![leader.identity.clone()]
    };
    let held = st.held.keys().map(|&(p, t)| held_identity(p, t)).collect();
    let unresolved = supplied
        .wanted
        .iter()
        .filter(|w| !call.resolved.contains(&(w.0, w.1)))
        .map(|w| w.2.clone())
        .collect();
    report(
        method,
        false,
        dedup([
            leader_group,
            held,
            call.snapshot.take().unwrap_or_default(),
            unresolved,
            supplied.malformed,
        ]),
    )
}

struct Call<'a, O: JobOs> {
    os: &'a O,
    deadline: Instant,
    leader: &'a Leader<'a, O::Held>,
    /// Supplied identities resolved in this call (held, gone, or stale).
    resolved: HashSet<(u32, u64)>,
    leader_dead: bool,
    /// The last `ActiveProcesses` read; `None` when it failed or none was made.
    last_count: Option<u32>,
    snapshot: Option<Vec<ProcessIdentity>>,
    snapshot_taken: bool,
    malformed: bool,
}

impl<O: JobOs> Call<'_, O> {
    fn expired(&self) -> bool {
        expired(self.os, self.deadline)
    }

    /// `terminate()` on each held handle while time remains.
    fn terminate_held(&self, st: &KillState<O::Held>) {
        for h in st.held.values() {
            if self.expired() {
                break;
            }
            let _ = self.os.terminate(h);
        }
    }

    /// One `open_verified`; true when it resolved the identity.
    fn open(&mut self, st: &mut KillState<O::Held>, w: (u32, u64)) -> bool {
        match open_verified(self.os, w.0, w.1, self.deadline) {
            Opened::Held(h) => {
                st.held.insert(w, h);
                self.resolved.insert(w);
                true
            }
            Opened::Gone | Opened::Stale => {
                self.resolved.insert(w);
                true
            }
            Opened::Unverifiable | Opened::Cut => false,
        }
    }

    /// Waits on the leader and each held handle under the clock rules; releases each held
    /// handle seen signalled. True when some handle signalled.
    fn wait_handles(&mut self, st: &mut KillState<O::Held>) -> bool {
        let mut any = false;
        if !self.leader_dead {
            let Some(wait) = budget(self.os, self.deadline, PROOF_WAIT) else {
                return any;
            };
            if self.os.signalled(self.leader.handle, wait).unwrap_or(false) {
                self.leader_dead = true;
                any = true;
            }
        }
        let keys: Vec<(u32, u64)> = st.held.keys().copied().collect();
        for k in keys {
            let Some(wait) = budget(self.os, self.deadline, PROOF_WAIT) else {
                break;
            };
            if st
                .held
                .get(&k)
                .is_some_and(|h| self.os.signalled(h, wait).unwrap_or(false))
            {
                st.held.remove(&k);
                any = true;
            }
        }
        any
    }

    fn proven(&self, st: &KillState<O::Held>, wanted: &[(u32, u64)]) -> bool {
        self.leader_dead
            && st.held.is_empty()
            && !self.malformed
            && wanted.iter().all(|w| self.resolved.contains(w))
    }

    /// Step 3: a zero count read, every handle signalled with a zero wait, every supplied
    /// identity resolved, and no malformed entry.
    fn opening_check(&mut self, st: &mut KillState<O::Held>, wanted: &[(u32, u64)]) -> bool {
        if self.malformed || !wanted.iter().all(|w| self.resolved.contains(w)) {
            return false;
        }
        if self.expired() {
            return false;
        }
        let count = self.os.active_processes();
        self.last_count = count.as_ref().ok().copied();
        if count.ok() != Some(0) {
            return false;
        }
        if self.expired() {
            return false;
        }
        if !self
            .os
            .signalled(self.leader.handle, Duration::ZERO)
            .unwrap_or(false)
        {
            return false;
        }
        self.leader_dead = true;
        let keys: Vec<(u32, u64)> = st.held.keys().copied().collect();
        for k in keys {
            if self.expired() {
                return false;
            }
            let signalled = st
                .held
                .get(&k)
                .is_some_and(|h| self.os.signalled(h, Duration::ZERO).unwrap_or(false));
            if !signalled {
                return false;
            }
            st.held.remove(&k);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    //! The real engine and helpers over a scripted process table and clock.

    use std::cell::{Cell, RefCell};

    use super::*;

    const L: u32 = 100;
    const ME: u32 = 7;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum C {
        Sleep(Duration),
        TerminateJob,
        Active,
        JobPids(usize),
        Open(u32, Access),
        Times(u32),
        InJob(u32),
        Signalled(u32, Duration),
        Terminate(u32),
    }

    #[derive(Debug, Clone)]
    struct P {
        start: u64,
        alive: bool,
        in_job: bool,
        /// Survives every termination.
        immortal: bool,
        /// Dies at this clock time.
        dies_at: Option<Duration>,
        deny_kill_access: bool,
        open_error: Option<i32>,
        times_error: bool,
    }

    fn proc(start: u64, in_job: bool) -> P {
        P {
            start,
            alive: true,
            in_job,
            immortal: false,
            dies_at: None,
            deny_kill_access: false,
            open_error: None,
            times_error: false,
        }
    }

    /// A handle: the pid and the start of the process it was opened on, and its rights.
    #[derive(Debug, Clone, Copy)]
    struct H {
        pid: u32,
        start: u64,
        access: Access,
    }

    type Hook = Box<dyn Fn(&C, &Os)>;

    struct Os {
        base: Instant,
        now: Cell<Duration>,
        procs: RefCell<HashMap<u32, P>>,
        calls: RefCell<Vec<C>>,
        count_error: Cell<bool>,
        list_error: Cell<bool>,
        /// `job_pids` answers `TooSmall` below this capacity.
        list_needs: Cell<usize>,
        terminate_job_error: Cell<bool>,
        /// Clock advance of every OS call (not `now`).
        cost: Cell<Duration>,
        hook: RefCell<Option<Hook>>,
    }

    impl Os {
        fn new() -> Os {
            let os = Os {
                base: Instant::now(),
                now: Cell::new(Duration::ZERO),
                procs: RefCell::new(HashMap::new()),
                calls: RefCell::new(Vec::new()),
                count_error: Cell::new(false),
                list_error: Cell::new(false),
                list_needs: Cell::new(0),
                terminate_job_error: Cell::new(false),
                cost: Cell::new(Duration::ZERO),
                hook: RefCell::new(None),
            };
            os.add(L, proc(1_000, true));
            os
        }

        fn add(&self, pid: u32, p: P) {
            self.procs.borrow_mut().insert(pid, p);
        }

        fn set(&self, pid: u32, f: impl FnOnce(&mut P)) {
            f(self.procs.borrow_mut().get_mut(&pid).unwrap());
        }

        fn at(&self, ms: u64) -> Instant {
            self.base + Duration::from_millis(ms)
        }

        fn alive(&self, p: &P) -> bool {
            p.alive && p.dies_at.is_none_or(|t| self.now.get() < t)
        }

        fn live(&self, pid: u32, start: u64) -> bool {
            self.procs
                .borrow()
                .get(&pid)
                .is_some_and(|p| p.start == start && self.alive(p))
        }

        fn call(&self, c: C) {
            self.now.set(self.now.get() + self.cost.get());
            self.calls.borrow_mut().push(c);
            if let Some(h) = self.hook.borrow().as_ref() {
                h(&c, self);
            }
        }

        fn calls(&self) -> Vec<C> {
            self.calls.borrow().clone()
        }

        fn count(&self, f: impl Fn(&C) -> bool) -> usize {
            self.calls.borrow().iter().filter(|c| f(c)).count()
        }

        fn leader_handle(&self) -> H {
            H {
                pid: L,
                start: 1_000,
                access: Access::Kill,
            }
        }

        fn kill(
            &self,
            extra: &[ProcessIdentity],
            deadline: Instant,
            st: &Mutex<KillState<H>>,
        ) -> KillReport {
            let identity = id(L, 1_000);
            let identity = ProcessIdentity {
                group: format!("job:{L}"),
                ..identity
            };
            let h = self.leader_handle();
            let leader = Leader {
                identity: &identity,
                handle: &h,
                me: ME,
            };
            kill_tree(self, &leader, extra, deadline, st)
        }
    }

    fn e(code: i32) -> io::Error {
        io::Error::from_raw_os_error(code)
    }

    impl JobOs for Os {
        type Held = H;

        fn now(&self) -> Instant {
            self.base + self.now.get()
        }

        fn sleep(&self, d: Duration) {
            self.calls.borrow_mut().push(C::Sleep(d));
            self.now.set(self.now.get() + d);
        }

        fn terminate_job(&self) -> io::Result<()> {
            self.call(C::TerminateJob);
            if self.terminate_job_error.get() {
                return Err(e(ERROR_ACCESS_DENIED));
            }
            for p in self.procs.borrow_mut().values_mut() {
                if p.in_job && !p.immortal {
                    p.alive = false;
                }
            }
            Ok(())
        }

        fn active_processes(&self) -> io::Result<u32> {
            self.call(C::Active);
            if self.count_error.get() {
                return Err(e(6));
            }
            let n = self
                .procs
                .borrow()
                .values()
                .filter(|p| p.in_job && self.alive(p))
                .count();
            Ok(u32::try_from(n).unwrap())
        }

        fn job_pids(&self, capacity: usize) -> io::Result<PidList> {
            self.call(C::JobPids(capacity));
            if self.list_error.get() {
                return Err(e(6));
            }
            if capacity < self.list_needs.get() {
                return Ok(PidList::TooSmall);
            }
            let mut v: Vec<u32> = self
                .procs
                .borrow()
                .iter()
                .filter(|(_, p)| p.in_job && self.alive(p))
                .map(|(k, _)| *k)
                .collect();
            v.sort_unstable();
            Ok(PidList::Complete(v))
        }

        fn open_process(&self, pid: u32, access: Access) -> io::Result<H> {
            self.call(C::Open(pid, access));
            let procs = self.procs.borrow();
            let Some(p) = procs.get(&pid).filter(|p| self.alive(p)) else {
                return Err(e(ERROR_INVALID_PARAMETER));
            };
            if let Some(c) = p.open_error {
                return Err(e(c));
            }
            if access == Access::Kill && p.deny_kill_access {
                return Err(e(ERROR_ACCESS_DENIED));
            }
            Ok(H {
                pid,
                start: p.start,
                access,
            })
        }

        fn creation_time(&self, h: &H) -> io::Result<u64> {
            self.call(C::Times(h.pid));
            if self
                .procs
                .borrow()
                .get(&h.pid)
                .is_some_and(|p| p.times_error)
            {
                return Err(e(ERROR_ACCESS_DENIED));
            }
            Ok(h.start)
        }

        fn in_job(&self, h: &H) -> io::Result<bool> {
            self.call(C::InJob(h.pid));
            Ok(self
                .procs
                .borrow()
                .get(&h.pid)
                .is_some_and(|p| p.start == h.start && p.in_job))
        }

        fn signalled(&self, h: &H, wait: Duration) -> io::Result<bool> {
            self.call(C::Signalled(h.pid, wait));
            if !self.live(h.pid, h.start) {
                return Ok(true);
            }
            let dies = self
                .procs
                .borrow()
                .get(&h.pid)
                .and_then(|p| p.dies_at)
                .filter(|t| *t <= self.now.get() + wait);
            if let Some(t) = dies {
                self.now.set(t);
                return Ok(true);
            }
            self.now.set(self.now.get() + wait);
            Ok(false)
        }

        fn terminate(&self, h: &H) -> io::Result<()> {
            self.call(C::Terminate(h.pid));
            if h.access != Access::Kill {
                return Err(e(ERROR_ACCESS_DENIED));
            }
            if let Some(p) = self.procs.borrow_mut().get_mut(&h.pid)
                && p.start == h.start
                && !p.immortal
            {
                p.alive = false;
            }
            Ok(())
        }
    }

    fn id(pid: u32, start: u64) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            start_time: start.to_string(),
            group: String::new(),
        }
    }

    fn state() -> Mutex<KillState<H>> {
        Mutex::new(KillState::default())
    }

    fn pids(r: &KillReport) -> Vec<u32> {
        r.survivors.iter().map(|s| s.pid).collect()
    }

    // ---- CL-17: the proof rule ----

    #[test]
    fn job_empty_and_handles_signalled_proves_death() {
        let os = Os::new();
        os.add(200, proc(5, false));
        let r = os.kill(&[id(200, 5)], os.at(5_000), &state());
        assert_eq!(r, report(METHOD, true, Vec::new()));
        assert!(!os.live(L, 1_000));
        assert!(!os.live(200, 5));
    }

    #[test]
    fn nonzero_count_is_not_proof() {
        let os = Os::new();
        os.add(300, proc(9, true));
        os.set(300, |p| p.immortal = true);
        let r = os.kill(&[], os.at(1_000), &state());
        assert!(!r.tree_dead);
        assert_eq!(r.method, METHOD_INCOMPLETE);
        // The leader was never seen signalled (the count never read 0); the member is listed.
        assert_eq!(pids(&r), vec![L, 300], "{r:?}");
    }

    #[test]
    fn unsignalled_held_is_not_proof() {
        let os = Os::new();
        os.add(200, proc(5, false));
        os.set(200, |p| p.immortal = true);
        let r = os.kill(&[id(200, 5)], os.at(1_000), &state());
        assert!(!r.tree_dead);
        assert_eq!(r.method, METHOD, "the job count read 0");
        assert_eq!(pids(&r), vec![200]);
    }

    #[test]
    fn unresolved_extra_is_not_proof() {
        let os = Os::new();
        os.add(200, proc(5, false));
        os.set(200, |p| p.open_error = Some(31));
        let r = os.kill(&[id(200, 5)], os.at(1_000), &state());
        assert!(!r.tree_dead);
        assert_eq!(r.survivors, vec![id(200, 5)]);
    }

    #[test]
    fn malformed_extra_is_not_proof() {
        for bad in [
            id(0, 5),
            id(SYSTEM_PID, 5),
            id(ME, 5),
            ProcessIdentity {
                pid: 200,
                start_time: "x".into(),
                group: String::new(),
            },
        ] {
            let os = Os::new();
            let r = os.kill(std::slice::from_ref(&bad), os.at(500), &state());
            assert!(!r.tree_dead, "{bad:?}");
            assert_eq!(r.survivors, vec![bad.clone()]);
            assert!(
                !os.calls().iter().any(|c| matches!(c, C::Open(..))),
                "a malformed entry is never opened: {bad:?}"
            );
        }
    }

    #[test]
    fn query_error_is_not_proof() {
        let os = Os::new();
        os.count_error.set(true);
        let r = os.kill(&[], os.at(500), &state());
        assert!(!r.tree_dead);
        assert_eq!(r.method, METHOD_INCOMPLETE);
        assert_eq!(pids(&r), vec![L]);
    }

    #[test]
    fn terminate_failure_does_not_decide() {
        // The job termination fails, but the leader dies by itself: proven.
        let os = Os::new();
        os.terminate_job_error.set(true);
        os.set(L, |p| p.dies_at = Some(Duration::from_millis(30)));
        let r = os.kill(&[], os.at(1_000), &state());
        assert!(r.tree_dead, "{r:?}");
        // And it never succeeds while the leader lives on.
        let os = Os::new();
        os.terminate_job_error.set(true);
        let r = os.kill(&[], os.at(300), &state());
        assert!(!r.tree_dead);
    }

    #[test]
    fn members_left_are_terminated_again() {
        let os = Os::new();
        os.add(300, proc(9, true));
        os.set(300, |p| p.immortal = true);
        let st = state();
        *os.hook.borrow_mut() = Some(Box::new(|c, os| {
            // The member becomes killable at the third termination.
            if *c == C::TerminateJob
                && os.count(|c| *c == C::TerminateJob) == 3
                && let Some(p) = os.procs.borrow_mut().get_mut(&300)
            {
                p.immortal = false;
            }
        }));
        let r = os.kill(&[], os.at(2_000), &st);
        assert!(os.count(|c| *c == C::TerminateJob) >= 3, "{:?}", os.calls());
        assert!(r.tree_dead, "{r:?}");
    }

    #[test]
    fn opening_check_needs_an_empty_job() {
        // The leader is already dead, but a job member still runs: no opening proof.
        let os = Os::new();
        os.set(L, |p| p.alive = false);
        os.add(300, proc(7, true));
        let r = os.kill(&[], os.at(1_000), &state());
        assert!(r.tree_dead);
        assert!(
            os.count(|c| *c == C::TerminateJob) >= 1,
            "the live member was terminated before the proof: {:?}",
            os.calls()
        );
        assert!(!os.live(300, 7));
    }

    #[test]
    fn malformed_entry_blocks_the_opening_proof() {
        // An already dead tree with an empty job is still not proven with a malformed entry.
        let os = Os::new();
        os.set(L, |p| p.alive = false);
        let bad = id(0, 5);
        let r = os.kill(std::slice::from_ref(&bad), os.at(500), &state());
        assert!(!r.tree_dead);
        assert_eq!(r.survivors, vec![bad]);
    }

    #[test]
    fn dead_tree_returns_without_terminate() {
        let os = Os::new();
        os.set(L, |p| p.alive = false);
        os.add(200, proc(5, false));
        os.set(200, |p| p.alive = false);
        let r = os.kill(&[id(200, 5)], os.at(1_000), &state());
        assert!(r.tree_dead);
        assert_eq!(
            os.count(|c| matches!(c, C::TerminateJob | C::Terminate(_))),
            0,
            "{:?}",
            os.calls()
        );
    }

    #[test]
    fn three_calls() {
        let os = Os::new();
        let st = state();
        // 1: proven.
        assert!(os.kill(&[], os.at(1_000), &st).tree_dead);
        // 2: a live extra outside the job that survives termination times out as a survivor.
        os.add(200, proc(5, false));
        os.set(200, |p| p.immortal = true);
        let r = os.kill(&[id(200, 5)], os.at(2_000), &st);
        assert!(!r.tree_dead);
        assert_eq!(r.survivors, vec![id(200, 5)]);
        // 3: an expired call with the same list still lists it.
        os.now.set(Duration::from_millis(3_000));
        let r = os.kill(&[id(200, 5)], os.at(3_000), &st);
        assert!(!r.tree_dead);
        assert_eq!(r.method, METHOD_INCOMPLETE);
        assert_eq!(pids(&r), vec![L, 200]);
    }

    #[test]
    fn survivors_are_confirmed_members() {
        let os = Os::new();
        os.add(300, proc(9, true));
        os.set(300, |p| p.immortal = true);
        // 301 is listed, but by the time it is opened the pid names a process outside the
        // job.
        os.add(301, proc(10, true));
        os.set(301, |p| p.immortal = true);
        *os.hook.borrow_mut() = Some(Box::new(|c, os| {
            if let C::JobPids(_) = c {
                os.set(301, |p| {
                    p.in_job = false;
                    p.start = 11;
                });
            }
        }));
        let r = os.kill(&[], os.at(500), &state());
        assert!(!r.tree_dead);
        assert!(pids(&r).contains(&300), "{r:?}");
        assert!(!pids(&r).contains(&301), "{r:?}");
        let m = r.survivors.iter().find(|s| s.pid == 300).unwrap();
        assert_eq!(m.group, format!("job:{L}"));
        assert_eq!(m.start_time, "9");
    }

    #[test]
    fn list_failure_is_incomplete() {
        let os = Os::new();
        os.add(300, proc(9, true));
        os.set(300, |p| p.immortal = true);
        os.list_error.set(true);
        let r = os.kill(&[], os.at(500), &state());
        assert_eq!(r.method, METHOD_INCOMPLETE);
        assert!(!pids(&r).contains(&300), "no snapshot: {r:?}");
        assert_eq!(os.count(|c| matches!(c, C::JobPids(_))), 1, "taken once");
    }

    #[test]
    fn nonzero_last_read_is_incomplete() {
        let os = Os::new();
        os.add(300, proc(9, true));
        os.set(300, |p| p.immortal = true);
        let r = os.kill(&[], os.at(500), &state());
        assert_eq!(r.method, METHOD_INCOMPLETE);
        // A zero last read with an unproven extra is complete.
        let os = Os::new();
        os.add(200, proc(5, false));
        os.set(200, |p| p.open_error = Some(31));
        let r = os.kill(&[id(200, 5)], os.at(500), &state());
        assert_eq!(r.method, METHOD);
    }

    #[test]
    fn list_grows_by_doubling() {
        let os = Os::new();
        os.list_needs.set(256);
        assert!(list_members(&os, os.at(1_000)).is_some());
        assert_eq!(
            os.calls(),
            vec![C::JobPids(64), C::JobPids(128), C::JobPids(256)]
        );
        let os = Os::new();
        os.list_needs.set(LIST_MAX + 1);
        assert_eq!(list_members(&os, os.at(1_000)), None);
        assert_eq!(os.calls().last(), Some(&C::JobPids(LIST_MAX)));
    }

    #[test]
    fn expiry_inside_a_list_retry_makes_no_further_call() {
        let os = Os::new();
        os.list_needs.set(1_000);
        os.cost.set(Duration::from_millis(10));
        assert_eq!(list_members(&os, os.at(15)), None);
        assert_eq!(os.calls(), vec![C::JobPids(64), C::JobPids(128)]);
    }

    #[test]
    fn expiry_between_the_two_opens_stops() {
        let os = Os::new();
        os.add(200, proc(5, false));
        os.set(200, |p| p.deny_kill_access = true);
        os.cost.set(Duration::from_millis(10));
        assert!(matches!(open_verified(&os, 200, 5, os.at(10)), Opened::Cut));
        assert_eq!(os.calls(), vec![C::Open(200, Access::Kill)]);
    }

    #[test]
    fn expiry_between_open_and_creation_time_stops() {
        let os = Os::new();
        os.add(200, proc(5, false));
        os.cost.set(Duration::from_millis(10));
        assert!(matches!(open_verified(&os, 200, 5, os.at(10)), Opened::Cut));
        assert_eq!(os.calls(), vec![C::Open(200, Access::Kill)]);
    }

    #[test]
    fn sub_millisecond_budget_polls() {
        let os = Os::new();
        os.add(200, proc(5, false));
        os.set(200, |p| p.immortal = true);
        let st = state();
        // Leave 0.5 ms for the handle waits of the first zero-count round.
        let deadline = os.base + Duration::from_micros(500);
        let _ = os.kill(&[id(200, 5)], deadline, &st);
        for c in os.calls() {
            if let C::Signalled(_, w) = c {
                assert_eq!(w, Duration::ZERO, "{:?}", os.calls());
            }
        }
        assert!(os.now() <= deadline, "no wait passed the deadline");
        // A whole budget is floored to milliseconds, then the remainder polls.
        let os = Os::new();
        os.add(200, proc(5, false));
        os.set(200, |p| p.immortal = true);
        let deadline = os.base + Duration::from_micros(2_700);
        let _ = os.kill(&[id(200, 5)], deadline, &state());
        let waits: Vec<Duration> = os
            .calls()
            .iter()
            .filter_map(|c| match c {
                C::Signalled(200, w) => Some(*w),
                _ => None,
            })
            .collect();
        assert!(
            waits.contains(&Duration::from_millis(2)),
            "{:?}",
            os.calls()
        );
        assert!(waits.contains(&Duration::ZERO), "{:?}", os.calls());
        assert!(os.now() <= deadline);
    }

    /// The wait budget comes from one clock read: an expired deadline makes no call, any
    /// positive remainder under 1 ms polls, a larger one is floored and capped.
    #[test]
    fn budget_separates_expiry_from_a_sub_millisecond_remainder() {
        let os = Os::new();
        let cap = PROOF_WAIT;
        let at = |d: Duration| os.base + d;
        assert_eq!(budget(&os, at(Duration::ZERO), cap), None);
        assert_eq!(
            budget(&os, at(Duration::from_nanos(1)), cap),
            Some(Duration::ZERO)
        );
        assert_eq!(
            budget(&os, at(Duration::from_micros(1_500)), cap),
            Some(Duration::from_millis(1))
        );
        assert_eq!(budget(&os, at(Duration::from_secs(1)), cap), Some(cap));
    }

    /// A deadline reached right before the proof waits makes no wait call at all; one
    /// nanosecond before it, the leader is polled.
    #[test]
    fn proof_waits_at_both_deadline_boundaries() {
        for (left, polls) in [(Duration::ZERO, false), (Duration::from_nanos(1), true)] {
            let os = Os::new();
            os.add(300, proc(7, true));
            let deadline = os.at(1_000);
            // The proof round's count read (the second; the opening check made the first)
            // reads 0, and the clock jumps to `deadline - left` on it.
            let reads = Cell::new(0);
            *os.hook.borrow_mut() = Some(Box::new(move |c, os| {
                if *c == C::Active {
                    reads.set(reads.get() + 1);
                    if reads.get() == 2 {
                        os.now.set(Duration::from_secs(1) - left);
                    }
                }
            }));
            let r = os.kill(&[], deadline, &state());
            let calls = os.calls();
            let after = calls
                .iter()
                .skip_while(|c| **c != C::Active)
                .skip(1)
                .skip_while(|c| **c != C::Active)
                .skip(1);
            let waits: Vec<&C> = after.filter(|c| matches!(c, C::Signalled(..))).collect();
            if polls {
                assert_eq!(
                    waits.first(),
                    Some(&&C::Signalled(L, Duration::ZERO)),
                    "{calls:?}"
                );
            } else {
                assert!(waits.is_empty(), "a wait after the deadline: {calls:?}");
                assert!(!r.tree_dead);
            }
        }
    }

    /// An extra first resolved in a proof round is terminated once in that round.
    #[test]
    fn new_extra_is_terminated_once() {
        let os = Os::new();
        os.add(200, proc(5, false));
        os.set(200, |p| p.open_error = Some(31));
        *os.hook.borrow_mut() = Some(Box::new(|c, os| {
            if *c == C::TerminateJob {
                os.set(200, |p| p.open_error = None);
            }
        }));
        let r = os.kill(&[id(200, 5)], os.at(1_000), &state());
        assert!(r.tree_dead, "{r:?}");
        assert_eq!(os.count(|c| *c == C::Terminate(200)), 1, "{:?}", os.calls());
    }

    #[test]
    fn nonzero_job_rounds_retry_an_extra() {
        let os = Os::new();
        os.add(300, proc(9, true));
        os.set(300, |p| p.immortal = true);
        os.add(200, proc(5, false));
        os.set(200, |p| p.open_error = Some(31));
        *os.hook.borrow_mut() = Some(Box::new(|c, os| {
            // The extra becomes openable while the job stays nonempty.
            if *c == C::Active && os.count(|c| *c == C::Active) == 3 {
                os.set(200, |p| p.open_error = None);
            }
        }));
        let st = state();
        let r = os.kill(&[id(200, 5)], os.at(1_000), &st);
        assert!(!r.tree_dead);
        assert!(os.calls().contains(&C::Terminate(200)), "{:?}", os.calls());
        assert!(!os.live(200, 5), "held and terminated");
    }

    #[test]
    fn zero_job_with_unverifiable_extra_paces() {
        let os = Os::new();
        os.add(200, proc(5, false));
        os.set(200, |p| p.open_error = Some(31));
        let r = os.kill(&[id(200, 5)], os.at(200), &state());
        assert!(!r.tree_dead);
        let rounds = os.count(|c| *c == C::Active);
        let sleeps = os.count(|c| matches!(c, C::Sleep(_)));
        assert!(rounds > 2, "{:?}", os.calls());
        // The round that saw the leader signal made progress; every other round slept.
        assert!(sleeps + 1 >= rounds, "rounds {rounds}, sleeps {sleeps}");
        assert!(rounds <= 25, "no spin: {rounds} rounds in 200 ms");
    }

    #[test]
    fn sleeps_never_pass_the_deadline() {
        // Busy poll.
        let os = Os::new();
        let st = state();
        let g = st.lock().unwrap();
        let deadline = os.base + Duration::from_micros(25_500);
        let r = os.kill(&[], deadline, &st);
        drop(g);
        assert_eq!(r.method, METHOD_BUSY);
        assert_eq!(os.now(), deadline);
        // Query-error path.
        let os = Os::new();
        os.count_error.set(true);
        let deadline = os.base + Duration::from_micros(33_300);
        let _ = os.kill(&[], deadline, &state());
        assert!(os.now() <= deadline);
        for c in os.calls() {
            if let C::Sleep(d) = c {
                assert!(d <= STEP, "{d:?}");
            }
        }
    }

    #[test]
    fn expired_call_makes_no_os_call() {
        let os = Os::new();
        os.now.set(Duration::from_millis(10));
        let st = state();
        let r = os.kill(&[id(200, 5)], os.at(10), &st);
        assert!(os.calls().is_empty(), "{:?}", os.calls());
        assert_eq!(r.method, METHOD_INCOMPLETE);
        assert_eq!(pids(&r), vec![L, 200]);
    }

    #[test]
    fn expired_call_with_busy_lock_is_busy() {
        let os = Os::new();
        os.now.set(Duration::from_millis(10));
        let st = state();
        let _g = st.lock().unwrap();
        let r = os.kill(&[id(200, 5)], os.at(10), &st);
        assert_eq!(r.method, METHOD_BUSY);
        assert!(os.calls().is_empty());
        assert_eq!(pids(&r), vec![L, 200]);
    }

    #[test]
    fn busy_returns_the_supplied_identities() {
        let os = Os::new();
        let st = state();
        let _g = st.lock().unwrap();
        let r = os.kill(&[id(200, 5), id(201, 6)], os.at(50), &st);
        assert_eq!(r.method, METHOD_BUSY);
        assert!(!r.tree_dead);
        assert_eq!(pids(&r), vec![L, 200, 201]);
        assert!(
            os.calls().iter().all(|c| matches!(c, C::Sleep(_))),
            "{:?}",
            os.calls()
        );
    }

    #[test]
    fn poisoned_lock_is_taken_over() {
        let os = Os::new();
        let st = state();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = st.lock().unwrap();
            panic!("poison");
        }));
        assert!(os.kill(&[], os.at(1_000), &st).tree_dead);
    }

    // ---- CL-18: extras ----

    #[test]
    fn stale_extra_is_not_terminated() {
        let os = Os::new();
        os.add(200, proc(6, false));
        os.set(200, |p| p.immortal = false);
        let r = os.kill(&[id(200, 5)], os.at(1_000), &state());
        assert!(r.tree_dead, "stale resolves: {r:?}");
        assert!(os.live(200, 6), "the process now at the pid survives");
        assert!(!os.calls().contains(&C::Terminate(200)));
    }

    #[test]
    fn gone_extra_is_resolved() {
        let os = Os::new();
        let r = os.kill(&[id(200, 5)], os.at(1_000), &state());
        assert!(r.tree_dead, "{r:?}");
    }

    #[test]
    fn access_denied_extra_is_waited_not_killed() {
        let os = Os::new();
        os.add(200, proc(5, false));
        os.set(200, |p| {
            p.deny_kill_access = true;
            p.dies_at = Some(Duration::from_millis(300));
        });
        let r = os.kill(&[id(200, 5)], os.at(2_000), &state());
        assert!(r.tree_dead, "{r:?}");
        assert!(os.calls().contains(&C::Open(200, Access::Wait)));
        // It died on its own at 300 ms: the termination through the wait handle failed.
        assert!(os.now() >= os.at(300), "it died on its own");
        let os = Os::new();
        os.add(200, proc(5, false));
        os.set(200, |p| p.deny_kill_access = true);
        let r = os.kill(&[id(200, 5)], os.at(500), &state());
        assert!(!r.tree_dead);
        assert!(os.live(200, 5), "never killed through a wait-only handle");
        assert_eq!(pids(&r), vec![200]);
    }

    #[test]
    fn unverifiable_extra_is_a_survivor() {
        let os = Os::new();
        os.add(200, proc(5, false));
        os.set(200, |p| p.times_error = true);
        let r = os.kill(&[id(200, 5)], os.at(500), &state());
        assert!(!r.tree_dead);
        assert_eq!(r.survivors, vec![id(200, 5)]);
        assert!(
            os.live(200, 5),
            "an unverified handle is never used to kill"
        );
    }

    #[test]
    fn reserved_pids_are_never_opened() {
        let os = Os::new();
        for pid in [0, SYSTEM_PID, ME] {
            os.add(pid, proc(5, false));
        }
        let r = os.kill(
            &[id(0, 5), id(SYSTEM_PID, 5), id(ME, 5)],
            os.at(300),
            &state(),
        );
        assert!(!r.tree_dead);
        assert!(!os.calls().iter().any(|c| matches!(c, C::Open(..))));
        assert_eq!(pids(&r), vec![0, SYSTEM_PID, ME]);
    }

    #[test]
    fn held_handle_blocks_a_later_empty_call() {
        let os = Os::new();
        os.add(200, proc(5, false));
        os.set(200, |p| p.immortal = true);
        let st = state();
        let r = os.kill(&[id(200, 5)], os.at(300), &st);
        assert!(!r.tree_dead);
        assert_eq!(st.lock().unwrap().held_count(), 1);
        let r = os.kill(&[], os.at(600), &st);
        assert!(!r.tree_dead, "the held extra still lives");
        assert_eq!(r.survivors, vec![id(200, 5)]);
        os.set(200, |p| p.alive = false);
        let r = os.kill(&[], os.at(900), &st);
        assert!(r.tree_dead, "{r:?}");
        assert_eq!(st.lock().unwrap().held_count(), 0);
    }

    #[test]
    fn busy_then_original_list_retry() {
        let os = Os::new();
        // pid 200 is now a process with start 6; the stale record says 5.
        os.add(200, proc(6, false));
        os.set(200, |p| p.immortal = true);
        let list = [id(200, 5), id(200, 6)];
        let st = state();
        let g = st.lock().unwrap();
        let r = os.kill(&list, os.at(50), &st);
        drop(g);
        assert_eq!(r.method, METHOD_BUSY);
        assert_eq!(r.survivors.iter().filter(|s| s.pid == 200).count(), 1);
        assert_eq!(
            r.survivors
                .iter()
                .find(|s| s.pid == 200)
                .unwrap()
                .start_time,
            "5",
            "the smallest start"
        );
        let r = os.kill(&list, os.at(500), &st);
        assert!(!r.tree_dead, "nothing proven while the current one lives");
        assert_eq!(st.lock().unwrap().held_count(), 1);
        assert_eq!(r.survivors, vec![id(200, 6)]);
        os.set(200, |p| p.alive = false);
        let r = os.kill(&list, os.at(900), &st);
        assert!(r.tree_dead, "{r:?}");
    }

    #[test]
    fn expiry_during_the_first_open_leaves_the_second_a_survivor() {
        let os = Os::new();
        os.add(200, proc(5, false));
        os.add(201, proc(6, false));
        *os.hook.borrow_mut() = Some(Box::new(|c, os| {
            if *c == C::Open(200, Access::Kill) {
                os.now.set(Duration::from_millis(100));
            }
        }));
        let r = os.kill(&[id(200, 5), id(201, 6)], os.at(100), &state());
        assert!(!r.tree_dead);
        assert!(!os.calls().iter().any(|c| matches!(c, C::Open(201, _))));
        assert!(pids(&r).contains(&201), "{r:?}");
        assert!(pids(&r).contains(&200), "{r:?}");
    }

    #[test]
    fn unresolved_identity_is_not_retained() {
        let os = Os::new();
        os.add(200, proc(5, false));
        os.set(200, |p| p.open_error = Some(31));
        let st = state();
        let r = os.kill(&[id(200, 5)], os.at(300), &st);
        assert!(!r.tree_dead);
        assert_eq!(st.lock().unwrap().held_count(), 0);
        // A later call without it proves the tree: it was never kept.
        let r = os.kill(&[], os.at(600), &st);
        assert!(r.tree_dead, "{r:?}");
    }

    #[test]
    fn duplicates_report_one_per_pid() {
        let os = Os::new();
        os.set(L, |p| p.immortal = true);
        os.add(201, proc(9, false));
        os.set(201, |p| p.open_error = Some(31));
        let bad = ProcessIdentity {
            pid: 202,
            start_time: "x".into(),
            group: String::new(),
        };
        let r = os.kill(
            &[
                id(L, 1_000),
                id(201, 9),
                id(201, 8),
                id(201, 8),
                bad.clone(),
                bad.clone(),
            ],
            os.at(200),
            &state(),
        );
        assert!(!r.tree_dead);
        assert_eq!(pids(&r), vec![L, 201, 202], "{r:?}");
        assert_eq!(
            r.survivors[0].group,
            format!("job:{L}"),
            "the leader entry wins"
        );
        assert_eq!(r.survivors[1].start_time, "8");
    }
}
