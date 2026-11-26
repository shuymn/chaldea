//! PR10a2-CL-2: the Windows loop over a scripted `ProcessOs` and a scripted clock (every
//! OS); the real processes are `tests/process_native.rs`'s `orphan_native`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use super::Fate;
use super::windows::kill_recorded;
use crate::backend::ProcessIdentity;
use crate::windows::kill::{Access, ProcessOs};

const ME: u32 = 50;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_INVALID_PARAMETER: i32 = 87;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum C {
    Open(u32),
    Ctime(u32),
    Terminate(u32),
    Signalled(u32, Duration),
    Sleep,
}

#[derive(Debug, Clone, Copy, Default)]
struct Proc {
    start: u64,
    alive: bool,
    /// `open_process` fails with this code.
    open_error: Option<i32>,
    ctime_error: bool,
    wait_error: bool,
    /// `terminate` does not end it.
    immortal: bool,
}

#[derive(Debug)]
struct H(u32);

struct Os {
    base: Instant,
    now: Cell<Duration>,
    /// Clock advance of each open and terminate call.
    cost: Cell<Duration>,
    procs: RefCell<HashMap<u32, Proc>>,
    calls: RefCell<Vec<(Duration, C)>>,
}

impl Os {
    fn new() -> Os {
        Os {
            base: Instant::now(),
            now: Cell::new(Duration::ZERO),
            cost: Cell::new(Duration::ZERO),
            procs: RefCell::new(HashMap::new()),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn add(&self, pid: u32, p: Proc) {
        self.procs.borrow_mut().insert(pid, p);
    }

    fn call(&self, c: C) {
        self.calls.borrow_mut().push((self.now.get(), c));
    }

    fn calls(&self) -> Vec<C> {
        self.calls.borrow().iter().map(|(_, c)| *c).collect()
    }

    fn proc(&self, pid: u32) -> Proc {
        self.procs.borrow().get(&pid).copied().unwrap_or_default()
    }

    fn run(&self, ids: &[ProcessIdentity], budget: Duration) -> Vec<Fate> {
        let r = kill_recorded(self, ids, ME, self.base + budget);
        for (at, c) in self.calls.borrow().iter() {
            if !matches!(c, C::Sleep) {
                assert!(*at < budget, "{c:?} at {at:?}");
            }
        }
        r.fates.into_iter().map(|(_, f)| f).collect()
    }
}

impl ProcessOs for Os {
    type Held = H;

    fn now(&self) -> Instant {
        self.base + self.now.get()
    }

    fn sleep(&self, d: Duration) {
        self.call(C::Sleep);
        self.now.set(self.now.get() + d);
    }

    fn open_process(&self, pid: u32, _access: Access) -> io::Result<H> {
        self.call(C::Open(pid));
        self.now.set(self.now.get() + self.cost.get());
        let p = self.proc(pid);
        if !p.alive {
            return Err(io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER));
        }
        if let Some(c) = p.open_error {
            return Err(io::Error::from_raw_os_error(c));
        }
        Ok(H(pid))
    }

    fn creation_time(&self, h: &H) -> io::Result<u64> {
        self.call(C::Ctime(h.0));
        let p = self.proc(h.0);
        if p.ctime_error {
            return Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED));
        }
        Ok(p.start)
    }

    fn signalled(&self, h: &H, wait: Duration) -> io::Result<bool> {
        self.call(C::Signalled(h.0, wait));
        let p = self.proc(h.0);
        if p.wait_error {
            return Err(io::Error::from_raw_os_error(6));
        }
        if p.alive {
            self.now.set(self.now.get() + wait);
        }
        Ok(!p.alive)
    }

    fn terminate(&self, h: &H) -> io::Result<()> {
        self.call(C::Terminate(h.0));
        self.now.set(self.now.get() + self.cost.get());
        if let Some(p) = self.procs.borrow_mut().get_mut(&h.0)
            && !p.immortal
        {
            p.alive = false;
        }
        Ok(())
    }
}

fn id(pid: u32, start: &str) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start_time: start.to_owned(),
        group: String::new(),
    }
}

fn live(start: u64) -> Proc {
    Proc {
        start,
        alive: true,
        ..Proc::default()
    }
}

const BUDGET: Duration = Duration::from_secs(5);

/// Every held handle is terminated before the first wait; two live → both `Gone`.
#[test]
fn terminate_all_before_any_wait() {
    let os = Os::new();
    os.add(100, live(1));
    os.add(101, live(2));
    assert_eq!(
        os.run(&[id(100, "1"), id(101, "2")], BUDGET),
        [Fate::Gone, Fate::Gone]
    );
    let calls = os.calls();
    let first_wait = calls
        .iter()
        .position(|c| matches!(c, C::Signalled(..)))
        .unwrap();
    let last_terminate = calls
        .iter()
        .rposition(|c| matches!(c, C::Terminate(_)))
        .unwrap();
    assert!(last_terminate < first_wait, "{calls:?}");
    assert_eq!(
        calls
            .iter()
            .filter(|c| matches!(c, C::Terminate(_)))
            .count(),
        2
    );
}

/// `Stale` → `Replaced`, never terminated; `Unverifiable` → `Unreadable`; a gone pid →
/// `Gone`.
#[test]
fn open_outcomes() {
    let os = Os::new();
    os.add(100, live(9));
    os.add(
        101,
        Proc {
            ctime_error: true,
            ..live(2)
        },
    );
    os.add(
        102,
        Proc {
            open_error: Some(31),
            ..live(3)
        },
    );
    assert_eq!(
        os.run(
            &[id(100, "1"), id(101, "2"), id(102, "3"), id(103, "4")],
            BUDGET
        ),
        [
            Fate::Replaced,
            Fate::Unreadable,
            Fate::Unreadable,
            Fate::Gone
        ]
    );
    assert!(!os.calls().iter().any(|c| matches!(c, C::Terminate(_))));
}

/// The deadline during the first open (`Cut`): `NotRead`, and no later open.
#[test]
fn cut_stops_opening() {
    let os = Os::new();
    os.add(100, live(1));
    os.add(101, live(2));
    os.cost.set(Duration::from_millis(60));
    assert_eq!(
        os.run(&[id(100, "1"), id(101, "2")], Duration::from_millis(50)),
        [Fate::NotRead, Fate::NotRead]
    );
    assert_eq!(os.calls(), [C::Open(100)]);
}

/// A wait error is `WaitFailed`, with no further wait on that handle, while another handle's
/// waits go on until the deadline (`Alive`).
#[test]
fn wait_error_ends_that_handle() {
    let os = Os::new();
    os.add(
        100,
        Proc {
            wait_error: true,
            immortal: true,
            ..live(1)
        },
    );
    os.add(
        101,
        Proc {
            immortal: true,
            ..live(2)
        },
    );
    assert_eq!(
        os.run(&[id(100, "1"), id(101, "2")], Duration::from_secs(1)),
        [Fate::WaitFailed, Fate::Alive]
    );
    let calls = os.calls();
    let waits = |pid| {
        calls
            .iter()
            .filter(|c| matches!(c, C::Signalled(p, _) if *p == pid))
            .count()
    };
    assert_eq!(waits(100), 1);
    assert!(waits(101) > 1);
    // Every wait was clipped to the time left.
    let mut t = Duration::ZERO;
    for (at, c) in os.calls.borrow().iter() {
        if let C::Signalled(_, w) = c {
            assert!(*at + *w <= Duration::from_secs(1));
            t = *at + *w;
        }
    }
    assert_eq!(t, Duration::from_secs(1));
}

/// The deadline before any wait: a held handle is `NotRead`.
#[test]
fn deadline_before_any_wait() {
    let os = Os::new();
    os.add(
        100,
        Proc {
            immortal: true,
            ..live(1)
        },
    );
    os.cost.set(Duration::from_millis(30));
    assert_eq!(
        os.run(&[id(100, "1")], Duration::from_millis(50)),
        [Fate::NotRead]
    );
    assert!(!os.calls().iter().any(|c| matches!(c, C::Signalled(..))));
}

/// The deadline passing between two terminations: the second handle is not terminated, and
/// neither is waited on.
#[test]
fn deadline_before_terminate() {
    let os = Os::new();
    os.add(100, live(1));
    os.add(101, live(2));
    os.cost.set(Duration::from_millis(30));
    assert_eq!(
        os.run(&[id(100, "1"), id(101, "2")], Duration::from_millis(75)),
        [Fate::NotRead, Fate::NotRead]
    );
    assert_eq!(
        os.calls(),
        [
            C::Open(100),
            C::Ctime(100),
            C::Open(101),
            C::Ctime(101),
            C::Terminate(100)
        ]
    );
}

/// pid 0, 4, the daemon's own, an empty or unparsable start: `Invalid`, no call; a duplicate
/// is one entry.
#[test]
fn invalid_and_duplicates() {
    let os = Os::new();
    let ids = [
        id(0, "1"),
        id(4, "1"),
        id(ME, "1"),
        id(100, ""),
        id(101, "x"),
    ];
    assert_eq!(os.run(&ids, BUDGET), [Fate::Invalid; 5]);
    assert!(os.calls().is_empty());
    let os = Os::new();
    os.add(100, live(1));
    let r = kill_recorded(&os, &[id(100, "1"), id(100, "1")], ME, os.base + BUDGET);
    assert_eq!(r.fates, [(id(100, "1"), Fate::Gone)]);
    assert_eq!(
        os.calls()
            .iter()
            .filter(|c| matches!(c, C::Open(_)))
            .count(),
        1
    );
}
