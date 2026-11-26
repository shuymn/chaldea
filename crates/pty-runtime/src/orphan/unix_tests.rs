//! PR10a2-CL-1: the macOS loop over a scripted `OrphanOs` and a scripted clock.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use super::Fate;
use super::unix::{OrphanOs, kill_recorded};
use crate::backend::ProcessIdentity;
use crate::unix::kill::Sig;
use crate::unix::sys::BsdInfo;

const ME: u32 = 50;
const P: u32 = 100;
const START: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum C {
    Info(u32),
    Short(u32),
    Kill(u32, Sig),
    Sleep(Duration),
}

/// One scripted read result.
#[derive(Debug, Clone, Copy)]
enum R {
    Live(u64),
    Zombie,
    Gone,
    Refused,
}

#[derive(Default)]
struct Proc {
    /// Read results in order; once empty, `then` answers every read.
    reads: VecDeque<R>,
    then: Option<R>,
    /// The signal that ends the process (it is `Gone` afterwards); `None`: immortal.
    dies_on: Option<Sig>,
    dead: bool,
}

struct Sim {
    base: Instant,
    now: Cell<Duration>,
    /// Clock advance of each `bsd_info` call.
    info_cost: Cell<Duration>,
    procs: RefCell<HashMap<u32, Proc>>,
    calls: RefCell<Vec<(Duration, C)>>,
}

impl Sim {
    fn new() -> Sim {
        Sim {
            base: Instant::now(),
            now: Cell::new(Duration::ZERO),
            info_cost: Cell::new(Duration::ZERO),
            procs: RefCell::new(HashMap::new()),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn add(&self, pid: u32, reads: &[R], then: R, dies_on: Option<Sig>) {
        self.procs.borrow_mut().insert(
            pid,
            Proc {
                reads: reads.iter().copied().collect(),
                then: Some(then),
                dies_on,
                dead: false,
            },
        );
    }

    fn at(&self, d: Duration) -> Instant {
        self.base + d
    }

    fn call(&self, c: C) {
        self.calls.borrow_mut().push((self.now.get(), c));
    }

    fn calls(&self) -> Vec<C> {
        self.calls.borrow().iter().map(|(_, c)| *c).collect()
    }

    fn kills(&self) -> Vec<(u32, Sig)> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                C::Kill(p, s) => Some((p, s)),
                _ => None,
            })
            .collect()
    }

    fn next_read(&self, pid: u32) -> R {
        let mut procs = self.procs.borrow_mut();
        let Some(p) = procs.get_mut(&pid) else {
            return R::Gone;
        };
        if p.dead {
            return R::Gone;
        }
        p.reads.pop_front().or(p.then).unwrap_or(R::Gone)
    }

    /// Every native call (a sleep is not one) started before `deadline`.
    fn assert_no_call_at_or_after(&self, deadline: Duration) {
        for (at, c) in self.calls.borrow().iter() {
            if !matches!(c, C::Sleep(_)) {
                assert!(*at < deadline, "{c:?} at {at:?}, deadline {deadline:?}");
            }
        }
    }
}

fn info(pid: u32, start: u64, zombie: bool) -> BsdInfo {
    BsdInfo {
        pid,
        ppid: 1,
        pgid: pid,
        zombie,
        start,
        name: [0; 32],
    }
}

impl OrphanOs for Sim {
    fn bsd_info(&self, pid: u32) -> Result<BsdInfo, i32> {
        self.call(C::Info(pid));
        let r = self.next_read(pid);
        self.now.set(self.now.get() + self.info_cost.get());
        match r {
            R::Live(s) => Ok(info(pid, s, false)),
            R::Zombie => Ok(info(pid, START, true)),
            R::Gone => Err(libc::ESRCH),
            R::Refused => Err(libc::EPERM),
        }
    }

    fn short_zombie(&self, pid: u32) -> Result<bool, i32> {
        self.call(C::Short(pid));
        Ok(false)
    }

    fn kill(&self, pid: u32, sig: Sig) {
        self.call(C::Kill(pid, sig));
        if let Some(p) = self.procs.borrow_mut().get_mut(&pid)
            && p.dies_on == Some(sig)
        {
            p.dead = true;
        }
    }

    fn now(&self) -> Instant {
        self.base + self.now.get()
    }

    fn sleep(&self, d: Duration) {
        self.call(C::Sleep(d));
        self.now.set(self.now.get() + d);
    }
}

fn id(pid: u32, start: &str) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start_time: start.to_owned(),
        group: pid.to_string(),
    }
}

fn leader() -> ProcessIdentity {
    id(P, &START.to_string())
}

fn run(sim: &Sim, ids: &[ProcessIdentity], budget: Duration) -> Vec<Fate> {
    let r = kill_recorded(sim, ids, ME, sim.at(budget));
    sim.assert_no_call_at_or_after(budget);
    r.fates.into_iter().map(|(_, f)| f).collect()
}

/// A live leader that ignores HUP and TERM: HUP, TERM, KILL to that pid only, each right
/// after a read of it showed the recorded start, at the engine's times; then `Gone`.
#[test]
fn live_leader_gets_hup_term_kill() {
    let sim = Sim::new();
    sim.add(P, &[], R::Live(START), Some(Sig::Kill));
    assert_eq!(run(&sim, &[leader()], Duration::from_secs(5)), [Fate::Gone]);
    assert_eq!(sim.kills(), [(P, Sig::Hup), (P, Sig::Term), (P, Sig::Kill)]);
    let calls = sim.calls.borrow().clone();
    for (i, (at, c)) in calls.iter().enumerate() {
        if let C::Kill(_, sig) = c {
            assert_eq!(calls[i - 1].1, C::Info(P), "the read right before {c:?}");
            match sig {
                Sig::Hup => assert_eq!(*at, Duration::ZERO),
                Sig::Term => assert_eq!(*at, Duration::from_millis(500)),
                Sig::Kill => assert_eq!(*at, Duration::from_secs(2)),
            }
        }
    }
}

/// The pid holds another start: `Replaced`, never signalled.
#[test]
fn another_start_is_replaced() {
    let sim = Sim::new();
    sim.add(P, &[], R::Live(START + 1), None);
    assert_eq!(
        run(&sim, &[leader()], Duration::from_secs(5)),
        [Fate::Replaced]
    );
    assert!(sim.kills().is_empty());
}

/// Every read refused: `Unreadable`, never signalled.
#[test]
fn always_refused_is_unreadable() {
    let sim = Sim::new();
    sim.add(P, &[], R::Refused, None);
    assert_eq!(
        run(&sim, &[leader()], Duration::from_secs(5)),
        [Fate::Unreadable]
    );
    assert!(sim.kills().is_empty());
}

/// The last read alone classifies: live then refused → `Unreadable`; refused then live →
/// `Alive`.
#[test]
fn last_read_classifies() {
    let sim = Sim::new();
    sim.add(P, &[R::Live(START)], R::Refused, None);
    assert_eq!(
        run(&sim, &[leader()], Duration::from_secs(5)),
        [Fate::Unreadable]
    );
    let sim = Sim::new();
    sim.add(P, &[R::Refused], R::Live(START), None);
    assert_eq!(
        run(&sim, &[leader()], Duration::from_secs(5)),
        [Fate::Alive]
    );
    assert!(sim.kills().iter().all(|(p, _)| *p == P));
}

/// pid 0, 1, the daemon's own, `i32::MAX + 1`, an empty or unparsable start: `Invalid`, no
/// call at all.
#[test]
fn invalid_identities_are_never_read() {
    let sim = Sim::new();
    let big = u32::try_from(i32::MAX).unwrap() + 1;
    let ids = [
        id(0, "1"),
        id(1, "1"),
        id(ME, "1"),
        id(big, "1"),
        id(P, ""),
        id(P + 1, "x1"),
    ];
    assert_eq!(run(&sim, &ids, Duration::from_secs(5)), [Fate::Invalid; 6]);
    assert!(sim.calls().is_empty(), "{:?}", sim.calls());
}

/// A deadline already passed: `NotRead`, no call.
#[test]
fn passed_deadline_reads_nothing() {
    let sim = Sim::new();
    sim.add(P, &[], R::Live(START), None);
    sim.now.set(Duration::from_secs(1));
    let r = kill_recorded(&sim, &[leader()], ME, sim.at(Duration::from_secs(1)));
    assert_eq!(r.fates[0].1, Fate::NotRead);
    assert!(sim.calls().is_empty());
}

/// The deadline passing between `bsd_info` and `short_zombie`: no `short_zombie` call, and
/// the cut read counts as refused.
#[test]
fn deadline_between_the_two_calls_of_a_read() {
    let sim = Sim::new();
    sim.add(P, &[], R::Refused, None);
    sim.info_cost.set(Duration::from_millis(60));
    assert_eq!(
        run(&sim, &[leader()], Duration::from_millis(50)),
        [Fate::Unreadable]
    );
    assert_eq!(sim.calls(), [C::Info(P)]);
}

/// The deadline passing during a read that showed the recorded start: no signal follows it.
#[test]
fn deadline_after_a_matching_read() {
    let sim = Sim::new();
    sim.add(P, &[], R::Live(START), None);
    sim.info_cost.set(Duration::from_millis(60));
    assert_eq!(
        run(&sim, &[leader()], Duration::from_millis(50)),
        [Fate::Alive]
    );
    assert_eq!(sim.calls(), [C::Info(P)]);
}

/// Every sleep is clipped to the time left: none ends after the deadline, and the last
/// ends exactly at it.
#[test]
fn sleeps_are_clipped() {
    let sim = Sim::new();
    sim.add(P, &[], R::Live(START), None);
    let budget = Duration::from_millis(2_013);
    assert_eq!(run(&sim, &[leader()], budget), [Fate::Alive]);
    let calls = sim.calls.borrow().clone();
    let mut last_end = Duration::ZERO;
    for (at, c) in calls {
        if let C::Sleep(d) = c {
            assert!(d <= Duration::from_millis(25), "{d:?}");
            assert!(at + d <= budget, "a sleep from {at:?} for {d:?}");
            last_end = at + d;
        }
    }
    assert_eq!(last_end, budget);
}

/// A budget of 100 ms or less: only the KILL round.
#[test]
fn short_budget_is_kill_only() {
    let sim = Sim::new();
    sim.add(P, &[], R::Live(START), None);
    assert_eq!(
        run(&sim, &[leader()], Duration::from_millis(100)),
        [Fate::Alive]
    );
    assert_eq!(sim.kills(), [(P, Sig::Kill)]);
}

/// A zombie is `Gone`, never signalled.
#[test]
fn zombie_is_gone() {
    let sim = Sim::new();
    sim.add(P, &[], R::Zombie, None);
    assert_eq!(run(&sim, &[leader()], Duration::from_secs(5)), [Fate::Gone]);
    assert!(sim.kills().is_empty());
}

/// The same identity twice: one entry, one read per round.
#[test]
fn duplicates_collapse() {
    let sim = Sim::new();
    sim.add(P, &[], R::Gone, None);
    let r = kill_recorded(
        &sim,
        &[leader(), leader()],
        ME,
        sim.at(Duration::from_secs(5)),
    );
    assert_eq!(r.fates, [(leader(), Fate::Gone)]);
    assert_eq!(sim.calls(), [C::Info(P)]);
    // A live one: each round reads it once.
    let sim = Sim::new();
    sim.add(P, &[], R::Live(START), Some(Sig::Hup));
    let r = kill_recorded(
        &sim,
        &[leader(), leader()],
        ME,
        sim.at(Duration::from_secs(5)),
    );
    assert_eq!(r.fates, [(leader(), Fate::Gone)]);
    assert_eq!(
        sim.calls(),
        [
            C::Info(P),
            C::Kill(P, Sig::Hup),
            C::Sleep(Duration::from_millis(25)),
            C::Info(P)
        ]
    );
}
