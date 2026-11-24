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
    self, DEADLINE_MS, EOF_CHILD_MS, EOF_PAST_DEADLINE, EXIT_RECORD, Gate, HOLD_READER_MS,
    LOG_MARKERS, Labels, MARKER_PAUSE, PARK_RECONCILER, READY_PAUSE, RECHECK_MARK,
    SERVE_PAST_DEADLINE, SERVE_PAUSE, SHUTDOWN_GATE, TOKEN_FIELD, TOKEN_MSG, TOKEN_SPAN,
    VERDICT_PAUSE,
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
            name: "late_discovery_after_the_answer_claims_no_state",
            run: late_discovery_after_the_answer_claims_no_state,
        },
        Case {
            name: "unconfirmed_winner_claims_no_holder",
            run: unconfirmed_winner_claims_no_holder,
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

/// A deadline the test holds for a command whose wait spans the test's own steps (a stop, a
/// start), so those steps never race the design's 15 s; below every gate's 60 s.
const HELD_DEADLINE: &str = "45000";

/// Releases a gate on every ending of the test.
struct ReleaseOnDrop<'a>(&'a Gate);

impl Drop for ReleaseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

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
/// the loser, waiting for it, exits 1 before its deadline (both hold a test deadline, so the
/// test's steps never race 15 s).
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
        &[
            (PARK_RECONCILER, gate.path().as_os_str()),
            (DEADLINE_MS, OsStr::new(HELD_DEADLINE)),
        ],
    ));
    assert!(gate.reached(WAIT), "{}", winner.stderr());
    let mut loser = Proc::spawn(&mut cmd(
        &labels,
        &d.path,
        &["daemon", "start"],
        &[(DEADLINE_MS, OsStr::new(HELD_DEADLINE))],
    ));
    assert!(
        wait_for(|| loser.stderr().contains("another daemon holds")),
        "{}",
        loser.stderr()
    );
    gate.release();
    assert_eq!(winner.code(), 1, "{}", winner.stderr());
    // Exit 1, not 4: the loser saw the winner's exit before its deadline.
    assert_eq!(loser.code(), 1, "{}{}", loser.stdout(), loser.stderr());
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

/// PR8-CL-14: a child that closes the channel without a line at once (holding the lock for
/// 5 s more), and a starter that reads the end and then waits until its deadline has passed
/// before its follow-up: the follow-up is bounded by that deadline, so it observes nothing and
/// the starter exits 1 at once, reporting the closed channel and that nothing (macOS) or no
/// exit (Windows) was observed before the deadline. A follow-up that ignored the deadline
/// would see the lock free or the child's exit instead.
fn eof_follow_up_within_deadline() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let past = Gate::new();
    let t = Instant::now();
    let (code, out, err) = start(
        &labels,
        &d.path,
        &[],
        &[
            (EOF_CHILD_MS, OsStr::new("0")),
            (EOF_PAST_DEADLINE, past.path().as_os_str()),
        ],
    );
    assert!(
        past.reached(Duration::ZERO),
        "the start never held the end past its deadline: {out}{err}"
    );
    assert_eq!(code, 1, "{out}{err}");
    assert!(
        err.contains("closed its readiness channel without a verdict"),
        "{err}"
    );
    #[cfg(not(windows))]
    {
        assert!(
            err.contains("nothing could be observed before the deadline"),
            "{err}"
        );
        assert!(!err.contains("holds the state directory"), "{err}");
    }
    // The exit wait ends at the deadline: that end is no observation of the daemon's state.
    #[cfg(windows)]
    {
        assert!(
            err.contains("exit was not observed before the deadline"),
            "{err}"
        );
        assert!(!err.contains("it exited"), "{err}");
        assert!(!err.contains("it is still running"), "{err}");
        assert!(
            !err.contains("the daemon exited before reporting readiness"),
            "{err}"
        );
    }
    assert!(t.elapsed() >= READY_TIMEOUT, "{:?}", t.elapsed());
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
/// never the stopped daemon's URL. The starter holds a test deadline, so the stop and the
/// second start never race its 15 s.
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
            &[
                (VERDICT_PAUSE, gate.path().as_os_str()),
                (DEADLINE_MS, OsStr::new(HELD_DEADLINE)),
            ],
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
/// down (a shutdown participant holds it stopping, with its lock and record, until the test
/// releases it) does not report that daemon running: it gets `already_running`, and once the
/// test releases the shutdown it sees the daemon exit and exits 1.
fn stopping_winner_is_not_reported_running() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let held = Gate::new();
    let _release = ReleaseOnDrop(&held);
    let (code, out, err) = start(
        &labels,
        &d.path,
        &[],
        &[(SHUTDOWN_GATE, held.path().as_os_str())],
    );
    assert_eq!(code, 0, "{out}{err}");
    let deadline = [(DEADLINE_MS, OsStr::new(HELD_DEADLINE))];
    let mut stopper = Proc::spawn(&mut cmd(&labels, &d.path, &["daemon", "stop"], &deadline));
    assert!(
        held.reached(WAIT),
        "the shutdown never reached its held participant"
    );
    let seen = discover(&d.path);
    assert!(matches!(seen, Discovery::Stopping(_)), "{seen:?}");
    let mut second = Proc::spawn(&mut cmd(&labels, &d.path, &["daemon", "start"], &deadline));
    assert!(
        wait_for(|| second.stderr().contains("another daemon holds")),
        "{}",
        second.stderr()
    );
    held.release();
    assert_eq!(second.code(), 1, "{}{}", second.stdout(), second.stderr());
    assert!(
        !second.stdout().contains("already running"),
        "{}",
        second.stdout()
    );
    assert!(second.stderr().contains("exited"), "{}", second.stderr());
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
/// start's last discovery (paused between them; a shutdown participant holds the daemon
/// stopping, with its lock and its `stopping` record, until the test releases it): that
/// discovery, which the test sees happen while the daemon is held, finds `stopping`, so once
/// the daemon exits the start reports the stop (exit 1), never the URL. The starter and the
/// stop hold a test deadline, so the test's steps never race 15 s.
fn stopping_after_the_answer_is_not_reported_started() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let gate = Gate::new();
    let held = Gate::new();
    let _release = ReleaseOnDrop(&held);
    let recheck = Gate::new();
    let mut starter = Gated {
        gate: &gate,
        p: Proc::spawn(&mut cmd(
            &labels,
            &d.path,
            &["daemon", "start"],
            &[
                (SERVE_PAUSE, gate.path().as_os_str()),
                (RECHECK_MARK, recheck.path().as_os_str()),
                (SHUTDOWN_GATE, held.path().as_os_str()),
                (DEADLINE_MS, OsStr::new(HELD_DEADLINE)),
            ],
        )),
    };
    assert!(gate.reached(WAIT), "the serving check never answered");
    let mut stopper = Proc::spawn(&mut cmd(
        &labels,
        &d.path,
        &["daemon", "stop"],
        &[(DEADLINE_MS, OsStr::new(HELD_DEADLINE))],
    ));
    assert!(
        held.reached(WAIT),
        "the shutdown never reached its held participant"
    );
    let seen = discover(&d.path);
    assert!(matches!(seen, Discovery::Stopping(_)), "{seen:?}");
    gate.release();
    assert!(
        recheck.reached(WAIT),
        "the start's last discovery did not run while the daemon was held"
    );
    held.release();
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

/// PR8-CL-14: the serving check answered 200, and the starter then waits until its deadline
/// has passed before its last discovery (the seam marks that it got there): the start times
/// out (exit 4) saying that serving was not confirmed, never that the daemon did not answer.
/// The daemon serves on.
fn late_discovery_after_the_answer_claims_no_state() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let past = Gate::new();
    let (code, out, err) = start(
        &labels,
        &d.path,
        &[],
        &[(SERVE_PAST_DEADLINE, past.path().as_os_str())],
    );
    assert!(
        past.reached(Duration::ZERO),
        "the start never held a 200 past its deadline: {out}{err}"
    );
    assert_eq!(code, 4, "{out}{err}");
    assert!(err.contains("was not confirmed within 15 s"), "{err}");
    assert!(!err.contains("did not answer"), "{err}");
    assert!(!out.contains("daemon started"), "{out}");
    let _ = serving(&d.path);
    stop(&labels, &d.path);
}

/// PR8-CL-14: a winner parked at step 9 (starting, the lock held) for longer than a second
/// start's deadline: that start gets `already_running` and times out (exit 4) saying no daemon
/// was confirmed serving, never asserting that a daemon holds the directory.
fn unconfirmed_winner_claims_no_holder() {
    let labels = Labels::new();
    let d = state();
    let _stop = StopOnDrop(&labels, &d.path);
    let gate = Gate::new();
    let _winner = Gated {
        gate: &gate,
        p: Proc::spawn(&mut cmd(
            &labels,
            &d.path,
            &["daemon", "start"],
            &[(PARK_RECONCILER, gate.path().as_os_str())],
        )),
    };
    assert!(gate.reached(WAIT), "the winner never parked");
    let (code, out, err) = start(&labels, &d.path, &[], &[]);
    assert_eq!(code, 4, "{out}{err}");
    assert!(err.contains("another daemon holds"), "{err}");
    assert!(
        err.contains("no daemon was confirmed serving") && err.contains("within 15 s"),
        "{err}"
    );
    assert!(!err.contains("but did not report serving"), "{err}");
    gate.release();
    assert!(wait_for(|| matches!(
        discover(&d.path),
        Discovery::Serving(_)
    )));
    stop(&labels, &d.path);
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
    assert!(
        err.contains("closed its readiness channel without a verdict"),
        "{err}"
    );
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
