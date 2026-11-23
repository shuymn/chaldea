//! Test-only fault points of the lifecycle (design §7): parks before and inside steps,
//! injected file and bind failures, the store's own faults, a withheld log acknowledgement,
//! and what a run publishes for its test (the gate, the store, the runtime, the port, and the
//! coordinator's marks).

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use super::files::ReplaceStage;
use super::gate::ShutdownGate;
use crate::store::Store;
use crate::store::test_faults;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A gate a step waits at until the test releases it.
#[derive(Default)]
pub(crate) struct Park {
    /// (reached, released)
    state: Mutex<(bool, bool)>,
    cv: Condvar,
    notify: tokio::sync::Notify,
}

impl Park {
    /// Waits (bounded) until a step reached the park.
    pub(crate) fn wait_reached(&self, within: Duration) -> bool {
        let end = Instant::now() + within;
        let mut s = lock(&self.state);
        while !s.0 {
            let now = Instant::now();
            if now >= end {
                return false;
            }
            s = self
                .cv
                .wait_timeout(s, end - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    pub(crate) fn release(&self) {
        lock(&self.state).1 = true;
        self.cv.notify_all();
        self.notify.notify_waiters();
    }

    fn reach(&self) {
        lock(&self.state).0 = true;
        self.cv.notify_all();
    }

    fn released(&self) -> bool {
        lock(&self.state).1
    }

    /// Parks a blocking step (inside `spawn_blocking`) until released.
    pub(crate) fn hold_blocking(&self) {
        self.reach();
        let mut s = lock(&self.state);
        while !s.1 {
            s = self.cv.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Parks an async step until released.
    pub(crate) async fn hold(&self) {
        self.reach();
        loop {
            let notified = self.notify.notified();
            if self.released() {
                return;
            }
            notified.await;
        }
    }
}

/// Where a park sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Point {
    /// At the boundary before step `n`, before its gate check.
    Before(u8),
    /// Inside step `n` (8: inside `Store::open`'s blocking call; 13: after `serve`; 14: before
    /// the readiness line's gate check).
    Inside(u8),
    /// After the `port` rename of step 13.
    AfterPortRename,
}

/// What a test injects into one run and what the run publishes back.
#[derive(Default)]
pub(crate) struct Faults {
    parks: Mutex<HashMap<Point, Arc<Park>>>,
    /// Every bind of step 10 fails.
    pub(crate) bind_fails: AtomicBool,
    /// `write_replace` failures, each used once: (file name, stage).
    pub(crate) replace: Mutex<Vec<(String, ReplaceStage)>>,
    /// The owner token's verifying read fails.
    pub(crate) token_read_fails: AtomicBool,
    /// The log thread never acknowledges a target switch.
    pub(crate) withhold_log_ack: AtomicBool,
    /// The rotation cap of `daemon.log` instead of 8 MiB.
    pub(crate) log_cap: Mutex<Option<u64>>,
    /// The store is opened with these fault points.
    pub(crate) store_faults: Mutex<Option<Arc<test_faults::Faults>>>,
    // Published by the run.
    pub(crate) gate: OnceLock<Arc<ShutdownGate>>,
    pub(crate) store: OnceLock<Store>,
    pub(crate) runtime: OnceLock<(tokio::runtime::Handle, usize)>,
    pub(crate) port: OnceLock<u16>,
    pub(crate) dir: OnceLock<PathBuf>,
    /// The coordinator's steps with the end it passed, in order.
    pub(crate) marks: Mutex<Vec<(String, Option<Instant>)>>,
    /// The thread that ran `Store::shutdown`.
    pub(crate) store_shutdown_thread: OnceLock<std::thread::ThreadId>,
    /// Requests only the coordinator's select loop answers.
    pub(crate) probe:
        Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<std::sync::mpsc::Sender<()>>>>,
}

impl Faults {
    /// Installs a park at `p` and returns it.
    pub(crate) fn park(&self, p: Point) -> Arc<Park> {
        let park = Arc::new(Park::default());
        lock(&self.parks).insert(p, park.clone());
        park
    }

    pub(crate) fn parked(&self, p: Point) -> Option<Arc<Park>> {
        lock(&self.parks).get(&p).cloned()
    }

    /// Fails the next `write_replace` of `name` at `stage`.
    pub(crate) fn fail_replace(&self, name: &str, stage: ReplaceStage) {
        lock(&self.replace).push((name.to_owned(), stage));
    }

    pub(crate) fn replace_hook(&self, name: &str, stage: ReplaceStage) -> io::Result<()> {
        let mut v = lock(&self.replace);
        if let Some(i) = v.iter().position(|(n, s)| n == name && *s == stage) {
            v.remove(i);
            return Err(io::Error::other(format!("injected {stage:?} failure")));
        }
        Ok(())
    }

    pub(crate) fn mark(&self, what: &str, end: Option<Instant>) {
        lock(&self.marks).push((what.to_owned(), end));
    }

    pub(crate) fn marks(&self) -> Vec<(String, Option<Instant>)> {
        lock(&self.marks).clone()
    }

    pub(crate) fn bind_fails(&self) -> bool {
        self.bind_fails.load(Ordering::SeqCst)
    }
}
