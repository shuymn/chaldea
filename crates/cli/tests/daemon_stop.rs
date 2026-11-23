//! `chaldea daemon stop` (design §2.10): the binary cases of PR8-CL-15. The daemons here are
//! foreground `daemon run`s this test starts (this binary as `chaldea` with test seams), so
//! the test can observe their exit.
//!
//! Literals are the design's: the 15 s stop deadline.

mod support;

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chaldea_daemon::lifecycle::{DiscoveryRecord, Evidence, RecordState, names, spawn_locked};
use chaldea_pty_runtime::host;
use support::faults::{self, Gate, Labels, SLOW_SHUTDOWN_MS, STOP_PAUSE};
use support::runner::{Case, main as run_cases};
use support::{Proc, WAIT, marked_dir, state};

fn main() {
    run_cases(&[
        Case {
            name: "stops_and_waits_for_the_target",
            run: stops_and_waits_for_the_target,
        },
        Case {
            name: "not_running_is_success",
            run: not_running_is_success,
        },
        Case {
            name: "replacement_does_not_confuse_completion",
            run: replacement_does_not_confuse_completion,
        },
        Case {
            name: "identity_mismatch_signals_nothing",
            run: identity_mismatch_signals_nothing,
        },
    ]);
}

/// The design's stop deadline, restated.
const STOP_WAIT: Duration = Duration::from_secs(15);

fn cmd(labels: &Labels, dir: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> Command {
    let mut c = faults::chaldea(labels, &[OsStr::new("--state-dir"), dir.as_os_str()]);
    c.args(args);
    for (k, v) in env {
        c.env(k, v);
    }
    c
}

/// A foreground daemon on `dir`, serving.
fn foreground(labels: &Labels, dir: &Path, env: &[(&str, &OsStr)]) -> Proc {
    let p = Proc::spawn(&mut cmd(labels, dir, &["daemon", "run"], env));
    p.serving();
    p
}

/// PR8-CL-15: `daemon stop` returns only once the target's identity is gone: its shutdown
/// takes a second (a slow participant), and the daemon has exited when stop returns.
fn stops_and_waits_for_the_target() {
    let labels = Labels::new();
    let d = state();
    let mut daemon = foreground(&labels, &d.path, &[(SLOW_SHUTDOWN_MS, OsStr::new("1000"))]);
    let t = Instant::now();
    let mut s = Proc::spawn(&mut cmd(&labels, &d.path, &["daemon", "stop"], &[]));
    assert_eq!(s.code(), 0, "{}{}", s.stdout(), s.stderr());
    let gone = daemon.child.try_wait().unwrap();
    assert!(gone.is_some(), "stop returned while the daemon ran");
    assert!(t.elapsed() >= Duration::from_secs(1), "{:?}", t.elapsed());
    assert_eq!(s.stdout().trim(), "stopped");
    assert_eq!(daemon.code(), 0, "{}", daemon.stderr());
}

/// PR8-CL-15: no daemon (no directory; a stale record with a free lock): "not running", 0.
fn not_running_is_success() {
    let labels = Labels::new();
    let d = state();
    let (code, out, _) = support::run_to_exit(&mut cmd(&labels, &d.path, &["daemon", "stop"], &[]));
    assert_eq!((code, out.trim()), (0, "not running"));
    marked_dir(&d.path);
    let stale = DiscoveryRecord {
        version: 1,
        instance: uuid::Uuid::new_v4().to_string(),
        pid: std::process::id(),
        start_time: host::current_identity().unwrap().start_time,
        state: RecordState::Serving,
        port: Some(1),
        url: None,
        started_at: String::new(),
        detached: None,
        evidence: Evidence::default(),
    };
    support::write_private(
        &d.path.join(names::RECORD),
        &serde_json::to_vec(&stale).unwrap(),
    );
    let (code, out, _) = support::run_to_exit(&mut cmd(&labels, &d.path, &["daemon", "stop"], &[]));
    assert_eq!((code, out.trim()), (0, "not running"));
    assert!(
        d.path.join(names::RECORD).exists(),
        "a client removed the record"
    );
}

/// PR8-CL-15: a new daemon takes the lock right after the target exits, before `stop` checks
/// completion: `stop` still completes on the target's identity, says so, and names the new
/// daemon.
fn replacement_does_not_confuse_completion() {
    let labels = Labels::new();
    let d = state();
    let mut first = foreground(&labels, &d.path, &[]);
    let gate = Gate::new();
    let mut s = Proc::spawn(&mut cmd(
        &labels,
        &d.path,
        &["daemon", "stop"],
        &[(STOP_PAUSE, gate.path().as_os_str())],
    ));
    assert!(gate.reached(WAIT), "{}", s.stderr());
    assert_eq!(first.code(), 0, "{}", first.stderr());
    let mut second = foreground(&labels, &d.path, &[]);
    gate.release();
    assert_eq!(s.code(), 0, "{}{}", s.stdout(), s.stderr());
    let out = s.stdout();
    assert!(out.starts_with("stopped\n"), "{out}");
    assert!(
        out.contains(&format!("a new daemon (pid {}) now holds", second.pid())),
        "{out}"
    );
    assert_eq!(second.stop_and_wait(&d.path), 0);
}

/// PR8-CL-15: the lock held and a valid record naming a live unrelated process with another
/// start time: discovery is `Unknown`, so `stop` waits for a record and exits 4 at its
/// deadline; the process was never signalled.
fn identity_mismatch_signals_nothing() {
    let labels = Labels::new();
    let d = state();
    marked_dir(&d.path);
    let lock = std::fs::File::create(d.path.join(names::LOCK)).unwrap();
    lock.try_lock().unwrap();
    let mut sleeper = Sleeper::start();
    let record = DiscoveryRecord {
        version: 1,
        instance: uuid::Uuid::new_v4().to_string(),
        pid: sleeper.pid(),
        start_time: "1".to_owned(),
        state: RecordState::Serving,
        port: Some(1),
        url: None,
        started_at: String::new(),
        detached: None,
        evidence: Evidence::default(),
    };
    support::write_private(
        &d.path.join(names::RECORD),
        &serde_json::to_vec(&record).unwrap(),
    );
    let t = Instant::now();
    let mut s = Proc::spawn(&mut cmd(&labels, &d.path, &["daemon", "stop"], &[]));
    let code = s.wait_within(STOP_WAIT + WAIT).code();
    assert_eq!(code, Some(4), "{}{}", s.stdout(), s.stderr());
    assert!(t.elapsed() >= STOP_WAIT, "{:?}", t.elapsed());
    assert!(
        s.stderr().contains("has not published its identity"),
        "{}",
        s.stderr()
    );
    assert!(sleeper.alive(), "the unrelated process was signalled");
    drop(lock);
}

/// A live process the test owns: a child of this binary blocked on its stdin.
struct Sleeper(std::process::Child);

impl Sleeper {
    fn start() -> Sleeper {
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.env(support::runner::SLEEP_ON_STDIN, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Sleeper(spawn_locked(&mut c).unwrap())
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn alive(&mut self) -> bool {
        self.0.try_wait().unwrap().is_none()
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        if let Some(mut i) = self.0.stdin.take() {
            let _ = i.write_all(b"\n");
        }
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end && self.alive() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
