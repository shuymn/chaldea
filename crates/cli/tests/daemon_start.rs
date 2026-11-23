//! `chaldea daemon start` (design §2.10): PR8-CL-14, the `daemon_start` cases of PR8-CL-20 and
//! PR8-CL-23. Every start runs this binary as `chaldea` with test seams (support/faults.rs), so
//! its detached child is this binary too, and on a macOS GUI host every launchd job carries
//! the test's own label prefix and is booted out when the test ends.
//!
//! Literals are the design's: the 15 s readiness deadline, the 2 s EOF follow-up.

mod support;

use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, Instant};

use chaldea_daemon::lifecycle::{Discovery, LockProbe, discover, names};
use support::faults::{
    self, EOF_CHILD_MS, EXIT_RECORD, Gate, HOLD_READER_MS, LOG_MARKERS, Labels, MARKER_PAUSE,
    PARK_RECONCILER, READY_PAUSE, SERVE_PAUSE, SLOW_SHUTDOWN_MS, TOKEN_FIELD, TOKEN_MSG,
    TOKEN_SPAN, VERDICT_PAUSE,
};
use support::runner::{Case, main as run_cases};
use support::{Dir, Proc, WAIT, get_status, lock_state, state, token_of, wait_for};

fn main() {
    run_cases(&[
        Case {
            name: "reports_ready_url",
            run: reports_ready_url,
        },
        Case {
            name: "fail_arrives_after_the_lock_decision",
            run: fail_arrives_after_the_lock_decision,
        },
        Case {
            name: "stopped_before_ready",
            run: stopped_before_ready,
        },
        Case {
            name: "concurrent_starts",
            run: concurrent_starts,
        },
        Case {
            name: "concurrent_first_start_at_marker",
            run: concurrent_first_start_at_marker,
        },
        Case {
            name: "loser_sees_winner_exit",
            run: loser_sees_winner_exit,
        },
        Case {
            name: "explicit_port_conflict",
            run: explicit_port_conflict,
        },
        Case {
            name: "eof_follow_up_within_deadline",
            run: eof_follow_up_within_deadline,
        },
        Case {
            name: "timeout_after_15_s_leaves_child",
            run: timeout_after_15_s_leaves_child,
        },
        Case {
            name: "late_child_serves",
            run: late_child_serves,
        },
        Case {
            name: "stopped_after_ready_is_not_reported_started",
            run: stopped_after_ready_is_not_reported_started,
        },
        Case {
            name: "stopping_winner_is_not_reported_running",
            run: stopping_winner_is_not_reported_running,
        },
        Case {
            name: "stopping_after_the_answer_is_not_reported_started",
            run: stopping_after_the_answer_is_not_reported_started,
        },
        Case {
            name: "detached_log_is_redacted",
            run: detached_log_is_redacted,
        },
        Case {
            name: "startup_failure_output_is_redacted",
            run: startup_failure_output_is_redacted,
        },
        Case {
            name: "child_receives_the_resolved_dir",
            run: child_receives_the_resolved_dir,
        },
    ]);
}

/// The design's readiness deadline, restated.
const READY_TIMEOUT: Duration = Duration::from_secs(15);

/// `chaldea --state-dir <dir> daemon <args…>` with seams, `env` set.
fn cmd(
    labels: &Labels,
    dir: &Path,
    args: &[&str],
    env: &[(&str, &OsStr)],
) -> std::process::Command {
    let mut c = faults::chaldea(labels, &[OsStr::new("--state-dir"), dir.as_os_str()]);
    c.args(args);
    for (k, v) in env {
        c.env(k, v);
    }
    c
}

/// Runs `daemon start [extra…]` to its exit (within 30 s): code, stdout, stderr.
fn start(
    labels: &Labels,
    dir: &Path,
    extra: &[&str],
    env: &[(&str, &OsStr)],
) -> (i32, String, String) {
    let mut args = vec!["daemon", "start"];
    args.extend_from_slice(extra);
    let mut p = Proc::spawn(&mut cmd(labels, dir, &args, env));
    let code = p.code();
    (code, p.stdout(), p.stderr())
}

/// The URL of `daemon started: <url> (pid <pid>)` or `daemon already running: …`.
fn url_of(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|l| {
            l.strip_prefix("daemon started: ")
                .or_else(|| l.strip_prefix("daemon already running: "))
        })
        .and_then(|r| r.split_once(' '))
        .map(|(u, _)| u.to_owned())
        .unwrap_or_else(|| panic!("no URL in {stdout:?}"))
}

/// Stops whatever serves `dir` when the test ends (through `daemon stop`, bounded).
struct StopOnDrop<'a>(&'a Labels, &'a Path);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        if matches!(
            discover(self.1),
            Discovery::Serving(_) | Discovery::Starting(_) | Discovery::Unknown
        ) {
            let mut p = Proc::spawn(&mut cmd(self.0, self.1, &["daemon", "stop"], &[]));
            let _ = p.wait_within(Duration::from_secs(30));
        }
    }
}

/// A starter waiting on `gate` (paused there, or waiting for a child paused there): on every
/// ending the gate is released and the starter given up to 30 s to exit by itself, so it
/// removes its readiness channel, before `Proc`'s drop kills it.
struct Gated<'a> {
    gate: &'a Gate,
    p: Proc,
}

impl Drop for Gated<'_> {
    fn drop(&mut self) {
        self.gate.release();
        let end = Instant::now() + WAIT;
        while Instant::now() < end && matches!(self.p.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn stop(labels: &Labels, dir: &Path) {
    let mut p = Proc::spawn(&mut cmd(labels, dir, &["daemon", "stop"], &[]));
    assert_eq!(p.code(), 0, "stop: {}{}", p.stdout(), p.stderr());
    assert!(p.stdout().contains("stopped"), "{}", p.stdout());
}

fn serving(dir: &Path) -> chaldea_daemon::lifecycle::DiscoveryRecord {
    match discover(dir) {
        Discovery::Serving(r) => r,
        other => panic!("not serving: {other:?}"),
    }
}

/// The detach path the starter took, printed for the log of every run.
fn detach_path(stderr: &str) -> &'static str {
    if stderr.contains("detaching through launchd") {
        "launchd"
    } else if stderr.contains("detaching through setsid") {
        "setsid"
    } else {
        "windows"
    }
}

/// PR8-CL-14: `ready` → the URL and pid on stdout, exit 0; the record agrees.
fn reports_ready_url() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let (code, out, err) = start(&labels, &d.path, &[], &[]);
    eprintln!("reports_ready_url: path {}", detach_path(&err));
    assert_eq!(code, 0, "{out}{err}");
    let r = serving(&d.path);
    assert_eq!(
        out.trim(),
        format!(
            "daemon started: http://127.0.0.1:{} (pid {})",
            r.port.unwrap(),
            r.pid
        )
    );
    assert_eq!(r.detached.as_deref(), Some(detach_path(&err)));
    let port = r.port.unwrap();
    assert_eq!(
        get_status(port, "/api/threads", Some(&token_of(&d.path))),
        200
    );
    stop(&labels, &d.path);
}

/// PR8-CL-14: a start that fails (its port held by the test) prints its verdict only after the
/// child's lock decision: when the starter exits 1, the lock is free and no record is left. A
/// store reader held for 2 s from step 9 keeps the lock decision 2 s behind the failure, so a
/// verdict sent before it would reach the starter while the lock is still held.
fn fail_arrives_after_the_lock_decision() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port().to_string();
    let (code, out, err) = start(
        &labels,
        &d.path,
        &["--port", &port],
        &[(HOLD_READER_MS, OsStr::new("2000"))],
    );
    // Read at once, before anything else could change them.
    let lock = lock_state(&d.path);
    let record = d.path.join(names::RECORD).exists();
    assert_eq!(code, 1, "{out}{err}");
    assert!(
        err.contains("port_unavailable") && err.contains(&port),
        "{err}"
    );
    assert_ne!(lock, LockProbe::Held, "{err}");
    assert!(!record, "a record outlived the verdict");
    drop(held);
}

/// PR8-CL-14: a stop while the child is parked at step 9: the starter prints the stopped
/// message and exits 1; the daemon itself stops cleanly: its log shows the stop and a store
/// report with both flags, no lock held at exit, the lock is free afterwards, and its exit code
/// (recorded by the test seam just before it exits) is 0.
fn stopped_before_ready() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let gate = Gate::new();
    let exit = tempfile::tempdir().unwrap();
    let mut starter = Gated {
        gate: &gate,
        p: Proc::spawn(&mut cmd(
            &labels,
            &d.path,
            &["daemon", "start"],
            &[
                (PARK_RECONCILER, gate.path().as_os_str()),
                (EXIT_RECORD, exit.path().as_os_str()),
            ],
        )),
    };
    let starter = &mut starter.p;
    assert!(gate.reached(WAIT), "{}", starter.stderr());
    let mut stopper = Proc::spawn(&mut cmd(&labels, &d.path, &["daemon", "stop"], &[]));
    let log = d.path.join(names::LOG);
    assert!(
        wait_for(|| std::fs::read_to_string(&log).is_ok_and(|l| l.contains("stopping:"))),
        "the stop did not arrive"
    );
    gate.release();
    assert_eq!(
        starter.code(),
        1,
        "{}{}",
        starter.stdout(),
        starter.stderr()
    );
    assert!(
        starter
            .stderr()
            .contains("the daemon was stopped before it became ready"),
        "{}",
        starter.stderr()
    );
    assert_eq!(
        stopper.code(),
        0,
        "{}{}",
        stopper.stdout(),
        stopper.stderr()
    );
    let log = std::fs::read_to_string(&log).unwrap();
    assert!(
        log.contains("readers_returned true, writer_exited true"),
        "{log}"
    );
    assert!(
        !log.contains("exiting with the instance lock held"),
        "{log}"
    );
    assert!(!log.contains("ERROR"), "{log}");
    assert_ne!(lock_state(&d.path), LockProbe::Held);
    let record = exit.path().join("daemon-exit");
    assert!(wait_for(|| record.exists()), "the daemon recorded no exit");
    assert_eq!(std::fs::read_to_string(&record).unwrap(), "0");
}

/// PR8-CL-14: two starters at once: one daemon serves, both exit 0 with its URL.
fn concurrent_starts() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let mut a = Proc::spawn(&mut cmd(&labels, &d.path, &["daemon", "start"], &[]));
    let mut b = Proc::spawn(&mut cmd(&labels, &d.path, &["daemon", "start"], &[]));
    assert_eq!(a.code(), 0, "{}{}", a.stdout(), a.stderr());
    assert_eq!(b.code(), 0, "{}{}", b.stdout(), b.stderr());
    let r = serving(&d.path);
    let url = format!("http://127.0.0.1:{}", r.port.unwrap());
    assert_eq!(url_of(&a.stdout()), url);
    assert_eq!(url_of(&b.stdout()), url);
    stop(&labels, &d.path);
}

/// PR8-CL-14: the first starter's child parked between the marker's temporary and its link;
/// the second start publishes the marker and serves; the first resumes, validates the marker,
/// reports `already_running`, and exits 0 with the same URL.
fn concurrent_first_start_at_marker() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let gate = Gate::new();
    let mut first = Proc::spawn(&mut cmd(
        &labels,
        &d.path,
        &["daemon", "start"],
        &[(MARKER_PAUSE, gate.path().as_os_str())],
    ));
    assert!(gate.reached(WAIT), "{}", first.stderr());
    let (code, out, err) = start(&labels, &d.path, &[], &[]);
    assert_eq!(code, 0, "{out}{err}");
    let url = url_of(&out);
    gate.release();
    assert_eq!(first.code(), 0, "{}{}", first.stdout(), first.stderr());
    assert_eq!(url_of(&first.stdout()), url);
    assert!(
        first.stderr().contains("another daemon holds"),
        "{}",
        first.stderr()
    );
    stop(&labels, &d.path);
}

/// PR8-CL-14: the winner (parked at step 9, then failing at bind) exits during its startup;
/// the loser, waiting for it, exits 1 before its deadline.
fn loser_sees_winner_exit() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let gate = Gate::new();
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port().to_string();
    let mut winner = Proc::spawn(&mut cmd(
        &labels,
        &d.path,
        &["daemon", "start", "--port", &port],
        &[(PARK_RECONCILER, gate.path().as_os_str())],
    ));
    assert!(gate.reached(WAIT), "{}", winner.stderr());
    let t = Instant::now();
    let mut loser = Proc::spawn(&mut cmd(&labels, &d.path, &["daemon", "start"], &[]));
    assert!(
        wait_for(|| loser.stderr().contains("another daemon holds")),
        "{}",
        loser.stderr()
    );
    gate.release();
    assert_eq!(winner.code(), 1, "{}", winner.stderr());
    assert_eq!(loser.code(), 1, "{}{}", loser.stdout(), loser.stderr());
    assert!(t.elapsed() < READY_TIMEOUT, "{:?}", t.elapsed());
    assert!(loser.stderr().contains("exited"), "{}", loser.stderr());
    drop(held);
}

/// PR8-CL-14: a start with `--port` other than the serving daemon's exits 1.
fn explicit_port_conflict() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let (code, out, err) = start(&labels, &d.path, &[], &[]);
    assert_eq!(code, 0, "{out}{err}");
    let port = serving(&d.path).port.unwrap();
    let other = if port == u16::MAX { port - 1 } else { port + 1 };
    let (code, out, err) = start(&labels, &d.path, &["--port", &other.to_string()], &[]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(
        err.contains(&format!("on port {port}, not {other}")),
        "{err}"
    );
    stop(&labels, &d.path);
}

/// PR8-CL-14: a child that closes the channel without a line 13.5 s in and keeps running with
/// the lock: the follow-up is cut by the 15 s deadline, with what it last saw (the lock held,
/// or the child running), so the starter exits 1 at 15 s, before the child exits at 18.5 s.
fn eof_follow_up_within_deadline() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let t = Instant::now();
    let (code, out, err) = start(
        &labels,
        &d.path,
        &[],
        &[(EOF_CHILD_MS, OsStr::new("13500"))],
    );
    let took = t.elapsed();
    assert_eq!(code, 1, "{out}{err}");
    assert!(err.contains("exited before reporting readiness"), "{err}");
    assert!(
        err.contains("still holds the state directory") || err.contains("still running"),
        "{err}"
    );
    assert!(took >= READY_TIMEOUT, "{took:?}");
    assert!(took < READY_TIMEOUT + Duration::from_secs(1), "{took:?}");
}

/// PR8-CL-14: a child that never writes: the starter exits 4 at 15 s naming the pending
/// launch, and does not kill it; released, the child serves (its verdict write fails and is
/// ignored) until `daemon stop`.
fn timeout_after_15_s_leaves_child() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let gate = Gate::new();
    let t = Instant::now();
    let (code, out, err) = start(
        &labels,
        &d.path,
        &[],
        &[(PARK_RECONCILER, gate.path().as_os_str())],
    );
    let took = t.elapsed();
    assert_eq!(code, 4, "{out}{err}");
    assert!(took >= READY_TIMEOUT, "{took:?}");
    assert!(
        err.contains("did not report readiness within 15 s"),
        "{err}"
    );
    assert!(err.contains("the pending launch is"), "{err}");
    assert!(gate.reached(Duration::ZERO));
    let pid = match discover(&d.path) {
        Discovery::Starting(r) => r.pid,
        other => panic!("the child is gone: {other:?}"),
    };
    gate.release();
    assert!(wait_for(|| matches!(
        discover(&d.path),
        Discovery::Serving(_)
    )));
    assert_eq!(serving(&d.path).pid, pid);
    stop(&labels, &d.path);
}

/// PR8-CL-14 (D10): a child that starts only after the starter timed out and removed its
/// readiness channel (paused before it opens its writer at step 1) continues and serves.
fn late_child_serves() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let gate = Gate::new();
    let t = Instant::now();
    let (code, out, err) = start(
        &labels,
        &d.path,
        &[],
        &[(READY_PAUSE, gate.path().as_os_str())],
    );
    assert_eq!(code, 4, "{out}{err}");
    assert!(t.elapsed() >= READY_TIMEOUT, "{:?}", t.elapsed());
    assert!(
        gate.reached(Duration::ZERO),
        "the child never reached step 1"
    );
    gate.release();
    assert!(
        wait_for(|| matches!(discover(&d.path), Discovery::Serving(_))),
        "the late child did not serve"
    );
    let r = serving(&d.path);
    assert_eq!(
        get_status(r.port.unwrap(), "/api/threads", Some(&token_of(&d.path))),
        200
    );
    stop(&labels, &d.path);
}

/// PR8-CL-14: the daemon that sent `ready` is stopped, and another started for the same
/// directory, before the starter acts on the verdict: the starter reports the stop (exit 1),
/// never the stopped daemon's URL.
fn stopped_after_ready_is_not_reported_started() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let gate = Gate::new();
    let mut first = Gated {
        gate: &gate,
        p: Proc::spawn(&mut cmd(
            &labels,
            &d.path,
            &["daemon", "start"],
            &[(VERDICT_PAUSE, gate.path().as_os_str())],
        )),
    };
    let first = &mut first.p;
    assert!(gate.reached(WAIT), "{}", first.stderr());
    let stopped = serving(&d.path);
    stop(&labels, &d.path);
    // The next daemon picks a free port from the range: the stopped one's saved port may be
    // taken by another test by now.
    std::fs::remove_file(d.path.join(names::PORT)).unwrap();
    let (code, out, err) = start(&labels, &d.path, &[], &[]);
    assert_eq!(code, 0, "{out}{err}");
    let second = serving(&d.path);
    assert_ne!(second.instance, stopped.instance);
    gate.release();
    assert_eq!(first.code(), 1, "{}{}", first.stdout(), first.stderr());
    assert!(
        !first.stdout().contains("daemon started"),
        "{}",
        first.stdout()
    );
    assert!(
        first.stderr().contains("stopped before the start finished"),
        "{}",
        first.stderr()
    );
    stop(&labels, &d.path);
}

/// PR8-CL-14: a start that finds the directory held by a daemon whose HTTP surface has shut
/// down (a 6 s shutdown participant keeps the lock and the record) does not report that daemon
/// running: it waits, sees it exit, and exits 1.
fn stopping_winner_is_not_reported_running() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let (code, out, err) = start(
        &labels,
        &d.path,
        &[],
        &[(SLOW_SHUTDOWN_MS, OsStr::new("6000"))],
    );
    assert_eq!(code, 0, "{out}{err}");
    let mut stopper = Proc::spawn(&mut cmd(&labels, &d.path, &["daemon", "stop"], &[]));
    let log = d.path.join(names::LOG);
    assert!(
        wait_for(|| std::fs::read_to_string(&log).is_ok_and(|l| l.contains("http shutdown:"))),
        "the HTTP surface did not shut down"
    );
    let (code, out, err) = start(&labels, &d.path, &[], &[]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(!out.contains("already running"), "{out}");
    assert!(err.contains("exited"), "{err}");
    assert_eq!(
        stopper.code(),
        0,
        "{}{}",
        stopper.stdout(),
        stopper.stderr()
    );
    assert_eq!(discover(&d.path), Discovery::NotRunning);
}

/// PR8-CL-14: the daemon whose serving check answered 200 begins its shutdown before the
/// start's last discovery (paused between them; a 6 s shutdown participant keeps the lock and
/// the record): the record says `stopping`, so the start reports the stop (exit 1), never the
/// URL.
fn stopping_after_the_answer_is_not_reported_started() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let gate = Gate::new();
    let mut starter = Gated {
        gate: &gate,
        p: Proc::spawn(&mut cmd(
            &labels,
            &d.path,
            &["daemon", "start"],
            &[
                (SERVE_PAUSE, gate.path().as_os_str()),
                (SLOW_SHUTDOWN_MS, OsStr::new("6000")),
            ],
        )),
    };
    assert!(gate.reached(WAIT), "the serving check never answered");
    let mut stopper = Proc::spawn(&mut cmd(&labels, &d.path, &["daemon", "stop"], &[]));
    let log = d.path.join(names::LOG);
    assert!(
        wait_for(|| std::fs::read_to_string(&log).is_ok_and(|l| l.contains("http shutdown:"))),
        "the HTTP surface did not shut down"
    );
    let seen = discover(&d.path);
    assert!(matches!(seen, Discovery::Stopping(_)), "{seen:?}");
    gate.release();
    let p = &mut starter.p;
    assert_eq!(p.code(), 1, "{}{}", p.stdout(), p.stderr());
    assert!(!p.stdout().contains("daemon started"), "{}", p.stdout());
    assert!(
        p.stderr().contains("stopped before the start finished"),
        "{}",
        p.stderr()
    );
    assert_eq!(
        stopper.code(),
        0,
        "{}{}",
        stopper.stdout(),
        stopper.stderr()
    );
}

/// PR8-CL-20: `chd_`, `chdt_`, `chdp_` secrets in a message, a field, and a span field of a
/// component's `tracing` call are redacted in `daemon.log`.
fn detached_log_is_redacted() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let (code, out, err) = start(&labels, &d.path, &[], &[(LOG_MARKERS, OsStr::new("1"))]);
    assert_eq!(code, 0, "{out}{err}");
    stop(&labels, &d.path);
    let log = std::fs::read_to_string(d.path.join(names::LOG)).unwrap();
    assert!(log.contains("LOG-MARKER"), "{log}");
    for t in [TOKEN_MSG, TOKEN_FIELD, TOKEN_SPAN] {
        assert!(!log.contains(t), "{t} in {log}");
    }
}

/// PR8-CL-20: starts whose state directory path carries a token: neither the `fail` line the
/// starter repeats (a refused directory) nor the starter's own message naming the log (a child
/// that exits without a verdict) contains it.
fn startup_failure_output_is_redacted() {
    let labels = Labels::new();
    let tmp = support::temp_parent();
    let token = format!("chd_{}", "Q".repeat(43));
    let refused = tmp.path().join(&token);
    std::fs::create_dir(&refused).unwrap();
    std::fs::write(refused.join("unrelated"), b"x").unwrap();
    let (code, out, err) = start(&labels, &refused, &[], &[]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(err.contains("state_dir"), "{err}");
    assert!(err.contains("chd_<redacted>"), "{err}");
    assert!(!out.contains(&token) && !err.contains(&token), "{out}{err}");
    let silent = tmp.path().join("s").join(&token);
    let (code, out, err) = start(&labels, &silent, &[], &[(EOF_CHILD_MS, OsStr::new("0"))]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(err.contains("exited before reporting readiness"), "{err}");
    assert!(err.contains("chd_<redacted>"), "{err}");
    assert!(!out.contains(&token) && !err.contains(&token), "{out}{err}");
}

/// PR8-CL-23: a relative `--state-dir` from another working directory: the child serves the
/// absolute directory (its own working directory is the temp directory).
fn child_receives_the_resolved_dir() {
    let labels = Labels::new();
    let d: Dir = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let mut c = faults::chaldea(&labels, &[OsStr::new("--state-dir"), OsStr::new("state")]);
    c.args(["daemon", "start"]).current_dir(d.tmp.path());
    let mut p = Proc::spawn(&mut c);
    assert_eq!(p.code(), 0, "{}{}", p.stdout(), p.stderr());
    let r = serving(&d.path);
    assert!(
        p.stdout().contains(&format!("(pid {})", r.pid)),
        "{}",
        p.stdout()
    );
    stop(&labels, &d.path);
}
