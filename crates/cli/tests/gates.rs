//! The test gate's own guarantee, G1–G3 (tests/support/faults.rs): each case runs this binary as
//! a bare waiter at a gate and observes how it ends.

mod support;

use std::ffi::OsStr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::{Command, Stdio};

use support::faults::{GATE_BOUND_MS, GATE_FAILED, GATE_MAX, GATE_WAITER, Gate};
use support::runner::{Case, main as run_cases};
use support::{Proc, WAIT, spend};

fn main() {
    run_cases(&[
        Case {
            name: "released_waiter_goes_on",
            run: released_waiter_goes_on,
        },
        Case {
            name: "expired_waiter_fails",
            run: expired_waiter_fails,
        },
        Case {
            name: "removed_gate_fails_the_waiter",
            run: removed_gate_fails_the_waiter,
        },
        Case {
            name: "unreadable_decision_fails_the_waiter",
            run: unreadable_decision_fails_the_waiter,
        },
        #[cfg(unix)]
        Case {
            name: "colliding_unreadable_decision_fails_the_waiter",
            run: colliding_unreadable_decision_fails_the_waiter,
        },
        Case {
            name: "dropped_gate_ends_its_waiter",
            run: dropped_gate_ends_its_waiter,
        },
        Case {
            name: "release_past_the_bound_fails",
            run: release_past_the_bound_fails,
        },
    ]);
}

/// This binary waiting at `gate`, with `env` set.
fn waiter(gate: &Gate, env: &[(&str, &OsStr)]) -> Proc {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.env(GATE_WAITER, gate.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    Proc::spawn(&mut c)
}

/// The panic message of `f`, which must panic.
fn panic_of(f: impl FnOnce()) -> String {
    let e = catch_unwind(AssertUnwindSafe(f)).expect_err("did not panic");
    e.downcast_ref::<String>()
        .cloned()
        .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default()
}

/// G2: the test's release is the waiter's one way on.
fn released_waiter_goes_on() {
    let gate = Gate::new();
    let mut w = waiter(&gate, &[]);
    assert!(gate.reached(WAIT), "{}", w.stderr());
    gate.release();
    assert_eq!(w.code(), 0, "{}", w.stderr());
}

/// G1, G2: a waiter whose bound passes before the release exits with the fixture's code, and the
/// test's release then fails.
fn expired_waiter_fails() {
    let gate = Gate::new();
    let mut w = waiter(&gate, &[(GATE_BOUND_MS, OsStr::new("0"))]);
    assert_eq!(w.code(), GATE_FAILED, "{}", w.stderr());
    assert!(w.stderr().contains("expired"), "{}", w.stderr());
    let why = panic_of(|| gate.release());
    assert!(why.contains("decided expired"), "{why}");
}

/// G2: a waiter whose gate directory is removed exits with the fixture's code at once, long
/// before its bound, and the test's release then fails.
fn removed_gate_fails_the_waiter() {
    let gate = Gate::new();
    let mut w = waiter(&gate, &[]);
    assert!(gate.reached(WAIT), "{}", w.stderr());
    std::fs::remove_dir_all(gate.path()).unwrap();
    assert_eq!(w.code(), GATE_FAILED, "{}", w.stderr());
    assert!(w.stderr().contains("is gone"), "{}", w.stderr());
    let why = panic_of(|| gate.release());
    assert!(why.contains("could not be released"), "{why}");
}

/// G2: a decision the waiter cannot read ends it with the fixture's code.
fn unreadable_decision_fails_the_waiter() {
    let gate = Gate::new();
    let mut w = waiter(&gate, &[]);
    assert!(gate.reached(WAIT), "{}", w.stderr());
    std::fs::create_dir(gate.path().join("decided")).unwrap();
    assert_eq!(w.code(), GATE_FAILED, "{}", w.stderr());
    assert!(w.stderr().contains("cannot be read"), "{}", w.stderr());
}

/// G3: a test that panics while its waiter waits drops the gate, and the waiter ends at once on
/// the guard's decision (exit with the fixture's code), not at its bound or on the removal of
/// the directory.
fn dropped_gate_ends_its_waiter() {
    let mut w = None;
    let _ = panic_of(|| {
        let gate = Gate::new();
        let p = w.insert(waiter(&gate, &[]));
        assert!(gate.reached(WAIT), "{}", p.stderr());
        panic!("the test fails while its waiter waits");
    });
    let w = w.as_mut().unwrap();
    assert_eq!(w.code(), GATE_FAILED, "{}", w.stderr());
    assert!(w.stderr().contains("decided dropped"), "{}", w.stderr());
}

/// G1: a release after more watchdogs than the waiter's bound leaves room for fails, and its
/// gate is then dropped, so the waiter ends.
fn release_past_the_bound_fails() {
    let mut w = None;
    let why = panic_of(|| {
        let gate = Gate::new();
        let p = w.insert(waiter(&gate, &[]));
        assert!(gate.reached(WAIT), "{}", p.stderr());
        spend(GATE_MAX);
        gate.release();
    });
    assert!(why.contains("exceed the waiter's bound"), "{why}");
    let w = w.as_mut().unwrap();
    assert_eq!(w.code(), GATE_FAILED, "{}", w.stderr());
}

/// G1, G2: a `decided` entry that exists but reads as missing (a dangling link): once the bound
/// passes, the waiter's own decision collides with it, and the one read after the collision
/// fails, so the waiter exits with the fixture's code instead of polling on.
#[cfg(unix)]
fn colliding_unreadable_decision_fails_the_waiter() {
    let gate = Gate::new();
    std::os::unix::fs::symlink(gate.path().join("absent"), gate.path().join("decided")).unwrap();
    let mut w = waiter(&gate, &[(GATE_BOUND_MS, OsStr::new("0"))]);
    assert_eq!(w.code(), GATE_FAILED, "{}", w.stderr());
    assert!(w.stderr().contains("cannot be read"), "{}", w.stderr());
}
