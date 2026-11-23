//! Windows binary tests: PR8-CL-9 (Ctrl-C and Ctrl-Break stop a serving daemon, also one that
//! inherited Ctrl-C ignored), the stop listener half of PR8-CL-15 (the stop event is
//! global, owner-only, and stops the daemon; a live target without an event is reported),
//! process setup without a console, PR8-CL-11 and CL-12 (the parent job: `daemon start` breaks
//! away when the job allows it and refuses otherwise; a detached child in any job fails; a
//! foreground daemon in a job warns), and CL-13's console half (a detached daemon has none).
//!
//! The job cases run `daemon start` in a helper this test assigns to a `fake::TestJob` before
//! telling it to go (on stdin); each prints its steps to stderr, so a CI log names the step a
//! failure stopped at.
#![cfg(windows)]

mod support;

use std::ffi::OsStr;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use chaldea_daemon::lifecycle::{
    Discovery, DiscoveryRecord, Evidence, LockProbe, RecordState, discover, names,
};
use chaldea_pty_runtime::fake::{self, CtrlEvent, TestJob};
use chaldea_pty_runtime::host::{self, JobLimits};
use support::{
    Proc, chaldea, free_port, get_status, lock_state, marked_dir, report, run_cmd, run_helper,
    run_to_exit, serving_record, state, token_of, wait_for,
};

/// `CREATE_NEW_CONSOLE` and `CREATE_NEW_PROCESS_GROUP` (`winbase.h`).
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

const EVENT: &str = "CHALDEA_CL9_EVENT";

/// In a helper on its own console: ignore Ctrl-C (an attribute the daemon inherits, as from
/// `start /b`), start a daemon there, send the event, and report the exit code and the lock.
/// Each step is printed to stderr, so a failure names the step it stopped at.
fn on_new_console(event: &str) {
    let dir = state();
    fake::ignore_ctrl_c().unwrap();
    eprintln!("cl9 {event}: the helper ignores Ctrl-C; the daemon inherits that");
    let mut cmd = run_cmd(&dir.path, Some(free_port()));
    if event == "break" {
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let mut d = Proc::spawn(&mut cmd);
    eprintln!("cl9 {event}: daemon pid {}", d.pid());
    let port = d.serving();
    eprintln!("cl9 {event}: serving on {port}");
    let attached = fake::console_processes().unwrap();
    eprintln!("cl9 {event}: console processes {attached:?}");
    assert!(
        attached.contains(&d.pid()),
        "{attached:?} lacks {}",
        d.pid()
    );
    match event {
        "c" => fake::send_ctrl_event(CtrlEvent::CtrlC, 0).unwrap(),
        _ => fake::send_ctrl_event(CtrlEvent::CtrlBreak, d.pid()).unwrap(),
    }
    eprintln!("cl9 {event}: event sent");
    let status = d.wait_within(Duration::from_secs(20));
    eprintln!(
        "cl9 {event}: daemon exited {status:?}; its stderr:\n{}",
        d.stderr()
    );
    report(&format!(
        "code={} lock={:?}",
        status.code().unwrap_or(-1),
        lock_state(&dir.path)
    ));
}

#[test]
fn ctrl_c_and_ctrl_break_stop() {
    if let Ok(event) = std::env::var(EVENT) {
        on_new_console(&event);
        return;
    }
    for event in ["c", "break"] {
        let got = run_helper(
            "ctrl_c_and_ctrl_break_stop",
            &[(EVENT, OsStr::new(event))],
            Duration::from_secs(90),
            |c| {
                c.creation_flags(CREATE_NEW_CONSOLE);
            },
        );
        assert_eq!(got, format!("code=0 lock={:?}", LockProbe::Free), "{event}");
    }
}

#[test]
fn stop_event_is_global() {
    let dir = state();
    let mut d = Proc::spawn(&mut run_cmd(&dir.path, Some(free_port())));
    d.serving();
    let r = serving_record(&dir.path);
    let name = host::stop_event_name(&r.instance);
    assert!(name.starts_with("Global\\"), "{name}");
    let (_owner, allows) = fake::event_sids(&name).unwrap();
    assert_eq!(allows, vec![fake::user_sid().unwrap()]);
    fake::set_stop_event(&name).unwrap();
    assert_eq!(d.code(), 0, "{}", d.stderr());
    assert_eq!(lock_state(&dir.path), LockProbe::Free);
}

/// Process setup without a console (`DETACHED_PROCESS`): the daemon starts, and the stop
/// event stops it.
#[test]
fn setup_without_console_can_start_and_stop() {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let dir = state();
    let mut cmd = run_cmd(&dir.path, Some(free_port()));
    cmd.creation_flags(DETACHED_PROCESS);
    let mut daemon = Proc::spawn(&mut cmd);

    daemon.serving();
    let record = serving_record(&dir.path);
    assert_eq!(record.pid, daemon.pid());
    fake::set_stop_event(&host::stop_event_name(&record.instance)).unwrap();
    assert_eq!(daemon.code(), 0, "{}", daemon.stderr());
    assert_eq!(lock_state(&dir.path), LockProbe::Free);
}

// ---- The parent job (PR8-CL-11, CL-12) and the console (CL-13) ----

const ROLE: &str = "CHALDEA_JOB_ROLE";
const DIR: &str = "CHALDEA_JOB_DIR";

fn limits(kill_on_close: bool, breakaway_ok: bool, silent_breakaway_ok: bool) -> JobLimits {
    JobLimits {
        kill_on_close,
        breakaway_ok,
        silent_breakaway_ok,
    }
}

/// Waits for `go` on stdin (sent once the parent assigned this helper to its jobs).
fn wait_go() {
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    eprintln!(
        "helper {}: go; job state {:?}",
        std::process::id(),
        host::job_state()
    );
}

/// Runs `test` as a helper with `role` and the state directory `dir`, assigned to `jobs` in
/// order (a second job nests under the first) before it goes; returns its report.
fn helper_in_jobs(test: &str, role: &str, dir: &Path, jobs: &[&TestJob]) -> String {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(ROLE, role)
        .env(DIR, dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut p = Proc::spawn(&mut cmd);
    for j in jobs {
        j.assign(p.pid()).unwrap();
    }
    eprintln!(
        "parent: helper {} assigned to {} job(s)",
        p.pid(),
        jobs.len()
    );
    p.child.stdin.take().unwrap().write_all(b"go\n").unwrap();
    let code = p.wait_within(Duration::from_secs(90)).code();
    let out = p.stdout();
    eprintln!(
        "parent: helper {role} exited {code:?}; stderr:\n{}",
        p.stderr()
    );
    assert_eq!(code, Some(0), "helper {role}\nstdout:\n{out}");
    out.lines()
        .find_map(|l| l.find("@@ ").map(|i| l[i + 3..].to_owned()))
        .unwrap_or_else(|| panic!("no report from helper {role}\nstdout:\n{out}"))
}

/// The helper's `chaldea --state-dir <dir> daemon start`: `code=<n>` and its output.
fn start_in_helper(dir: &Path) {
    wait_go();
    let (code, out, err) = run_to_exit(
        chaldea(&[], &[])
            .arg("--state-dir")
            .arg(dir)
            .args(["daemon", "start"]),
    );
    eprintln!("helper: daemon start exited {code}\nstdout:\n{out}stderr:\n{err}");
    report(&format!("code={code} {}", err.replace('\n', " ")));
}

/// Stops the daemon serving `dir` (through `daemon stop`), if any.
fn stop_dir(dir: &Path) {
    if !matches!(discover(dir), Discovery::NotRunning) {
        let (code, out, err) = run_to_exit(
            chaldea(&[], &[])
                .arg("--state-dir")
                .arg(dir)
                .args(["daemon", "stop"]),
        );
        eprintln!("stop: {code} {out}{err}");
    }
}

struct StopOnDrop<'a>(&'a Path);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        stop_dir(self.0);
    }
}

/// PR8-CL-11: started from inside a kill-on-close job with `limits`, the daemon runs in no
/// job, records so, and keeps serving after the job is closed.
fn breaks_away(test: &str, limits: JobLimits) {
    if std::env::var(ROLE).as_deref() == Ok("start") {
        start_in_helper(Path::new(&std::env::var_os(DIR).unwrap()));
        return;
    }
    let d = state();
    let _stop = StopOnDrop(&d.path);
    let job = TestJob::new(limits).unwrap();
    let got = helper_in_jobs(test, "start", &d.path, &[&job]);
    assert!(got.starts_with("code=0"), "{got}");
    let r = serving_record(&d.path);
    eprintln!("parent: daemon pid {}, evidence {:?}", r.pid, r.evidence);
    assert!(
        !job.contains(r.pid).unwrap(),
        "the daemon is in the test's job"
    );
    assert!(
        !fake::process_in_any_job(r.pid).unwrap(),
        "the daemon is in a job"
    );
    let ev = r.evidence.job.clone().expect("job evidence");
    assert_eq!(ev["in_job"], false, "{ev}");
    assert_eq!(r.detached.as_deref(), Some("windows"));
    job.close();
    std::thread::sleep(Duration::from_millis(500));
    let after = serving_record(&d.path);
    assert_eq!(after.instance, r.instance);
    assert_eq!(
        get_status(
            after.port.unwrap(),
            "/api/threads",
            Some(&token_of(&d.path))
        ),
        200
    );
    let (code, out, err) = run_to_exit(
        chaldea(&[], &[])
            .arg("--state-dir")
            .arg(&d.path)
            .args(["daemon", "stop"]),
    );
    assert_eq!(code, 0, "{out}{err}");
}

#[test]
fn start_breaks_away_from_a_kill_on_close_job_with_breakaway() {
    breaks_away(
        "start_breaks_away_from_a_kill_on_close_job_with_breakaway",
        limits(true, true, false),
    );
}

#[test]
fn start_breaks_away_from_a_kill_on_close_job_with_silent_breakaway() {
    breaks_away(
        "start_breaks_away_from_a_kill_on_close_job_with_silent_breakaway",
        limits(true, false, true),
    );
}

/// PR8-CL-12: inside a job that allows no breakaway, `daemon start` exits 1 with the refusal
/// before any spawn: no daemon, no state directory.
#[test]
fn start_refuses_a_job_without_breakaway() {
    if std::env::var(ROLE).as_deref() == Ok("start") {
        start_in_helper(Path::new(&std::env::var_os(DIR).unwrap()));
        return;
    }
    let d = state();
    let _stop = StopOnDrop(&d.path);
    let job = TestJob::new(limits(true, false, false)).unwrap();
    let got = helper_in_jobs(
        "start_refuses_a_job_without_breakaway",
        "start",
        &d.path,
        &[&job],
    );
    assert!(got.starts_with("code=1"), "{got}");
    assert!(
        got.contains("runs inside a job that Chaldea cannot leave"),
        "{got}"
    );
    assert!(!d.path.exists(), "the state directory was created");
}

/// PR8-CL-12: a detached child launched directly into a job (no starter) fails with
/// `parent_job`, exits 1, and leaves the lock untaken.
#[test]
fn child_rejects_any_job() {
    if std::env::var(ROLE).as_deref() == Ok("child") {
        wait_go();
        let dir = std::env::var_os(DIR).unwrap();
        let mut c = chaldea(&[], &[]);
        c.arg("--state-dir").arg(&dir).args([
            "daemon",
            "run",
            "--detached-child",
            "--ready",
            "stdout",
        ]);
        let mut p = Proc::spawn(&mut c);
        let code = p.wait_within(Duration::from_secs(30)).code();
        report(&format!(
            "code={code:?} line={}",
            p.stdout().lines().next().unwrap_or("")
        ));
        return;
    }
    let d = state();
    let _stop = StopOnDrop(&d.path);
    let job = TestJob::new(limits(true, false, false)).unwrap();
    let got = helper_in_jobs("child_rejects_any_job", "child", &d.path, &[&job]);
    assert!(
        got.starts_with("code=Some(1) line=fail parent_job "),
        "{got}"
    );
    assert_ne!(lock_state_or_missing(&d.path), LockProbe::Held);
}

fn lock_state_or_missing(dir: &Path) -> LockProbe {
    chaldea_daemon::lifecycle::probe(dir).unwrap_or(LockProbe::Missing)
}

/// PR8-CL-12: an outer kill-on-close job without breakaway and an inner one with
/// `BREAKAWAY_OK`: the start fails (the starter's refusal or the child's `parent_job`), never
/// a daemon left in a job.
#[test]
fn nested_outer_job_refuses() {
    if std::env::var(ROLE).as_deref() == Ok("start") {
        start_in_helper(Path::new(&std::env::var_os(DIR).unwrap()));
        return;
    }
    let d = state();
    let _stop = StopOnDrop(&d.path);
    let outer = TestJob::new(limits(true, false, false)).unwrap();
    let inner = TestJob::new(limits(false, true, false)).unwrap();
    let got = helper_in_jobs(
        "nested_outer_job_refuses",
        "start",
        &d.path,
        &[&outer, &inner],
    );
    assert!(got.starts_with("code=1"), "{got}");
    assert!(
        got.contains("parent_job") || got.contains("cannot leave"),
        "{got}"
    );
    assert!(matches!(discover(&d.path), Discovery::NotRunning));
}

/// PR8-CL-12: a foreground daemon in a job warns and serves.
#[test]
fn foreground_in_a_job_warns() {
    if std::env::var(ROLE).as_deref() == Ok("run") {
        wait_go();
        let dir = std::path::PathBuf::from(std::env::var_os(DIR).unwrap());
        let mut daemon = Proc::spawn(&mut run_cmd(&dir, None));
        daemon.serving();
        let warned = daemon.stderr().contains("runs inside a job");
        let code = daemon.stop_and_wait(&dir);
        report(&format!("warned={warned} code={code}"));
        return;
    }
    let d = state();
    let _stop = StopOnDrop(&d.path);
    let job = TestJob::new(limits(true, false, false)).unwrap();
    let got = helper_in_jobs("foreground_in_a_job_warns", "run", &d.path, &[&job]);
    assert_eq!(got, "warned=true code=0");
}

/// PR8-CL-13 (Windows): a detached daemon has no console and records so.
#[test]
fn detached_child_has_no_console() {
    let d = state();
    let _stop = StopOnDrop(&d.path);
    let (code, out, err) = run_to_exit(
        chaldea(&[], &[])
            .arg("--state-dir")
            .arg(&d.path)
            .args(["daemon", "start"]),
    );
    assert_eq!(code, 0, "{out}{err}");
    let r = serving_record(&d.path);
    assert_eq!(r.evidence.console, Some(false), "{:?}", r.evidence);
    assert_eq!(r.detached.as_deref(), Some("windows"));
}

/// PR8-CL-15: a live process holding the lock with a valid record of itself and no stop event:
/// `daemon stop` reports it and exits 1, signalling nothing.
#[test]
fn no_listener_is_reported() {
    if std::env::var(ROLE).as_deref() == Ok("holder") {
        let dir = std::path::PathBuf::from(std::env::var_os(DIR).unwrap());
        let lock = std::fs::File::create(dir.join(names::LOCK)).unwrap();
        lock.try_lock().unwrap();
        let me = host::current_identity().unwrap();
        let record = DiscoveryRecord {
            version: 1,
            instance: uuid::Uuid::new_v4().to_string(),
            pid: me.pid,
            start_time: me.start_time,
            state: RecordState::Serving,
            port: Some(1),
            url: None,
            started_at: String::new(),
            detached: None,
            evidence: Evidence::default(),
        };
        support::write_private(
            &dir.join(names::RECORD),
            &serde_json::to_vec(&record).unwrap(),
        );
        report("holding");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        drop(lock);
        return;
    }
    let d = state();
    marked_dir(&d.path);
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "no_listener_is_reported",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(ROLE, "holder")
    .env(DIR, &d.path)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut holder = Proc::spawn(&mut cmd);
    assert!(
        wait_for(|| holder.stdout().contains("@@ holding")),
        "{}",
        holder.stderr()
    );
    let (code, out, err) = run_to_exit(
        chaldea(&[], &[])
            .arg("--state-dir")
            .arg(&d.path)
            .args(["daemon", "stop"]),
    );
    assert_eq!(code, 1, "{out}{err}");
    assert!(err.contains("without a stop listener"), "{err}");
    assert!(
        holder.child.try_wait().unwrap().is_none(),
        "the holder was stopped"
    );
    drop(holder.child.stdin.take());
    assert_eq!(holder.code(), 0, "{}", holder.stderr());
}
