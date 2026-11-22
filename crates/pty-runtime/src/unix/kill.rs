//! The tree kill (`docs/design/pty-unix.md` §2.5): passes over the process table, the
//! census rule, HUP → TERM → KILL with budget-capped waits, and the proof of death. It runs
//! over an [`Os`] so the unit tests drive it with a synthetic process table and clock.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io;
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use super::table::{Lookup, Proc};
use crate::backend::{KillReport, ProcessIdentity};

pub(crate) const METHOD: &str = "unix_killpg_session_scan";
pub(crate) const METHOD_LOST: &str = "unix_ownership_lost";
pub(crate) const METHOD_BUSY: &str = "unix_busy";

/// The poll step of the grace waits and the proof loop.
const STEP: Duration = Duration::from_millis(25);
/// The poll step while another call holds the kill lock.
const LOCK_STEP: Duration = Duration::from_millis(10);
/// Retries of a saturated process list, each with double the capacity.
const LIST_RETRIES: usize = 3;
/// Extra entries over the count call's estimate.
const LIST_SLACK: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sig {
    Hup,
    Term,
    Kill,
}

/// What the kill engine needs from the OS. `owned` is the only call that may change the
/// ownership state; `lost` only reads it.
pub(crate) trait Os {
    /// A fresh ownership check of the leader (§2.4), bounded by `deadline`, with the
    /// leader's record when the check read one.
    fn owned(&self, deadline: Instant) -> (bool, Option<Lookup>);
    /// Whether ownership was lost; no syscall.
    fn lost(&self) -> bool;
    fn pid_count(&self) -> io::Result<usize>;
    /// The pids and whether the buffer was full.
    fn pid_list(&self, capacity: usize) -> io::Result<(Vec<u32>, bool)>;
    fn lookup(&self, pid: u32) -> Lookup;
    /// The session id, or the errno.
    fn getsid(&self, pid: u32) -> Result<u32, i32>;
    /// Errors are ignored: a signal that cannot be sent leaves the target for the proof.
    fn killpg(&self, pgid: u32, sig: Sig);
    fn kill(&self, pid: u32, sig: Sig);
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
}

/// The leader of the tree: its pid (also its group and session id), its recorded start
/// (none during a stage 5 failure), and the daemon's own pid.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Leader {
    pub(crate) pid: u32,
    pub(crate) start: Option<u64>,
    pub(crate) me: u32,
}

/// Targets kept across calls: `(pid, start)` → the process group last read (0 unknown).
/// Only [`Call::read`] and [`Call::read_sid`] change it, at the moment of each direct read
/// (§2.5, the retention invariant).
#[derive(Debug, Default)]
pub(crate) struct KillState {
    targets: BTreeMap<(u32, u64), u32>,
}

/// One census pass.
#[derive(Debug)]
struct Pass {
    /// The whole list was obtained and every pid in it looked up.
    complete: bool,
    clean: bool,
    /// Every defect is churn (a process exited during the pass).
    churn_only: bool,
    /// A covered process was read live or unreadable in this pass.
    alive: bool,
    /// The ownership check after the pass held.
    owned: bool,
    /// Pids whose covered generation this pass read live: their children are covered.
    covered: HashSet<u32>,
    /// Live processes this pass read that are not covered (yet), by parent pid.
    pending: HashMap<u32, Vec<Proc>>,
    /// The parent pid each pending process waits under; a pending process the pass read
    /// resolved is no longer here.
    waiting: HashMap<u32, u32>,
}

impl Pass {
    fn new() -> Pass {
        Pass {
            complete: false,
            clean: true,
            churn_only: true,
            alive: false,
            owned: false,
            covered: HashSet::new(),
            pending: HashMap::new(),
            waiting: HashMap::new(),
        }
    }

    fn churn(&mut self) {
        self.clean = false;
    }

    fn incomplete(&mut self) {
        self.clean = false;
        self.churn_only = false;
    }
}

/// What a proof step established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proof {
    /// Every covered identity was read resolved.
    Dead,
    /// A covered process was read live or unreadable.
    Alive,
    /// Nothing may be proven yet (an unclean pass, no ownership, or the deadline).
    Unknown,
}

/// A process not proven dead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Survivor {
    pid: u32,
    start: Option<u64>,
    pgid: Option<u32>,
}

impl Survivor {
    fn identity(self) -> ProcessIdentity {
        ProcessIdentity {
            pid: self.pid,
            start_time: self.start.map(|s| s.to_string()).unwrap_or_default(),
            group: self
                .pgid
                .filter(|g| *g != 0)
                .map(|g| g.to_string())
                .unwrap_or_default(),
        }
    }
}

struct Call<'a, O: Os> {
    os: &'a O,
    leader: Leader,
    deadline: Instant,
    /// Zombies seen in this call.
    zombies: BTreeSet<(u32, u64)>,
    /// Ownership was established at the call's start: only then does session membership
    /// cover a process, and only while ownership has not been lost since.
    owned0: bool,
    /// Covered processes without a readable start (unreadable session members, and the
    /// leader while its start is unknown): pid → its group when known.
    unknown: BTreeMap<u32, Option<u32>>,
    /// The start each pid was last read live with: the report prefers that generation.
    last_live: HashMap<u32, u64>,
}

impl<O: Os> Call<'_, O> {
    fn expired(&self) -> bool {
        self.os.now() >= self.deadline
    }

    fn sleep(&self, step: Duration) {
        let left = self.deadline.saturating_duration_since(self.os.now());
        let d = step.min(left);
        if !d.is_zero() {
            self.os.sleep(d);
        }
    }

    fn member_ok(&self) -> bool {
        self.owned0 && !self.os.lost()
    }

    /// The one place a `lookup` lands (§2.5, the retention invariant). Before anything
    /// else runs, the retained targets take the read: every other generation at that pid
    /// is removed when the read shows it gone, a zombie, or another start, and a covered
    /// live record (a retained identity, a session member, or a child of a generation this
    /// pass read covered) is retained under its full identity. `pass` is the census pass
    /// the read belongs to, if any.
    fn read(&mut self, st: &mut KillState, pid: u32, pass: Option<&mut Pass>) -> Lookup {
        let r = self.os.lookup(pid);
        self.apply(st, pid, r, pass);
        r
    }

    /// Applies one read of `pid` (see [`Call::read`]); also used for the leader record an
    /// ownership check read.
    fn apply(&mut self, st: &mut KillState, pid: u32, r: Lookup, mut pass: Option<&mut Pass>) {
        match r {
            Lookup::Gone | Lookup::Zombie(_) => self.resolve(st, pid, None, pass),
            Lookup::Live(p) => {
                self.resolve(st, pid, Some(p.start), pass.as_deref_mut());
                self.last_live.insert(pid, p.start);
                let covered = st.targets.contains_key(&(pid, p.start))
                    || (self.member_ok() && (p.pgid == self.leader.pid || pid == self.leader.pid))
                    || pass.as_ref().is_some_and(|ps| ps.covered.contains(&p.ppid));
                if covered {
                    self.cover(st, p, pass);
                } else if let Some(ps) = pass {
                    ps.pending.entry(p.ppid).or_default().push(p);
                    ps.waiting.insert(p.pid, p.ppid);
                }
            }
            Lookup::Unreadable => {
                let held = self.unknown.contains_key(&pid)
                    || st
                        .targets
                        .range((pid, 0)..=(pid, u64::MAX))
                        .next()
                        .is_some();
                if let Some(ps) = pass
                    && held
                {
                    ps.alive = true;
                }
            }
        }
    }

    /// The `getsid` that follows a census lookup of `pid` (`looked` its result), through
    /// the same rule: `ESRCH` resolves the pid; the leader's session covers the process.
    fn read_sid(
        &mut self,
        st: &mut KillState,
        pid: u32,
        looked: Lookup,
        pass: &mut Pass,
    ) -> Result<u32, i32> {
        let r = self.os.getsid(pid);
        match r {
            Err(libc::ESRCH) => self.resolve(st, pid, None, Some(pass)),
            Ok(sid) if sid == self.leader.pid && self.member_ok() => match looked {
                Lookup::Live(p) => self.cover(st, p, Some(pass)),
                Lookup::Unreadable => {
                    self.unknown.entry(pid).or_insert(None);
                    pass.alive = true;
                }
                Lookup::Gone | Lookup::Zombie(_) => {}
            },
            _ => {}
        }
        r
    }

    /// Retains `p` and, within `pass`, every pending descendant of it. A pending process
    /// the pass has since read resolved is not retained, but the ancestry it was read with
    /// still leads to its own pending children.
    fn cover(&mut self, st: &mut KillState, p: Proc, pass: Option<&mut Pass>) {
        let mut work = vec![(p, true)];
        let mut pass = pass;
        while let Some((p, retain)) = work.pop() {
            if retain {
                st.targets.insert((p.pid, p.start), p.pgid);
                self.unknown.remove(&p.pid);
            }
            if let Some(ps) = pass.as_deref_mut() {
                if retain {
                    ps.alive = true;
                    ps.waiting.remove(&p.pid);
                }
                if ps.covered.insert(p.pid) {
                    for c in ps.pending.remove(&p.pid).unwrap_or_default() {
                        let waiting = ps.waiting.remove(&c.pid).is_some();
                        work.push((c, waiting));
                    }
                }
            }
        }
    }

    /// Removes the identities at `pid` that a read disproved: all of them, or all but the
    /// generation `keep` that the read showed live. A pending process the read shows gone
    /// stops waiting, so covering its parent cannot retain it (see [`Call::cover`]).
    fn resolve(
        &mut self,
        st: &mut KillState,
        pid: u32,
        keep: Option<u64>,
        pass: Option<&mut Pass>,
    ) {
        let gone: Vec<(u32, u64)> = st
            .targets
            .range((pid, 0)..=(pid, u64::MAX))
            .map(|(k, _)| *k)
            .filter(|(_, s)| Some(*s) != keep)
            .collect();
        for k in gone {
            st.targets.remove(&k);
        }
        if keep.is_none() {
            self.unknown.remove(&pid);
            self.last_live.remove(&pid);
            if let Some(ps) = pass {
                ps.waiting.remove(&pid);
            }
        }
    }

    /// A fresh ownership check; the leader record it read goes through [`Call::apply`].
    fn owned(&mut self, st: &mut KillState) -> bool {
        let (owned, looked) = self.os.owned(self.deadline);
        if let Some(r) = looked {
            self.apply(st, self.leader.pid, r, None);
        }
        owned
    }

    /// A census pass, then a fresh ownership check.
    fn round(&mut self, st: &mut KillState) -> Pass {
        let mut pass = self.pass(st);
        pass.owned = !self.expired() && self.owned(st);
        pass
    }

    fn pass(&mut self, st: &mut KillState) -> Pass {
        let mut p = Pass::new();
        if self.expired() {
            p.incomplete();
            return p;
        }
        let Some(list) = self.list() else {
            p.incomplete();
            return p;
        };
        for pid in list {
            // pid 0 is the kernel; `getsid(0)` would name the caller's own session.
            if pid == 0 {
                continue;
            }
            if self.expired() {
                p.incomplete();
                return p;
            }
            let looked = self.read(st, pid, Some(&mut p));
            match looked {
                Lookup::Gone => p.churn(),
                Lookup::Zombie(start) => {
                    if self.zombies.insert((pid, start)) {
                        p.churn();
                    }
                }
                Lookup::Live(_) | Lookup::Unreadable => {
                    if self.expired() {
                        p.incomplete();
                        return p;
                    }
                    match self.read_sid(st, pid, looked, &mut p) {
                        Ok(_) => {}
                        Err(libc::ESRCH) => p.churn(),
                        // A sid that could have made the pid a member.
                        Err(_) => p.incomplete(),
                    }
                }
            }
        }
        p.complete = true;
        p
    }

    /// The whole process list, or `None` when it failed, came back empty, stayed
    /// saturated, or the deadline passed.
    fn list(&self) -> Option<Vec<u32>> {
        let count = self.os.pid_count().ok().filter(|n| *n > 0)?;
        let mut capacity = count + LIST_SLACK;
        for _ in 0..=LIST_RETRIES {
            if self.expired() {
                return None;
            }
            let (pids, saturated) = self.os.pid_list(capacity).ok()?;
            if pids.is_empty() {
                return None;
            }
            if !saturated {
                return Some(pids);
            }
            capacity *= 2;
        }
        None
    }

    /// Every retained identity and every covered pid without a start, each read by
    /// itself, so one the list omitted is still seen. Death is proven exactly when these
    /// reads leave nothing retained: a read that keeps an identity, or retains a new
    /// generation, leaves the set non-empty. A cut by the deadline proves nothing.
    fn recheck(&mut self, st: &mut KillState) -> Proof {
        let ids: Vec<u32> = st.targets.keys().map(|(pid, _)| *pid).collect();
        let unknown: Vec<u32> = self.unknown.keys().copied().collect();
        for pid in ids.into_iter().chain(unknown) {
            if self.expired() {
                return Proof::Unknown;
            }
            self.read(st, pid, None);
        }
        if st.targets.is_empty() && self.unknown.is_empty() {
            Proof::Dead
        } else {
            Proof::Alive
        }
    }

    /// The proof step of a pass that read nothing covered alive.
    fn prove(&mut self, pass: &Pass, st: &mut KillState) -> Proof {
        if !(pass.clean && pass.owned && self.owned0) {
            return Proof::Unknown;
        }
        self.recheck(st)
    }

    /// The group (after a fresh ownership check) and every target outside it, each by a
    /// verified per-pid signal.
    fn signal(&mut self, sig: Sig, st: &mut KillState) {
        if self.expired() {
            return;
        }
        let owned = self.owned(st);
        if owned {
            if self.expired() {
                return;
            }
            self.os.killpg(self.leader.pid, sig);
        }
        let ids: Vec<((u32, u64), u32)> = st.targets.iter().map(|(k, g)| (*k, *g)).collect();
        for ((pid, start), pgid) in ids {
            if owned && pgid == self.leader.pid {
                continue;
            }
            if self.expired() {
                return;
            }
            self.signal_verified(st, pid, start, sig);
        }
    }

    /// `lookup` (through [`Call::read`]), the clock check, `kill`: only when the lookup
    /// shows the recorded start.
    fn signal_verified(&mut self, st: &mut KillState, pid: u32, start: u64, sig: Sig) {
        if pid == 0 || pid == 1 || pid == self.leader.me {
            return;
        }
        if self.expired() {
            return;
        }
        if let Lookup::Live(p) = self.read(st, pid, None)
            && p.start == start
        {
            if self.expired() {
                return;
            }
            self.os.kill(pid, sig);
        }
    }

    /// Waits until `until` (capped by the deadline), polling the targets every `STEP`,
    /// and returns early once none is live.
    fn grace(&mut self, until: Instant, st: &mut KillState) {
        let until = until.min(self.deadline);
        loop {
            if self.os.now() >= until {
                return;
            }
            let ids: Vec<(u32, u64)> = st.targets.keys().copied().collect();
            let mut any = false;
            for (pid, start) in ids {
                if self.expired() {
                    return;
                }
                match self.read(st, pid, None) {
                    Lookup::Live(p) if p.start == start => any = true,
                    Lookup::Unreadable => any = true,
                    _ => {}
                }
                if any {
                    break;
                }
            }
            if !any {
                return;
            }
            let left = until.saturating_duration_since(self.os.now());
            let d = STEP.min(left);
            if d.is_zero() {
                return;
            }
            self.os.sleep(d);
        }
    }

    /// The survivors, derived from the retained targets (one entry per pid, the
    /// generation last read live preferred) and the covered pids without a start.
    fn survivors(&self, st: &KillState) -> Vec<Survivor> {
        let mut by_pid: BTreeMap<u32, Survivor> = BTreeMap::new();
        for (&(pid, start), &pgid) in &st.targets {
            let s = Survivor {
                pid,
                start: Some(start),
                pgid: Some(pgid),
            };
            let live = self.last_live.get(&pid) == Some(&start);
            match by_pid.entry(pid) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(s);
                }
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    if live {
                        e.insert(s);
                    }
                }
            }
        }
        for (&pid, &pgid) in &self.unknown {
            by_pid.entry(pid).or_insert(Survivor {
                pid,
                start: None,
                pgid,
            });
        }
        by_pid.into_values().collect()
    }
}

/// Accepts an `extra` identity: not pid 0, 1, or the daemon's own, and a start in this
/// backend's format. An identity whose start does not parse cannot match a live process.
fn accept(id: &ProcessIdentity, me: u32) -> Option<(u32, u64)> {
    if id.pid == 0 || id.pid == 1 || id.pid == me {
        return None;
    }
    let start = id.start_time.parse::<u64>().ok()?;
    Some((id.pid, start))
}

fn method<O: Os>(os: &O) -> &'static str {
    if os.lost() { METHOD_LOST } else { METHOD }
}

/// A report built from identities already held, without a syscall: the leader, then each
/// `extra` pid not listed yet.
fn unresolved(leader: Leader, extra: &[(u32, u64)], method: &'static str) -> KillReport {
    let mut listed = HashSet::from([leader.pid]);
    let mut survivors = vec![
        Survivor {
            pid: leader.pid,
            start: leader.start,
            pgid: Some(leader.pid),
        }
        .identity(),
    ];
    for (pid, start) in extra {
        if listed.insert(*pid) {
            survivors.push(
                Survivor {
                    pid: *pid,
                    start: Some(*start),
                    pgid: None,
                }
                .identity(),
            );
        }
    }
    KillReport {
        method: method.to_owned(),
        tree_dead: false,
        survivors,
    }
}

fn lock_until<'m, O: Os>(
    os: &O,
    state: &'m Mutex<KillState>,
    deadline: Instant,
) -> Option<MutexGuard<'m, KillState>> {
    loop {
        match state.try_lock() {
            Ok(g) => return Some(g),
            Err(TryLockError::Poisoned(e)) => return Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => {}
        }
        let now = os.now();
        if now >= deadline {
            return None;
        }
        os.sleep(LOCK_STEP.min(deadline - now));
    }
}

/// The steps of §2.5. Every result comes from this call's own reads.
pub(crate) fn kill_tree<O: Os>(
    os: &O,
    leader: Leader,
    extra: &[ProcessIdentity],
    deadline: Instant,
    state: &Mutex<KillState>,
) -> KillReport {
    let accepted: Vec<(u32, u64)> = extra
        .iter()
        .filter_map(|id| accept(id, leader.me))
        .collect();
    let t0 = os.now();
    if t0 >= deadline {
        // No read and no blocking lock. When the state is free at once, the new `extra`
        // entries are kept for the next call and the kept targets are listed.
        let mut report = unresolved(leader, &accepted, method(os));
        let kept = match state.try_lock() {
            Ok(g) => Some(g),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        if let Some(mut g) = kept {
            for key in &accepted {
                g.targets.entry(*key).or_insert(0);
            }
            let mut listed: HashSet<u32> = report.survivors.iter().map(|s| s.pid).collect();
            for (&(pid, start), &pgid) in &g.targets {
                if listed.insert(pid) {
                    report.survivors.push(
                        Survivor {
                            pid,
                            start: Some(start),
                            pgid: Some(pgid),
                        }
                        .identity(),
                    );
                }
            }
        }
        return report;
    }
    let Some(mut guard) = lock_until(os, state, deadline) else {
        return unresolved(leader, &accepted, METHOD_BUSY);
    };
    let st = &mut *guard;
    for key in &accepted {
        st.targets.entry(*key).or_insert(0);
    }
    if let Some(start) = leader.start {
        st.targets.insert((leader.pid, start), leader.pid);
    }
    let mut call = Call {
        os,
        leader,
        deadline,
        zombies: BTreeSet::new(),
        owned0: false,
        unknown: BTreeMap::new(),
        last_live: HashMap::new(),
    };
    if leader.start.is_none() {
        call.unknown.insert(leader.pid, Some(leader.pid));
    }
    // Ownership for the whole call, under the same deadline rules as every other check.
    call.owned0 = !call.expired() && call.owned(st);
    let budget = deadline - t0;

    // Opening pass: an already dead tree returns before any signal.
    let mut pass = call.round(st);
    let mut alive = false;
    loop {
        if pass.alive {
            alive = true;
            break;
        }
        match call.prove(&pass, st) {
            Proof::Dead => return finish(&call, st, true),
            Proof::Alive => {
                alive = true;
                break;
            }
            Proof::Unknown => {}
        }
        if os.lost() || call.expired() {
            return finish(&call, st, false);
        }
        if !pass.churn_only {
            break;
        }
        pass = call.round(st);
    }

    if alive {
        if budget > Duration::from_millis(100) {
            call.signal(Sig::Hup, st);
            call.grace(t0 + (budget / 4).min(Duration::from_millis(500)), st);
            // Reads what appeared since the HUP.
            let _ = call.round(st);
            call.signal(Sig::Term, st);
            call.grace(t0 + (budget / 2).min(Duration::from_secs(2)), st);
        }
        let _ = call.round(st);
        call.signal(Sig::Kill, st);
    }

    // Prove.
    while !call.expired() {
        pass = call.round(st);
        if !pass.alive {
            match call.prove(&pass, st) {
                Proof::Dead => return finish(&call, st, true),
                Proof::Alive => {}
                Proof::Unknown => {
                    if os.lost() {
                        return finish(&call, st, false);
                    }
                    if !pass.churn_only {
                        call.sleep(STEP);
                    }
                    continue;
                }
            }
        }
        call.signal(Sig::Kill, st);
        call.sleep(STEP);
    }
    finish(&call, st, false)
}

/// The report, derived from the retained targets: nothing is pruned or added here, since
/// every read already updated them.
fn finish<O: Os>(call: &Call<'_, O>, st: &KillState, tree_dead: bool) -> KillReport {
    let tree_dead = tree_dead && !call.os.lost();
    KillReport {
        method: method(call.os).to_owned(),
        tree_dead,
        survivors: if tree_dead {
            Vec::new()
        } else {
            call.survivors(st)
                .into_iter()
                .map(Survivor::identity)
                .collect()
        },
    }
}

#[cfg(test)]
mod tests {
    //! The engine over a synthetic process table and clock: every OS call is logged, and a
    //! hook can change the table at the moment of any call (a fork, an exit), so each
    //! interleaving of the census rule is reproduced exactly.

    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use super::*;

    const ME: u32 = 999;
    const L: u32 = 100;
    const X: u32 = 500;
    const START: u64 = 1_000;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum St {
        Live,
        Zombie,
        Unreadable,
        /// Another user's zombie: only the short record (no start) reads it.
        ForeignZombie,
    }

    #[derive(Debug, Clone)]
    struct P {
        ppid: u32,
        pgid: u32,
        sid: u32,
        start: u64,
        st: St,
        /// Which of HUP, TERM, KILL end it.
        dies: [bool; 3],
        /// An exited child of it stays a zombie.
        keeps_zombies: bool,
    }

    #[derive(Debug, Default)]
    struct World {
        procs: BTreeMap<u32, P>,
    }

    impl World {
        fn add(&mut self, pid: u32, ppid: u32, pgid: u32, sid: u32, start: u64) -> &mut P {
            self.procs.insert(
                pid,
                P {
                    ppid,
                    pgid,
                    sid,
                    start,
                    st: St::Live,
                    dies: [true; 3],
                    keeps_zombies: false,
                },
            );
            self.procs.get_mut(&pid).unwrap()
        }

        /// `child` forked by `parent`: same group and session.
        fn fork(&mut self, parent: u32, child: u32) {
            let p = self.procs[&parent].clone();
            self.add(child, parent, p.pgid, p.sid, p.start + 1);
        }

        fn exit(&mut self, pid: u32) {
            let Some(p) = self.procs.get(&pid).cloned() else {
                return;
            };
            if p.st == St::Zombie {
                return;
            }
            for c in self.procs.values_mut() {
                if c.ppid == pid {
                    c.ppid = 1;
                }
            }
            let keep = p.ppid == ME || self.procs.get(&p.ppid).is_some_and(|q| q.keeps_zombies);
            if keep {
                self.procs.get_mut(&pid).unwrap().st = St::Zombie;
            } else {
                self.procs.remove(&pid);
            }
        }

        fn signal(&mut self, pid: u32, sig: Sig) {
            let Some(p) = self.procs.get(&pid) else {
                return;
            };
            let i = match sig {
                Sig::Hup => 0,
                Sig::Term => 1,
                Sig::Kill => 2,
            };
            if matches!(p.st, St::Live | St::Unreadable) && p.dies[i] {
                self.exit(pid);
            }
        }

        fn alive(&self, pid: u32) -> bool {
            self.procs
                .get(&pid)
                .is_some_and(|p| matches!(p.st, St::Live | St::Unreadable))
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Ev {
        Now,
        Owned,
        PidCount,
        PidList(usize),
        Lookup(u32),
        Getsid(u32),
        Killpg(u32, Sig),
        Kill(u32, Sig),
        Sleep(Duration),
    }

    impl Ev {
        fn is_signal(self) -> bool {
            matches!(self, Ev::Killpg(..) | Ev::Kill(..))
        }
    }

    type Hook = Box<dyn FnMut(&mut World, Ev)>;
    type ListFn = Box<dyn FnMut(usize) -> io::Result<(Vec<u32>, bool)>>;
    type ClockFn = Box<dyn FnMut(Ev) -> Duration>;
    /// Clock advance of every call an event kind matches.
    type Cost = (fn(Ev) -> bool, Duration);

    struct Sim {
        base: Instant,
        now: Cell<Duration>,
        world: RefCell<World>,
        log: RefCell<Vec<Ev>>,
        owned: Cell<bool>,
        lost: Cell<bool>,
        /// Clock advance per call of each kind.
        cost: RefCell<Vec<Cost>>,
        hook: RefCell<Option<Hook>>,
        /// Extra clock advance decided per call.
        clock: RefCell<Option<ClockFn>>,
        list: RefCell<Option<ListFn>>,
        getsid_err: RefCell<BTreeMap<u32, i32>>,
        /// Results for the next ownership checks, taken in order before the normal rule
        /// (a scripted `false` leaves `lost` clear, as a failed `waitid` other than `ECHILD`).
        owned_script: RefCell<std::collections::VecDeque<bool>>,
        /// Ownership checks return the leader's record, as the real check does.
        owned_reads: Cell<bool>,
    }

    impl Sim {
        /// The leader `L` (a child of the daemon), the unrelated `X`, launchd (pid 1,
        /// unreadable), and the daemon itself.
        fn new() -> Sim {
            let mut w = World::default();
            w.add(1, 0, 1, 1, 1).st = St::Unreadable;
            w.add(ME, 1, ME, ME, 10);
            w.add(L, ME, L, L, START);
            w.add(X, 1, X, X, 20);
            Sim {
                base: Instant::now(),
                now: Cell::new(Duration::ZERO),
                world: RefCell::new(w),
                log: RefCell::new(Vec::new()),
                owned: Cell::new(true),
                lost: Cell::new(false),
                cost: RefCell::new(Vec::new()),
                hook: RefCell::new(None),
                clock: RefCell::new(None),
                list: RefCell::new(None),
                getsid_err: RefCell::new(BTreeMap::new()),
                owned_script: RefCell::new(std::collections::VecDeque::new()),
                owned_reads: Cell::new(false),
            }
        }

        /// The record of `pid` as a lookup would read it, without logging a call.
        fn record(&self, pid: u32) -> Lookup {
            let w = self.world.borrow();
            match w.procs.get(&pid) {
                None => Lookup::Gone,
                Some(p) => match p.st {
                    St::Zombie => Lookup::Zombie(p.start),
                    St::ForeignZombie => Lookup::Zombie(0),
                    St::Unreadable => Lookup::Unreadable,
                    St::Live => Lookup::Live(Proc {
                        pid,
                        ppid: p.ppid,
                        pgid: p.pgid,
                        start: p.start,
                    }),
                },
            }
        }

        fn leader(&self) -> Leader {
            Leader {
                pid: L,
                start: Some(START),
                me: ME,
            }
        }

        fn at(&self, ms: u64) -> Instant {
            self.base + Duration::from_millis(ms)
        }

        fn kill(
            &self,
            extra: &[ProcessIdentity],
            budget_ms: u64,
            st: &Mutex<KillState>,
        ) -> KillReport {
            let deadline = self.base + self.now.get() + Duration::from_millis(budget_ms);
            kill_tree(self, self.leader(), extra, deadline, st)
        }

        fn on(&self, f: impl FnMut(&mut World, Ev) + 'static) {
            *self.hook.borrow_mut() = Some(Box::new(f));
        }

        fn cost(&self, kind: fn(Ev) -> bool, d: Duration) {
            self.cost.borrow_mut().push((kind, d));
        }

        fn enter(&self, ev: Ev) {
            self.log.borrow_mut().push(ev);
            if let Some(h) = self.hook.borrow_mut().as_mut() {
                h(&mut self.world.borrow_mut(), ev);
            }
            let jump = self
                .clock
                .borrow_mut()
                .as_mut()
                .map_or(Duration::ZERO, |f| f(ev));
            let add: Duration = jump
                + self
                    .cost
                    .borrow()
                    .iter()
                    .filter(|(k, _)| k(ev))
                    .map(|(_, d)| *d)
                    .sum();
            self.now.set(self.now.get() + add);
        }

        fn alive(&self, pid: u32) -> bool {
            self.world.borrow().alive(pid)
        }

        fn log(&self) -> Vec<Ev> {
            self.log.borrow().clone()
        }

        fn signals(&self) -> Vec<Ev> {
            self.log().into_iter().filter(|e| e.is_signal()).collect()
        }

        fn identity(&self, pid: u32) -> ProcessIdentity {
            let w = self.world.borrow();
            let p = &w.procs[&pid];
            ProcessIdentity {
                pid,
                start_time: p.start.to_string(),
                group: p.pgid.to_string(),
            }
        }
    }

    impl Os for Sim {
        /// The deadline is checked before the leader is read, as in the real check: a read
        /// that uses up the budget still returns ownership.
        fn owned(&self, deadline: Instant) -> (bool, Option<Lookup>) {
            self.enter(Ev::Owned);
            let in_time = self.base + self.now.get() < deadline;
            let read = self.owned_reads.get().then(|| self.lookup(L));
            if let Some(v) = self.owned_script.borrow_mut().pop_front() {
                return (v, read);
            }
            if !self.owned.get() {
                self.lost.set(true);
                return (false, read);
            }
            (in_time, read)
        }

        fn lost(&self) -> bool {
            self.lost.get()
        }

        fn pid_count(&self) -> io::Result<usize> {
            self.enter(Ev::PidCount);
            Ok(self.world.borrow().procs.len())
        }

        fn pid_list(&self, capacity: usize) -> io::Result<(Vec<u32>, bool)> {
            self.enter(Ev::PidList(capacity));
            if let Some(f) = self.list.borrow_mut().as_mut() {
                return f(capacity);
            }
            let pids: Vec<u32> = self.world.borrow().procs.keys().copied().collect();
            let saturated = pids.len() >= capacity;
            Ok((pids.into_iter().take(capacity).collect(), saturated))
        }

        fn lookup(&self, pid: u32) -> Lookup {
            self.enter(Ev::Lookup(pid));
            self.record(pid)
        }

        fn getsid(&self, pid: u32) -> Result<u32, i32> {
            self.enter(Ev::Getsid(pid));
            if let Some(e) = self.getsid_err.borrow().get(&pid) {
                return Err(*e);
            }
            match self.world.borrow().procs.get(&pid) {
                Some(p) if matches!(p.st, St::Live | St::Unreadable) => Ok(p.sid),
                _ => Err(libc::ESRCH),
            }
        }

        fn killpg(&self, pgid: u32, sig: Sig) {
            self.enter(Ev::Killpg(pgid, sig));
            let mut w = self.world.borrow_mut();
            let members: Vec<u32> = w
                .procs
                .iter()
                .filter(|(_, p)| p.pgid == pgid)
                .map(|(pid, _)| *pid)
                .collect();
            for pid in members {
                w.signal(pid, sig);
            }
        }

        fn kill(&self, pid: u32, sig: Sig) {
            self.enter(Ev::Kill(pid, sig));
            self.world.borrow_mut().signal(pid, sig);
        }

        fn now(&self) -> Instant {
            self.enter(Ev::Now);
            self.base + self.now.get()
        }

        fn sleep(&self, d: Duration) {
            self.enter(Ev::Sleep(d));
            self.now.set(self.now.get() + d);
        }
    }

    fn state() -> Mutex<KillState> {
        Mutex::new(KillState::default())
    }

    fn immortal(sim: &Sim, pid: u32) {
        sim.world.borrow_mut().procs.get_mut(&pid).unwrap().dies = [false; 3];
    }

    fn kill_only(sim: &Sim, pid: u32) {
        sim.world.borrow_mut().procs.get_mut(&pid).unwrap().dies = [false, false, true];
    }

    // ---- CL-13 ----

    #[test]
    fn signals_go_in_order() {
        let sim = Sim::new();
        kill_only(&sim, L);
        let r = sim.kill(&[], 5_000, &state());
        assert!(r.tree_dead, "{r:?}");
        assert_eq!(
            sim.signals(),
            vec![
                Ev::Killpg(L, Sig::Hup),
                Ev::Killpg(L, Sig::Term),
                Ev::Killpg(L, Sig::Kill)
            ]
        );
        // Each group signal follows a fresh ownership check, then only a clock read.
        let log = sim.log();
        for (i, e) in log.iter().enumerate() {
            if matches!(e, Ev::Killpg(..)) {
                assert_eq!(&log[i - 2..i], &[Ev::Owned, Ev::Now], "before {e:?}");
            }
        }
        // HUP and TERM are separated by a grace wait.
        let hup = log
            .iter()
            .position(|e| *e == Ev::Killpg(L, Sig::Hup))
            .unwrap();
        let term = log
            .iter()
            .position(|e| *e == Ev::Killpg(L, Sig::Term))
            .unwrap();
        assert!(log[hup..term].iter().any(|e| matches!(e, Ev::Sleep(_))));
        assert!(!sim.alive(L));
        assert!(sim.alive(X));
    }

    #[test]
    fn session_member_outside_the_group_is_killed() {
        let sim = Sim::new();
        // M: same session, its own group (`process_group(0)`).
        sim.world.borrow_mut().add(200, L, 200, L, 2_000);
        let r = sim.kill(&[], 5_000, &state());
        assert!(r.tree_dead, "{r:?}");
        assert!(!sim.alive(200));
        assert!(sim.signals().contains(&Ev::Kill(200, Sig::Hup)));
        assert!(sim.alive(X));
    }

    #[test]
    fn escapee_child_forked_after_its_parent_died_is_killed() {
        let sim = Sim::new();
        // S left the session (setsid) and survives until KILL; once L has died it forks C.
        sim.world.borrow_mut().add(300, L, 300, 300, 3_000);
        kill_only(&sim, 300);
        sim.on(|w, ev| {
            if matches!(ev, Ev::Killpg(L, Sig::Term)) && !w.procs.contains_key(&301) {
                assert!(!w.alive(L));
                w.fork(300, 301);
                w.procs.get_mut(&301).unwrap().dies = [false, false, true];
            }
        });
        let r = sim.kill(&[], 5_000, &state());
        assert!(r.tree_dead, "{r:?}");
        assert!(sim.log().contains(&Ev::Lookup(301)));
        assert!(!sim.alive(300));
        assert!(!sim.alive(301));
        assert!(sim.alive(X));
    }

    #[test]
    fn targets_survive_across_calls() {
        let sim = Sim::new();
        let st = state();
        // S: an escapee that nothing kills in call 1.
        sim.world.borrow_mut().add(300, L, 300, 300, 3_000);
        immortal(&sim, 300);
        let r1 = sim.kill(&[], 200, &st);
        assert!(!r1.tree_dead);
        assert!(r1.survivors.iter().any(|s| s.pid == 300), "{r1:?}");
        assert!(!sim.alive(L));
        // Between the calls S (now a child of launchd) forks C and becomes killable.
        {
            let mut w = sim.world.borrow_mut();
            assert_eq!(w.procs[&300].ppid, 1);
            w.procs.get_mut(&300).unwrap().dies = [true; 3];
            w.fork(300, 301);
        }
        let r2 = sim.kill(&[], 5_000, &st);
        assert!(r2.tree_dead, "{r2:?}");
        assert!(!sim.alive(300));
        assert!(!sim.alive(301));
    }

    #[test]
    fn clock_step_does_not_hide_a_child() {
        let sim = Sim::new();
        // S escaped the session; its child C has a start stamp before its parent's.
        {
            let mut w = sim.world.borrow_mut();
            w.add(300, L, 300, 300, 3_000);
            w.add(301, 300, 300, 300, 5);
        }
        let r = sim.kill(&[], 5_000, &state());
        assert!(r.tree_dead, "{r:?}");
        assert!(!sim.alive(301));
    }

    // ---- CL-14: the census rule ----
    //
    // In each case the leader is no longer listed (so its own exit adds no churn to the
    // opening pass), and A, a session member outside the group, has forked B (absent
    // from the list) before A's outcome. Only a pass that sees B can prove death;
    // accepting the defective pass reports a live B as dead.

    /// A, a live session member outside the group; the leader not in the table.
    fn census_sim() -> Sim {
        let sim = Sim::new();
        {
            let mut w = sim.world.borrow_mut();
            w.procs.remove(&L);
            w.add(200, 1, 200, L, 2_000);
        }
        sim
    }

    /// Runs the kill with `defect` applied at the first matching event, and checks that B
    /// was found and killed.
    fn census_case(sim: &Sim, at: Ev, defect: fn(&mut World)) {
        let fired = std::rc::Rc::new(Cell::new(false));
        let f = fired.clone();
        sim.on(move |w, ev| {
            if ev == at && !f.get() {
                f.set(true);
                // A forks B, then the defect gives A's outcome.
                w.fork(200, 201);
                defect(w);
            }
        });
        // A is first seen in the opening pass, so the defect lands there.
        let r = sim.kill(&[], 5_000, &state());
        assert!(fired.get());
        assert!(!sim.alive(201), "B survived: {r:?}");
        assert!(r.tree_dead, "{r:?}");
    }

    #[test]
    fn gone_at_lookup_is_unclean() {
        census_case(&census_sim(), Ev::Lookup(200), |w| w.exit(200));
    }

    #[test]
    fn new_zombie_is_unclean() {
        let sim = census_sim();
        // A's parent keeps it as a zombie once it exits.
        {
            let mut w = sim.world.borrow_mut();
            w.add(150, 1, 150, 150, 1_500).keeps_zombies = true;
            w.procs.get_mut(&200).unwrap().ppid = 150;
        }
        census_case(&sim, Ev::Lookup(200), |w| w.exit(200));
    }

    #[test]
    fn known_zombie_keeps_the_pass_clean() {
        let sim = Sim::new();
        sim.world.borrow_mut().procs.get_mut(&L).unwrap().st = St::Zombie;
        let mut call = Call {
            os: &sim,
            leader: sim.leader(),
            deadline: sim.at(5_000),
            zombies: BTreeSet::new(),
            owned0: true,
            unknown: BTreeMap::new(),
            last_live: HashMap::new(),
        };
        let mut st = KillState::default();
        let first = call.pass(&mut st);
        assert!(
            !first.clean && first.churn_only,
            "the leader's zombie is new"
        );
        let second = call.pass(&mut st);
        assert!(second.clean, "{second:?}");
    }

    #[test]
    fn esrch_at_getsid_after_live_lookup_is_unclean() {
        census_case(&census_sim(), Ev::Getsid(200), |w| w.exit(200));
    }

    #[test]
    fn esrch_at_getsid_after_unreadable_lookup_is_unclean() {
        let sim = census_sim();
        sim.world.borrow_mut().procs.get_mut(&200).unwrap().st = St::Unreadable;
        census_case(&sim, Ev::Getsid(200), |w| {
            // B is readable; A exits.
            w.procs.get_mut(&201).unwrap().st = St::Live;
            w.exit(200);
        });
    }

    #[test]
    fn other_getsid_failure_is_unclean() {
        let sim = census_sim();
        // A's session cannot be read: it could be a member.
        sim.getsid_err.borrow_mut().insert(200, libc::EPERM);
        let r = sim.kill(&[], 1_000, &state());
        assert!(!r.tree_dead, "{r:?}");
        assert!(sim.alive(200));
        assert!(sim.base + sim.now.get() <= sim.at(1_000));
    }

    #[test]
    fn saturated_list_is_retried_then_not_proof() {
        let sim = Sim::new();
        sim.world.borrow_mut().procs.get_mut(&L).unwrap().st = St::Zombie;
        *sim.list.borrow_mut() = Some(Box::new(|cap| Ok((vec![1; cap], true))));
        let r = sim.kill(&[], 300, &state());
        assert!(!r.tree_dead, "{r:?}");
        let caps: Vec<usize> = sim
            .log()
            .iter()
            .filter_map(|e| match e {
                Ev::PidList(c) => Some(*c),
                _ => None,
            })
            .collect();
        let n = sim.world.borrow().procs.len() + LIST_SLACK;
        assert_eq!(&caps[..4], &[n, 2 * n, 4 * n, 8 * n]);
    }

    #[test]
    fn zero_list_result_is_not_proof() {
        let sim = Sim::new();
        sim.world.borrow_mut().procs.get_mut(&L).unwrap().st = St::Zombie;
        *sim.list.borrow_mut() = Some(Box::new(|_| Ok((Vec::new(), false))));
        let r = sim.kill(&[], 300, &state());
        assert!(!r.tree_dead, "{r:?}");
    }

    #[test]
    fn deadline_cut_pass_is_not_proof() {
        let sim = Sim::new();
        sim.world.borrow_mut().procs.get_mut(&L).unwrap().st = St::Zombie;
        // Each lookup takes 100 ms: no pass over the table fits in the budget.
        sim.cost(|e| matches!(e, Ev::Lookup(_)), Duration::from_millis(100));
        let r = sim.kill(&[], 250, &state());
        assert!(!r.tree_dead, "{r:?}");
    }

    #[test]
    fn churn_is_rescanned_without_sleep() {
        let sim = census_sim();
        sim.on(|w, ev| {
            if ev == Ev::Lookup(200) && w.procs.contains_key(&200) {
                w.exit(200);
            }
        });
        let r = sim.kill(&[], 5_000, &state());
        assert!(r.tree_dead, "{r:?}");
        let log = sim.log();
        assert!(log.iter().filter(|e| matches!(e, Ev::PidCount)).count() >= 2);
        assert!(!log.iter().any(|e| matches!(e, Ev::Sleep(_))), "{log:?}");
        assert!(sim.signals().is_empty());
    }

    // ---- CL-16 ----

    #[test]
    fn ownership_is_checked_before_each_killpg() {
        let sim = Sim::new();
        immortal(&sim, L);
        let _ = sim.kill(&[], 1_000, &state());
        let log = sim.log();
        let groups: Vec<usize> = (0..log.len())
            .filter(|i| matches!(log[*i], Ev::Killpg(..)))
            .collect();
        assert!(groups.len() >= 3);
        for i in groups {
            assert_eq!(&log[i - 2..i], &[Ev::Owned, Ev::Now]);
        }
    }

    #[test]
    fn lost_ownership_sends_no_group_signal() {
        let sim = Sim::new();
        sim.owned.set(false);
        let r = sim.kill(&[], 1_000, &state());
        assert!(!r.tree_dead);
        assert_eq!(r.method, METHOD_LOST);
        assert!(!sim.log().iter().any(|e| matches!(e, Ev::Killpg(..))));
    }

    #[test]
    fn lookup_immediately_precedes_kill() {
        let sim = Sim::new();
        {
            let mut w = sim.world.borrow_mut();
            w.add(200, L, 200, L, 2_000).dies = [false; 3];
            w.add(300, L, 300, 300, 3_000).dies = [false; 3];
        }
        let _ = sim.kill(&[], 1_000, &state());
        let log = sim.log();
        let mut n = 0;
        for (i, e) in log.iter().enumerate() {
            if let Ev::Kill(pid, _) = e {
                n += 1;
                assert_eq!(&log[i - 2..i], &[Ev::Lookup(*pid), Ev::Now]);
            }
        }
        assert!(n >= 6);
    }

    #[test]
    fn reused_pid_in_extra_is_not_signalled() {
        let sim = Sim::new();
        let mut stale = sim.identity(X);
        stale.start_time = "21".to_owned();
        let r = sim.kill(&[stale], 1_000, &state());
        assert!(r.tree_dead, "{r:?}");
        assert!(sim.alive(X));
        assert!(!sim.log().iter().any(|e| matches!(e, Ev::Kill(X, _))));
    }

    #[test]
    fn matching_extra_is_killed() {
        let sim = Sim::new();
        let id = sim.identity(X);
        let r = sim.kill(&[id], 1_000, &state());
        assert!(r.tree_dead, "{r:?}");
        assert!(!sim.alive(X));
    }

    #[test]
    fn init_and_self_are_never_signalled() {
        let sim = Sim::new();
        let init = ProcessIdentity {
            pid: 1,
            start_time: "1".to_owned(),
            group: "1".to_owned(),
        };
        let me = sim.identity(ME);
        let zero = ProcessIdentity {
            pid: 0,
            start_time: "0".to_owned(),
            group: "0".to_owned(),
        };
        let _ = sim.kill(&[init, me, zero], 1_000, &state());
        assert!(
            !sim.log()
                .iter()
                .any(|e| matches!(e, Ev::Kill(0 | 1 | ME, _) | Ev::Killpg(0 | 1 | ME, _)))
        );
        assert!(sim.alive(ME));
    }

    // ---- CL-17 ----

    fn ends_by(sim: &Sim, deadline: Instant) {
        assert!(
            sim.base + sim.now.get() <= deadline,
            "ran past the deadline"
        );
    }

    #[test]
    fn kill_returns_by_its_deadline_with_survivors() {
        let sim = Sim::new();
        immortal(&sim, L);
        let deadline = sim.at(1_000);
        let r = sim.kill(&[], 1_000, &state());
        ends_by(&sim, deadline);
        assert!(!r.tree_dead);
        assert_eq!(r.method, METHOD);
        assert_eq!(r.survivors, vec![sim.identity(L)]);
    }

    #[test]
    fn expired_call_makes_no_os_call() {
        let sim = Sim::new();
        let extra = sim.identity(X);
        let r = sim.kill(std::slice::from_ref(&extra), 0, &state());
        assert!(!r.tree_dead);
        assert!(sim.log().iter().all(|e| *e == Ev::Now), "{:?}", sim.log());
        assert_eq!(
            r.survivors,
            vec![
                sim.identity(L),
                ProcessIdentity {
                    group: String::new(),
                    ..extra
                }
            ]
        );
    }

    #[test]
    fn busy_lock_returns_by_the_deadline() {
        let sim = Sim::new();
        let st = state();
        let _held = st.lock().unwrap();
        let deadline = sim.at(300);
        let r = sim.kill(&[], 300, &st);
        ends_by(&sim, deadline);
        assert_eq!(r.method, METHOD_BUSY);
        assert!(!r.tree_dead);
        assert!(
            sim.log()
                .iter()
                .all(|e| matches!(e, Ev::Now | Ev::Sleep(_))),
            "{:?}",
            sim.log()
        );
    }

    /// A clock that jumps 10 s at the first event `at` after the `n`th event `after`; the
    /// flag shows that it did.
    fn jump_at_after(sim: &Sim, after: Ev, n: usize, at: Ev) -> Rc<Cell<bool>> {
        let fired = Rc::new(Cell::new(false));
        let flag = Rc::clone(&fired);
        let mut seen = 0;
        *sim.clock.borrow_mut() = Some(Box::new(move |ev| {
            if ev == after {
                seen += 1;
            } else if seen >= n && ev == at && !flag.get() {
                flag.set(true);
                return Duration::from_secs(10);
            }
            Duration::ZERO
        }));
        fired
    }

    #[test]
    fn lookup_consuming_the_budget_stops_before_the_signal() {
        // 300, a child of the leader outside its group, is signalled by pid. The lookup of
        // 300 that would authorise that signal (the first one after the HUP to the group)
        // shows the recorded start and ends past the deadline.
        let sim = Sim::new();
        sim.world.borrow_mut().add(300, L, 300, 300, 3_000);
        let fired = jump_at_after(&sim, Ev::Killpg(L, Sig::Hup), 1, Ev::Lookup(300));
        let r = sim.kill(&[], 1_000, &state());
        assert!(fired.get(), "{:?}", sim.log());
        assert!(!r.tree_dead);
        assert!(
            !sim.log().iter().any(|e| matches!(e, Ev::Kill(300, _))),
            "{:?}",
            sim.log()
        );
    }

    #[test]
    fn deadline_inside_owned_stops_before_killpg() {
        // The ownership check before the first group signal (the third check of the call)
        // holds, but its read of the leader ends past the deadline.
        let sim = Sim::new();
        immortal(&sim, L);
        sim.owned_reads.set(true);
        let fired = jump_at_after(&sim, Ev::Owned, 3, Ev::Lookup(L));
        let r = sim.kill(&[], 500, &state());
        assert!(fired.get(), "{:?}", sim.log());
        assert!(!r.tree_dead);
        assert!(
            !sim.log().iter().any(|e| matches!(e, Ev::Killpg(..))),
            "{:?}",
            sim.log()
        );
        // The opening pass saw the live leader; the report keeps it.
        assert_eq!(r.survivors, vec![sim.identity(L)]);
    }

    #[test]
    fn failed_list_keeps_the_last_survivors() {
        // An unreadable session member is never recorded as a target (it has no start),
        // so only a pass finds it. Once the list fails, no pass can run; the report keeps
        // the member seen before.
        const U: u32 = 200;
        let sim = Sim::new();
        sim.world.borrow_mut().add(U, L, U, L, 30).st = St::Unreadable;
        immortal(&sim, U);
        let failing = std::rc::Rc::new(Cell::new(false));
        let f2 = failing.clone();
        sim.on(move |_, ev| {
            if matches!(ev, Ev::Killpg(..)) {
                f2.set(true);
            }
        });
        *sim.list.borrow_mut() = Some(Box::new(move |_| {
            Ok((
                if failing.get() {
                    Vec::new()
                } else {
                    vec![1, ME, L, U, X]
                },
                false,
            ))
        }));
        let r = sim.kill(&[], 500, &state());
        assert!(!r.tree_dead, "{r:?}");
        assert!(r.survivors.iter().any(|s| s.pid == U), "{r:?}");
    }

    #[test]
    fn deadline_between_list_retries_stops_the_pass() {
        let sim = Sim::new();
        *sim.list.borrow_mut() = Some(Box::new(|cap| Ok((vec![1; cap], true))));
        sim.cost(|e| matches!(e, Ev::PidList(_)), Duration::from_millis(100));
        let deadline = sim.at(150);
        let r = sim.kill(&[], 150, &state());
        assert!(!r.tree_dead);
        // The list call that crossed the deadline is the last one.
        let lists = sim
            .log()
            .iter()
            .filter(|e| matches!(e, Ev::PidList(_)))
            .count();
        assert_eq!(lists, 2, "{:?}", sim.log());
        let _ = deadline;
    }

    #[test]
    fn sleeps_are_capped_to_the_budget() {
        let sim = Sim::new();
        immortal(&sim, L);
        let deadline = sim.at(130);
        let r = sim.kill(&[], 130, &state());
        assert!(!r.tree_dead);
        ends_by(&sim, deadline);
        assert!(sim.log().iter().any(|e| matches!(e, Ev::Sleep(_))));
    }

    #[test]
    fn unreadable_target_is_an_unresolved_survivor() {
        let sim = Sim::new();
        let id = sim.identity(X);
        immortal(&sim, X);
        // The extra turns unreadable (EPERM) once it has been accepted.
        sim.on(|w, ev| {
            if ev == Ev::PidCount {
                w.procs.get_mut(&X).unwrap().st = St::Unreadable;
            }
        });
        let r = sim.kill(&[id], 1_000, &state());
        assert!(!r.tree_dead, "{r:?}");
        assert!(r.survivors.iter().any(|s| s.pid == X), "{r:?}");
    }

    #[test]
    fn small_budget_sends_kill_only() {
        let sim = Sim::new();
        let r = sim.kill(&[], 100, &state());
        assert!(r.tree_dead, "{r:?}");
        assert_eq!(sim.signals(), vec![Ev::Killpg(L, Sig::Kill)]);
    }

    // ---- CL-18 ----

    #[test]
    fn three_calls() {
        let sim = Sim::new();
        let st = state();
        let r1 = sim.kill(&[], 5_000, &st);
        assert!(r1.tree_dead, "{r1:?}");
        // A live extra whose signals have no effect.
        immortal(&sim, X);
        let x = sim.identity(X);
        let r2 = sim.kill(std::slice::from_ref(&x), 500, &st);
        assert!(!r2.tree_dead, "{r2:?}");
        assert!(r2.survivors.iter().any(|s| s.pid == X));
        let r3 = sim.kill(&[], 0, &st);
        assert!(!r3.tree_dead, "{r3:?}");
        assert!(r3.survivors.iter().any(|s| s.pid == X), "{r3:?}");
    }

    #[test]
    fn dead_tree_returns_without_signal() {
        let sim = Sim::new();
        let st = state();
        assert!(sim.kill(&[], 5_000, &st).tree_dead);
        sim.log.borrow_mut().clear();
        let r = sim.kill(&[], 5_000, &st);
        assert!(r.tree_dead, "{r:?}");
        assert!(sim.signals().is_empty());
        assert!(!sim.log().iter().any(|e| matches!(e, Ev::Sleep(_))));
    }

    #[test]
    fn resolved_targets_are_pruned() {
        let sim = Sim::new();
        let st = state();
        sim.world.borrow_mut().add(300, L, 300, 300, 3_000);
        let r = sim.kill(&[sim.identity(X)], 5_000, &st);
        assert!(r.tree_dead, "{r:?}");
        let left = st.lock().unwrap().targets.clone();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn list_omitting_a_live_leader_is_not_proof() {
        // A list the kernel truncated (concurrent process creation) leaves out the live
        // leader, and is not marked saturated.
        let sim = Sim::new();
        immortal(&sim, L);
        *sim.list.borrow_mut() = Some(Box::new(|_| Ok((vec![1, X, ME], false))));
        let r = sim.kill(&[], 500, &state());
        assert!(!r.tree_dead, "{r:?}");
        assert!(r.survivors.iter().any(|s| s.pid == L), "{r:?}");
        // A leader the recheck finds is killed, then proven dead by the recheck.
        let sim = Sim::new();
        *sim.list.borrow_mut() = Some(Box::new(|_| Ok((vec![1, X, ME], false))));
        let r = sim.kill(&[], 500, &state());
        assert!(r.tree_dead, "{r:?}");
        assert!(!sim.alive(L));
    }

    #[test]
    fn member_cut_from_the_extension_is_tracked_by_the_next_call() {
        // M, a live child of the leader in its group, is read by the census; the clock
        // passes the deadline right after the ownership check that follows the pass.
        let sim = Sim::new();
        sim.world.borrow_mut().add(200, L, L, L, 2_000).dies = [false; 3];
        jump_after_owned_following(&sim, 200);
        let st = state();
        let r = sim.kill(&[], 1_000, &st);
        assert!(!r.tree_dead, "{r:?}");
        assert!(r.survivors.contains(&sim.identity(200)), "{r:?}");
        // Between the calls M leaves the session and the leader exits.
        {
            let mut w = sim.world.borrow_mut();
            let m = w.procs.get_mut(&200).unwrap();
            (m.ppid, m.pgid, m.sid) = (1, 200, 200);
            w.procs.get_mut(&L).unwrap().st = St::Zombie;
        }
        *sim.clock.borrow_mut() = None;
        let r = sim.kill(&[], 1_000, &st);
        assert!(!r.tree_dead, "{r:?}");
        assert!(sim.alive(200));
    }

    /// The recorded escapee E (its parent gone, its own session) and a recorded target
    /// that sorts after it and no longer exists; the leader has exited. Every list omits E.
    fn omitted_escapee() -> (Sim, Vec<ProcessIdentity>) {
        const E: u32 = 300;
        let sim = Sim::new();
        sim.world.borrow_mut().procs.get_mut(&L).unwrap().st = St::Zombie;
        sim.world.borrow_mut().add(E, 1, E, E, 3_000).dies = [false; 3];
        *sim.list.borrow_mut() = Some(Box::new(|_| Ok((vec![1, ME, L, X], false))));
        let extra = vec![
            sim.identity(E),
            ProcessIdentity {
                pid: 400,
                start_time: "4000".into(),
                group: "400".into(),
            },
        ];
        (sim, extra)
    }

    #[test]
    fn recheck_cut_after_finding_a_target_keeps_it() {
        // The recheck finds E alive; the deadline passes before the next identity.
        let (sim, extra) = omitted_escapee();
        *sim.clock.borrow_mut() = Some(Box::new(|ev| {
            if ev == Ev::Lookup(300) {
                Duration::from_secs(10)
            } else {
                Duration::ZERO
            }
        }));
        let st = state();
        let r = sim.kill(&extra, 1_000, &st);
        assert!(!r.tree_dead, "{r:?}");
        assert!(
            r.survivors
                .iter()
                .any(|s| s.pid == 300 && s.start_time == "3000"),
            "{r:?}"
        );
        // A later call with a complete list still tracks E.
        *sim.clock.borrow_mut() = None;
        *sim.list.borrow_mut() = None;
        let r = sim.kill(&[], 1_000, &st);
        assert!(!r.tree_dead, "{r:?}");
        assert!(sim.alive(300));
    }

    #[test]
    fn absence_from_the_list_does_not_prune_a_target() {
        // After ownership is lost the call returns, before its deadline, as soon as no
        // target is live in a pass. E was only absent from the lists, so it stays a
        // target; the exited leader, which a lookup showed a zombie, is pruned.
        let (sim, extra) = omitted_escapee();
        sim.owned.set(false);
        sim.lost.set(true);
        let st = state();
        let r = sim.kill(&extra, 1_000, &st);
        assert!(!r.tree_dead, "{r:?}");
        assert!(sim.now.get() < Duration::from_millis(1_000));
        let targets = st.lock().unwrap().targets.clone();
        assert!(targets.contains_key(&(300, 3_000)), "{targets:?}");
        assert!(!targets.contains_key(&(L, START)), "{targets:?}");
        // A later call with a complete list still names E.
        *sim.list.borrow_mut() = None;
        let r = sim.kill(&[], 1_000, &st);
        assert!(r.survivors.iter().any(|s| s.pid == 300), "{r:?}");
        assert!(sim.alive(300));
    }

    #[test]
    fn member_first_seen_in_an_escalation_census_stays_reported() {
        // U, an unreadable session member (never a target: no start), appears after the
        // HUP; the list fails from the first KILL on.
        const U: u32 = 200;
        let sim = Sim::new();
        immortal(&sim, L);
        let phase = std::rc::Rc::new(Cell::new(0u8));
        let p2 = phase.clone();
        sim.on(move |w, ev| match ev {
            Ev::Killpg(_, Sig::Hup) if p2.get() == 0 => {
                let u = w.add(U, L, U, L, 30);
                u.st = St::Unreadable;
                u.dies = [false; 3];
                p2.set(1);
            }
            Ev::Killpg(_, Sig::Kill) => p2.set(2),
            _ => {}
        });
        *sim.list.borrow_mut() = Some(Box::new(move |_| {
            Ok((
                match phase.get() {
                    0 => vec![1, ME, L, X],
                    1 => vec![1, ME, L, X, U],
                    _ => Vec::new(),
                },
                false,
            ))
        }));
        let r = sim.kill(&[], 5_000, &state());
        assert!(!r.tree_dead, "{r:?}");
        assert!(r.survivors.iter().any(|s| s.pid == U), "{r:?}");
    }

    #[test]
    fn foreign_zombie_on_a_quiet_host_proves_death() {
        // Another user's zombie stays in every list. Confirmed as a zombie by the short
        // record, it is churn only the first time.
        let sim = Sim::new();
        sim.world.borrow_mut().add(600, 1, 600, 600, 0).st = St::ForeignZombie;
        // Each pass takes 1 ms, so a regression that rescans forever reaches the deadline.
        sim.cost(|e| e == Ev::PidCount, Duration::from_millis(1));
        let r = sim.kill(&[], 5_000, &state());
        assert!(r.tree_dead, "{r:?}");
        assert!(!sim.alive(L));
        // When the zombie cannot be confirmed (unreadable, `getsid` ESRCH), every pass
        // sees churn and death is not proven: the conservative rule.
        let sim = Sim::new();
        sim.world.borrow_mut().add(600, 1, 600, 600, 0).st = St::Unreadable;
        sim.getsid_err.borrow_mut().insert(600, libc::ESRCH);
        // Each pass takes 1 ms, so the immediate rescans reach the deadline.
        sim.cost(|e| e == Ev::PidCount, Duration::from_millis(1));
        let r = sim.kill(&[], 500, &state());
        assert!(!r.tree_dead, "{r:?}");
    }

    #[test]
    fn expired_call_keeps_a_new_extra_for_the_next_call() {
        let sim = Sim::new();
        let st = state();
        assert!(sim.kill(&[], 5_000, &st).tree_dead);
        let x = sim.identity(X);
        let r = sim.kill(std::slice::from_ref(&x), 0, &st);
        assert!(!r.tree_dead);
        assert!(r.survivors.iter().any(|s| s.pid == X), "{r:?}");
        // The next call, without the extra, still kills it.
        let r = sim.kill(&[], 5_000, &st);
        assert!(r.tree_dead, "{r:?}");
        assert!(!sim.alive(X), "the extra of the expired call was dropped");
    }

    #[test]
    fn start_changed_before_the_signal_is_not_signalled() {
        // X matches its extra at census time; before the signal's own lookup, X exits and
        // another process takes its pid.
        let sim = Sim::new();
        let x = sim.identity(X);
        let mut censused = false;
        sim.on(move |w, ev| match ev {
            Ev::Getsid(X) => censused = true,
            Ev::Lookup(X) if censused => {
                censused = false;
                if w.procs.get(&X).is_some_and(|p| p.start == 20) {
                    w.procs.remove(&X);
                    w.add(X, 1, X, X, 21);
                }
            }
            _ => {}
        });
        let _ = sim.kill(&[x], 1_000, &state());
        assert!(
            !sim.log().iter().any(|e| matches!(e, Ev::Kill(X, _))),
            "{:?}",
            sim.signals()
        );
        assert!(sim.alive(X));
        assert_eq!(sim.world.borrow().procs[&X].start, 21);
    }

    // ---- Retention at the moment of each read ----

    /// A clock that jumps 10 s at the first clock read after an ownership check that
    /// follows `getsid(after)`: the census has read `after` by then.
    fn jump_after_owned_following(sim: &Sim, after: u32) {
        let mut stage = 0;
        *sim.clock.borrow_mut() = Some(Box::new(move |ev| {
            match (stage, ev) {
                (0, Ev::Getsid(p)) if p == after => stage = 1,
                (1, Ev::Owned) => stage = 2,
                (2, Ev::Now) => {
                    stage = 3;
                    return Duration::from_secs(10);
                }
                _ => {}
            }
            Duration::ZERO
        }));
    }

    /// A clock that jumps 10 s right after the first event `at`.
    fn jump_at(sim: &Sim, at: Ev) {
        let mut done = false;
        *sim.clock.borrow_mut() = Some(Box::new(move |ev| {
            if !done && ev == at {
                done = true;
                Duration::from_secs(10)
            } else {
                Duration::ZERO
            }
        }));
    }

    /// Between two calls `pid` leaves the session (`setsid`, reparented to launchd) and
    /// its parent exits; the clock is a plain one again.
    fn leave_and_orphan(sim: &Sim, pid: u32, parent_gone: impl FnOnce(&mut World)) {
        let mut w = sim.world.borrow_mut();
        let m = w.procs.get_mut(&pid).unwrap();
        (m.ppid, m.pgid, m.sid) = (1, pid, pid);
        parent_gone(&mut w);
        drop(w);
        *sim.clock.borrow_mut() = None;
    }

    /// The second call, with a complete list, still tracks `pid`.
    fn retry_tracks(sim: &Sim, st: &Mutex<KillState>, pid: u32) {
        let r = sim.kill(&[], 1_000, st);
        assert!(!r.tree_dead, "{r:?}");
        assert!(sim.alive(pid));
        assert!(r.survivors.iter().any(|s| s.pid == pid), "{r:?}");
    }

    #[test]
    fn member_read_before_getsid_is_retained() {
        // The deadline passes right after the census read M, before its `getsid`. M was
        // reparented (its parent exited), so only its group covers it.
        let sim = Sim::new();
        sim.world.borrow_mut().add(200, 1, L, L, 2_000).dies = [false; 3];
        jump_at(&sim, Ev::Lookup(200));
        let st = state();
        assert!(!sim.kill(&[], 1_000, &st).tree_dead);
        leave_and_orphan(&sim, 200, |w| w.procs.get_mut(&L).unwrap().st = St::Zombie);
        retry_tracks(&sim, &st, 200);
    }

    #[test]
    fn member_read_before_the_ownership_check_is_retained() {
        // The deadline passes right after M's `getsid`, before any later ownership check.
        // M is reparented and in another group of the session: only its session covers
        // it.
        let sim = Sim::new();
        sim.world.borrow_mut().add(200, 1, 150, L, 2_000).dies = [false; 3];
        jump_at(&sim, Ev::Getsid(200));
        let st = state();
        assert!(!sim.kill(&[], 1_000, &st).tree_dead);
        leave_and_orphan(&sim, 200, |w| w.procs.get_mut(&L).unwrap().st = St::Zombie);
        retry_tracks(&sim, &st, 200);
    }

    #[test]
    fn descendant_of_a_tracked_parent_is_retained_when_read() {
        // P, a recorded escapee, is alive; M, its child, has already left P's session.
        // The census reads both; the deadline passes right after (before any later walk).
        let sim = Sim::new();
        sim.world.borrow_mut().procs.get_mut(&L).unwrap().st = St::Zombie;
        sim.world.borrow_mut().add(300, 1, 300, 300, 3_000).dies = [false; 3];
        sim.world.borrow_mut().add(310, 300, 310, 310, 3_100).dies = [false; 3];
        jump_after_owned_following(&sim, 310);
        let st = state();
        let r = sim.kill(&[sim.identity(300)], 1_000, &st);
        assert!(!r.tree_dead, "{r:?}");
        leave_and_orphan(&sim, 310, |w| {
            w.procs.remove(&300);
        });
        retry_tracks(&sim, &st, 310);
    }

    #[test]
    fn stale_extra_at_a_reused_pid_does_not_hide_a_live_member() {
        // `extra` names an older process at pid 200; M now has that pid and is a member.
        let sim = Sim::new();
        sim.world.borrow_mut().add(200, L, L, L, 2_000).dies = [false; 3];
        jump_after_owned_following(&sim, 200);
        let stale = ProcessIdentity {
            pid: 200,
            start_time: "999".into(),
            group: "200".into(),
        };
        let st = state();
        assert!(!sim.kill(&[stale], 1_000, &st).tree_dead);
        // The read of M disproved the stale generation and retained M's.
        let targets = st.lock().unwrap().targets.clone();
        assert!(!targets.contains_key(&(200, 999)), "{targets:?}");
        assert!(targets.contains_key(&(200, 2_000)), "{targets:?}");
        leave_and_orphan(&sim, 200, |w| w.procs.get_mut(&L).unwrap().st = St::Zombie);
        retry_tracks(&sim, &st, 200);
    }

    #[test]
    fn deadline_exits_do_not_keep_confirmed_zombies() {
        // U, another user's helper in the session, keeps every call from a proof; each
        // call names a new `extra` that is a zombie during the call and reaped after it.
        let sim = Sim::new();
        {
            let mut w = sim.world.borrow_mut();
            let u = w.add(200, L, 200, L, 30);
            u.st = St::Unreadable;
            u.dies = [false; 3];
            w.procs.get_mut(&L).unwrap().st = St::Zombie;
        }
        sim.cost(|e| e == Ev::PidCount, Duration::from_millis(1));
        let st = state();
        for i in 0..5 {
            let pid = 600 + i;
            sim.world
                .borrow_mut()
                .add(pid, 1, pid, pid, 6_000 + u64::from(i));
            let id = sim.identity(pid);
            sim.world.borrow_mut().procs.get_mut(&pid).unwrap().st = St::Zombie;
            let r = sim.kill(&[id], 300, &st);
            assert!(!r.tree_dead, "{r:?}");
            sim.world.borrow_mut().procs.remove(&pid);
            let targets = st.lock().unwrap().targets.clone();
            assert!(
                targets.keys().all(|(p, _)| *p < 600),
                "call {i}: {targets:?}"
            );
        }
    }

    #[test]
    fn stage_5_leader_found_by_the_recheck_keeps_its_start() {
        // The leader's start was never recorded; every list omits it, so only the recheck
        // reads it, live.
        let sim = Sim::new();
        immortal(&sim, L);
        *sim.list.borrow_mut() = Some(Box::new(|_| Ok((vec![1, ME, X], false))));
        let leader = Leader {
            start: None,
            ..sim.leader()
        };
        let r = kill_tree(&sim, leader, &[], sim.at(300), &state());
        assert!(!r.tree_dead, "{r:?}");
        assert!(
            r.survivors
                .iter()
                .any(|s| s.pid == L && s.start_time == START.to_string()),
            "{r:?}"
        );
    }

    #[test]
    fn retained_escapee_omitted_from_a_later_list_stays_reported() {
        // E, a recorded escapee, is listed by the opening census only; the deadline passes
        // at the KILL, before any proof could look E up again.
        const E: u32 = 300;
        let sim = Sim::new();
        immortal(&sim, L);
        sim.world.borrow_mut().add(E, 1, E, E, 3_000).dies = [false; 3];
        let phase = std::rc::Rc::new(Cell::new(false));
        let p2 = phase.clone();
        sim.on(move |_, ev| {
            if matches!(ev, Ev::Killpg(_, Sig::Hup)) {
                p2.set(true);
            }
        });
        *sim.list.borrow_mut() = Some(Box::new(move |_| {
            Ok((
                if phase.get() {
                    vec![1, ME, L, X]
                } else {
                    vec![1, ME, L, E, X]
                },
                false,
            ))
        }));
        jump_at(&sim, Ev::Killpg(L, Sig::Kill));
        let r = sim.kill(&[sim.identity(E)], 5_000, &state());
        assert!(!r.tree_dead, "{r:?}");
        assert!(r.survivors.iter().any(|s| s.pid == E), "{r:?}");
    }

    #[test]
    fn child_listed_before_its_parent_is_covered() {
        // C (pid 250) is the `setsid` child of P (pid 300), a recorded escapee; the list
        // reaches C first, so C is covered only once the same pass reads P.
        let sim = Sim::new();
        sim.world.borrow_mut().procs.get_mut(&L).unwrap().st = St::Zombie;
        sim.world.borrow_mut().add(300, 1, 300, 300, 3_000);
        sim.world.borrow_mut().add(250, 300, 250, 250, 3_100);
        let r = sim.kill(&[sim.identity(300)], 5_000, &state());
        assert!(r.tree_dead, "{r:?}");
        assert!(!sim.alive(250));
        assert!(!sim.alive(300));
    }

    #[test]
    fn membership_needs_ownership_for_the_call() {
        // Ownership cannot be established: a live process in group and session `L` (which
        // may then name another process's group) is not covered, never signalled, and
        // death is not proven.
        let sim = Sim::new();
        sim.owned.set(false);
        sim.world.borrow_mut().procs.get_mut(&L).unwrap().st = St::Zombie;
        sim.world.borrow_mut().add(200, 1, L, L, 2_000);
        let st = state();
        let r = sim.kill(&[], 300, &st);
        assert!(!r.tree_dead, "{r:?}");
        let targets = st.lock().unwrap().targets.clone();
        assert!(!targets.contains_key(&(200, 2_000)), "{targets:?}");
        assert!(sim.alive(200));
        assert!(
            !sim.log().iter().any(|e| matches!(e, Ev::Kill(200, _))),
            "{:?}",
            sim.log()
        );
    }

    #[test]
    fn recheck_that_retains_a_new_generation_is_not_proof() {
        // `extra` names an older process at pid 200; M, a live member, now has that pid and
        // every list omits it. The leader has exited. The recheck of the stale identity
        // reads M: M is retained, so nothing proves death.
        let sim = Sim::new();
        sim.world.borrow_mut().procs.get_mut(&L).unwrap().st = St::Zombie;
        sim.world.borrow_mut().add(200, 1, L, L, 2_000).dies = [false; 3];
        *sim.list.borrow_mut() = Some(Box::new(|_| Ok((vec![1, ME, L, X], false))));
        let stale = ProcessIdentity {
            pid: 200,
            start_time: "999".into(),
            group: "200".into(),
        };
        let st = state();
        let r = sim.kill(&[stale], 1_000, &st);
        assert!(!r.tree_dead, "{r:?}");
        assert!(sim.alive(200));
        assert!(
            r.survivors
                .iter()
                .any(|s| s.pid == 200 && s.start_time == "2000"),
            "{r:?}"
        );
    }

    #[test]
    fn member_needs_ownership_established_at_the_start() {
        // The first ownership check fails without loss; later ones hold. M (session `L`,
        // its own group) is never retained or signalled by pid.
        let sim = Sim::new();
        sim.owned_script.borrow_mut().push_back(false);
        // Each pass takes 1 ms, so passes that cannot prove anything reach the deadline.
        sim.cost(|e| e == Ev::PidCount, Duration::from_millis(1));
        sim.world.borrow_mut().procs.get_mut(&L).unwrap().st = St::Zombie;
        sim.world.borrow_mut().add(200, 1, 200, L, 2_000);
        let st = state();
        let r = sim.kill(&[], 300, &st);
        assert!(!r.tree_dead, "{r:?}");
        let targets = st.lock().unwrap().targets.clone();
        assert!(!targets.contains_key(&(200, 2_000)), "{targets:?}");
        assert!(
            !sim.log().iter().any(|e| matches!(e, Ev::Kill(200, _))),
            "{:?}",
            sim.log()
        );
    }

    #[test]
    fn proof_needs_ownership_established_at_the_start() {
        // The first ownership check fails without loss; later ones hold. The tree is
        // already dead and every pass is clean, but death is not proven.
        let sim = Sim::new();
        sim.owned_script.borrow_mut().push_back(false);
        // Each pass takes 1 ms, so passes that cannot prove anything reach the deadline.
        sim.cost(|e| e == Ev::PidCount, Duration::from_millis(1));
        sim.world.borrow_mut().procs.get_mut(&L).unwrap().st = St::Zombie;
        let r = sim.kill(&[], 300, &state());
        assert!(!r.tree_dead, "{r:?}");
    }

    #[test]
    fn resolved_child_waiting_for_its_parent_is_not_retained() {
        // C (pid 250), the `setsid` child of the recorded escapee P (pid 300), is read
        // first and waits for P; C exits before its `getsid`. Reading P must not bring C
        // back. The deadline passes right after P is read.
        let sim = Sim::new();
        sim.world.borrow_mut().procs.get_mut(&L).unwrap().st = St::Zombie;
        sim.world.borrow_mut().add(300, 1, 300, 300, 3_000).dies = [false; 3];
        sim.world.borrow_mut().add(250, 300, 250, 250, 2_500);
        sim.on(|w, ev| {
            if ev == Ev::Getsid(250) {
                w.exit(250);
            }
        });
        jump_at(&sim, Ev::Lookup(300));
        let st = state();
        let r = sim.kill(&[sim.identity(300)], 1_000, &st);
        assert!(!r.tree_dead, "{r:?}");
        let targets = st.lock().unwrap().targets.clone();
        assert!(!targets.contains_key(&(250, 2_500)), "{targets:?}");
        assert!(targets.contains_key(&(300, 3_000)), "{targets:?}");
    }

    #[test]
    fn busy_and_expired_reports_list_each_pid_once() {
        let sim = Sim::new();
        let extra = [sim.identity(L), sim.identity(X), sim.identity(X)];
        let once = |r: &KillReport| {
            let pids: Vec<u32> = r.survivors.iter().map(|s| s.pid).collect();
            let unique: BTreeSet<u32> = pids.iter().copied().collect();
            assert_eq!(pids.len(), unique.len(), "{r:?}");
        };
        // Expired, with the state free.
        let st = state();
        once(&sim.kill(&extra, 0, &st));
        // Busy.
        let held = state();
        let _g = held.lock().unwrap();
        once(&sim.kill(&extra, 100, &held));
    }

    #[test]
    fn ownership_check_read_is_applied_to_the_targets() {
        // The check at the call's start reads the leader, a zombie, and uses up the budget
        // before any census: that read alone resolves the leader.
        let sim = Sim::new();
        sim.owned_reads.set(true);
        sim.world.borrow_mut().procs.get_mut(&L).unwrap().st = St::Zombie;
        sim.cost(|e| e == Ev::Owned, Duration::from_millis(1_000));
        let st = state();
        let r = sim.kill(&[], 500, &st);
        assert!(!r.tree_dead, "{r:?}");
        let lookups: Vec<Ev> = sim
            .log()
            .into_iter()
            .filter(|e| matches!(e, Ev::Lookup(_)))
            .collect();
        assert_eq!(lookups, vec![Ev::Lookup(L)], "only the check's own read");
        assert!(r.survivors.iter().all(|s| s.pid != L), "{r:?}");
        assert!(st.lock().unwrap().targets.is_empty());
    }

    #[test]
    fn resolved_pending_parent_still_leads_to_its_child() {
        // The list reaches D (pid 260), then its parent C (pid 280), then C's parent P (pid
        // 300), a recorded escapee. C exits before its `getsid`. Reading P retains D through
        // the ancestry already read, but not C. The deadline passes right after P is read.
        let sim = Sim::new();
        sim.world.borrow_mut().procs.get_mut(&L).unwrap().st = St::Zombie;
        sim.world.borrow_mut().add(300, 1, 300, 300, 3_000).dies = [false; 3];
        sim.world.borrow_mut().add(280, 300, 280, 280, 2_800);
        sim.world.borrow_mut().add(260, 280, 260, 260, 2_600).dies = [false; 3];
        sim.on(|w, ev| {
            if ev == Ev::Getsid(280) {
                w.exit(280);
            }
        });
        jump_at(&sim, Ev::Lookup(300));
        let st = state();
        let r = sim.kill(&[sim.identity(300)], 1_000, &st);
        assert!(!r.tree_dead, "{r:?}");
        let targets = st.lock().unwrap().targets.clone();
        assert!(!targets.contains_key(&(280, 2_800)), "{targets:?}");
        assert!(targets.contains_key(&(260, 2_600)), "{targets:?}");
    }
}
