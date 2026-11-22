//! The native Windows backend with FakeCli (`docs/design/pty-windows.md` §7). Every test
//! holds `LOCK` and a `Cleanup` that kills its fixtures by verified identity on any exit.
//! Process death is observed through `fake::windows_watch` handles, never by a stream
//! closing alone.

#![cfg(windows)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chaldea_pty_runtime::fake::{
    FaultReport, Watch, WindowsFault, gate, identify, kill_verified, process_handle_count,
    windows_close_threads_exited, windows_fault_report, windows_in_job, windows_inject_next,
    windows_job_active, windows_on_teardown_kill, windows_output_parked,
    windows_output_parked_at_close, windows_track_close_threads, windows_watch,
};
use chaldea_pty_runtime::{
    Cleanup, CloseBounds, Guard, OutputEnd, ProcessIdentity, PtySize, Session, SessionConfig,
    SessionEvent, SpawnError, SpawnSpec, StartFault, WriteOrigin, WriteOutcome, WriteUnit,
    completion_channel, native,
};
use chaldea_testkit::report::{Cleanup as FixtureCleanup, Fixture, ReportListener};

const BIN: &str = env!("CARGO_BIN_EXE_chaldea-fake-cli");
const WAIT: Duration = Duration::from_secs(10);
const KILL: Duration = Duration::from_secs(5);
const HELPER_ENV: &str = "CHALDEA_PTY_WINDOWS_HELPER";

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn soon() -> Instant {
    Instant::now() + WAIT
}

fn start_of(pid: u32) -> Option<String> {
    identify(pid).ok().flatten().map(|i| i.start_time)
}

fn fixture_cleanup() -> FixtureCleanup {
    FixtureCleanup::new(Some(start_of)).kill_with(kill_verified)
}

fn system_root() -> OsString {
    std::env::var_os("SystemRoot").expect("SystemRoot is set on Windows")
}

fn temp_cwd() -> PathBuf {
    std::env::temp_dir()
}

/// A fresh file path in the OS temp dir, removed on drop.
struct TempPath(PathBuf);

impl TempPath {
    fn new(tag: &str) -> TempPath {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        TempPath(std::env::temp_dir().join(format!(
            "chaldea-ptywin-{tag}-{}-{n}.json",
            std::process::id()
        )))
    }

    fn arg(&self) -> String {
        self.0.to_str().unwrap().to_owned()
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn spec(argv: Vec<OsString>, env: Vec<(OsString, OsString)>, size: PtySize) -> SpawnSpec {
    SpawnSpec {
        argv,
        cwd: temp_cwd(),
        env,
        size,
    }
}

fn os(v: &[&str]) -> Vec<OsString> {
    v.iter().map(OsString::from).collect()
}

/// FakeCli `args` with a report stream on `report` (and `SystemRoot`, which Winsock needs).
fn fake_spec(report: &str, args: &[&str]) -> SpawnSpec {
    let mut argv = os(&[BIN, "--report", report, "--max-life-secs", "60"]);
    argv.extend(os(args));
    spec(
        argv,
        vec![("SystemRoot".into(), system_root())],
        PtySize::new(80, 24).unwrap(),
    )
}

/// Session events, delivered to a channel.
fn events() -> (SessionConfig, Receiver<SessionEvent>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    (
        SessionConfig::new(move |e| {
            let _ = tx.lock().unwrap().send(e);
        }),
        rx,
    )
}

fn wait_exit(rx: &Receiver<SessionEvent>) -> i32 {
    let end = soon();
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if let SessionEvent::Exited(e) = rx.recv_timeout(left).expect("no exit event") {
            return e.code.expect("an exit code");
        }
    }
}

/// A session running FakeCli with a report stream, its listener and root fixture, its
/// events, and a cleanup tracking every fixture.
struct Rig {
    session: Session,
    listener: ReportListener,
    root: Fixture,
    cleanup: FixtureCleanup,
    #[allow(dead_code, reason = "kept so the event sender never fails")]
    events: Receiver<SessionEvent>,
}

fn rig_spec(listener: ReportListener, s: &SpawnSpec) -> Rig {
    let (config, events) = events();
    let session = Session::spawn(&*native(), s, config).unwrap_or_else(|e| panic!("{e}"));
    let mut cleanup = fixture_cleanup();
    let root = listener.accept(soon()).unwrap();
    cleanup.track(root.hello.pid);
    Rig {
        session,
        listener,
        root,
        cleanup,
        events,
    }
}

fn rig(args: &[&str]) -> Rig {
    let listener = ReportListener::bind().unwrap();
    let s = fake_spec(&listener.arg(), args);
    rig_spec(listener, &s)
}

impl Rig {
    fn roles(&mut self, roles: &[&str]) -> BTreeMap<String, Fixture> {
        let got = self.listener.expect_roles(roles, soon()).unwrap();
        for f in got.values() {
            self.cleanup.track(f.hello.pid);
        }
        got
    }
}

/// A FakeCli `role` sleeper the test starts itself, outside the session's job, on its own
/// listener.
fn outside(cleanup: &mut FixtureCleanup, role: &str) -> Fixture {
    let l = ReportListener::bind().unwrap();
    let child = Command::new(BIN)
        .args(["--mode", "sleeper", "--role", role, "--report", &l.arg()])
        .args(["--max-life-secs", "60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    cleanup.own(child);
    let f = l.accept(soon()).unwrap();
    assert_eq!(f.hello.role, role);
    f
}

fn id_of(f: &Fixture) -> ProcessIdentity {
    identify(f.hello.pid)
        .unwrap()
        .unwrap_or_else(|| panic!("{} is not alive", f.hello.role))
}

fn watch(f: &Fixture) -> Watch {
    windows_watch(&id_of(f)).unwrap()
}

fn next_kind(f: &mut Fixture, kind: &str) -> serde_json::Value {
    loop {
        let v = f.next_event(soon()).unwrap().expect("stream closed");
        if v["event"] == kind {
            return v;
        }
    }
}

fn evidence(s: &Session) -> BTreeMap<&'static str, String> {
    s.evidence().iter().cloned().collect()
}

/// Reads a `report-process` file.
fn read_report(p: &TempPath) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(&p.0).expect("report-process wrote its file")).unwrap()
}

fn strings(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_owned())
        .collect()
}

/// Runs `report-process` through the backend with `extra` args after `--` and `env`; the
/// file's JSON and the spec's argv.
fn report_process(
    extra: &[&str],
    env: Vec<(OsString, OsString)>,
    fault: Option<WindowsFault>,
    cwd: Option<PathBuf>,
) -> (serde_json::Value, Vec<String>) {
    let out = TempPath::new("report");
    let mut argv = os(&[BIN, "--mode", "report-process", "--out", &out.arg(), "--"]);
    argv.extend(os(extra));
    let mut s = spec(argv.clone(), env, PtySize::new(80, 24).unwrap());
    if let Some(c) = cwd {
        s.cwd = c;
    }
    if let Some(f) = fault {
        windows_inject_next(f);
    }
    let (config, rx) = events();
    let session = Session::spawn(&*native(), &s, config).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(wait_exit(&rx), 0);
    let close = session.close(&[], CloseBounds::default());
    assert!(close.released.tree_dead, "{close:?}");
    let v = read_report(&out);
    (
        v,
        argv.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
    )
}

// ---- CL-1 ----

/// The length of one argument quoted by std's rule.
fn quoted_len(a: &str) -> usize {
    let units: Vec<u16> = a.encode_utf16().collect();
    let quote = units.is_empty() || units.iter().any(|&c| c == 0x20 || c == 0x09);
    let mut n = 0;
    let mut backslashes = 0;
    for &c in &units {
        if c == u16::from(b'\\') {
            backslashes += 1;
        } else {
            if c == u16::from(b'"') {
                n += backslashes + 1;
            }
            backslashes = 0;
        }
        n += 1;
    }
    if quote {
        n += 2 + backslashes;
    }
    n
}

#[test]
fn command_line_at_the_limit_spawns() {
    let _l = lock();
    let out = TempPath::new("limit");
    let fixed = ["--mode", "report-process", "--out", &out.arg(), "--"];
    // `"<BIN>"`, each argument after a space, and the NUL.
    let used = BIN.encode_utf16().count()
        + 2
        + fixed.iter().map(|a| 1 + quoted_len(a)).sum::<usize>()
        + 1
        + 1;
    let pad = "x".repeat(32_767 - used);
    let mut argv = os(&[BIN]);
    argv.extend(os(&fixed));
    argv.push(pad.clone().into());
    let s = spec(argv.clone(), Vec::new(), PtySize::new(80, 24).unwrap());
    let (config, rx) = events();
    let session = Session::spawn(&*native(), &s, config).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(wait_exit(&rx), 0);
    let _ = session.close(&[], CloseBounds::default());
    let v = read_report(&out);
    let got = strings(&v["argv"]);
    let want: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(got, want);
}

// ---- CL-3, CL-7: handle counts in a quiescent helper ----

/// Runs this test binary again in helper mode for `test`; asserts the helper passed.
fn run_helper(test: &str, case: &str, piped: bool) {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args(["--exact", test, "--nocapture", "--test-threads", "1"])
        .env(HELPER_ENV, case);
    if piped {
        c.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    }
    let out = chaldea_pty_runtime::with_spawn_lock(|| c.spawn()).unwrap();
    let out = out.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "helper {case} failed: {:?}\n{}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn helper_case() -> Option<String> {
    std::env::var(HELPER_ENV).ok()
}

/// One warm-up cycle gives the baseline; five measured cycles must each equal it. Close
/// workers' handles are tracked so their threads' exit can be awaited before each sample.
fn handle_cycles(label: &str, mut cycle: impl FnMut()) {
    windows_track_close_threads(true);
    let mut sample = || {
        cycle();
        assert!(
            windows_close_threads_exited(WAIT),
            "{label}: a close thread did not exit"
        );
        process_handle_count().unwrap_or_else(|| panic!("{label}: no handle count"))
    };
    let baseline = sample();
    for i in 0..5 {
        assert_eq!(sample(), baseline, "{label}: cycle {i}");
    }
}

fn the_report() -> FaultReport {
    windows_fault_report().expect("a native spawn ran on this thread")
}

fn pid_of_create(r: &FaultReport) -> Option<u32> {
    r.entries("create_process")
        .iter()
        .find_map(|e| e.result.ok())
        .and_then(|p| u32::try_from(p).ok())
}

const PRE_PROCESS: &[&str] = &[
    "FirstPipe",
    "SecondPipe",
    "CloseWorker",
    "PseudoConsole",
    "DrainStart",
    "JobCreate",
    "JobLimits",
    "AttrInit",
    "PseudoConsoleAttr",
    "CreateProcess",
];

fn pre_fault(name: &str) -> WindowsFault {
    match name {
        "FirstPipe" => WindowsFault::FirstPipe,
        "SecondPipe" => WindowsFault::SecondPipe,
        "CloseWorker" => WindowsFault::CloseWorker,
        "PseudoConsole" => WindowsFault::PseudoConsole,
        "DrainStart" => WindowsFault::DrainStart,
        "JobCreate" => WindowsFault::JobCreate,
        "JobLimits" => WindowsFault::JobLimits,
        "AttrInit" => WindowsFault::AttrInit,
        "PseudoConsoleAttr" => WindowsFault::PseudoConsoleAttr,
        "CreateProcess" => WindowsFault::CreateProcess,
        other => panic!("unknown fault {other}"),
    }
}

fn sleeper_spec() -> SpawnSpec {
    spec(
        os(&[BIN, "--mode", "sleeper", "--max-life-secs", "60"]),
        vec![("SystemRoot".into(), system_root())],
        PtySize::new(80, 24).unwrap(),
    )
}

#[test]
fn pre_process_faults_leave_no_handle() {
    if helper_case().is_some() {
        for name in PRE_PROCESS {
            handle_cycles(name, || {
                windows_inject_next(pre_fault(name));
                let r = Session::spawn(&*native(), &sleeper_spec(), SessionConfig::new(|_| {}));
                match r {
                    Err(SpawnError::NeverStarted { cleanup, .. }) => {
                        assert_eq!(cleanup, Cleanup::Confirmed, "{name}");
                    }
                    other => panic!("{name}: {other:?}"),
                }
                let rep = the_report();
                assert_eq!(
                    rep.entries("attr_init").len(),
                    rep.entries("attr_delete").len(),
                    "{name}: {rep:?}"
                );
                assert!(
                    pid_of_create(&rep).is_none(),
                    "{name}: a process was created"
                );
                for e in &rep.trace {
                    if !matches!(e.name, "create_pseudo_console" | "create_process") {
                        assert!(!e.lock_held, "{name}: {e:?}");
                    }
                }
            });
        }
        return;
    }
    let _l = lock();
    run_helper("pre_process_faults_leave_no_handle", "pre", false);
}

#[test]
fn after_process_faults_leave_no_handle() {
    if helper_case().is_some() {
        let cases: &[&str] = &[
            "Identity",
            "Assign",
            "ResumeFails",
            "AfterSpawn",
            "Writer",
            "Waiter",
        ];
        for name in cases {
            handle_cycles(name, || {
                let mut s = sleeper_spec();
                let mut config = SessionConfig::new(|_| {});
                let mut opener = None;
                match *name {
                    "Identity" => windows_inject_next(WindowsFault::Identity),
                    "Assign" => windows_inject_next(WindowsFault::Assign),
                    "ResumeFails" => windows_inject_next(WindowsFault::ResumeFails),
                    "AfterSpawn" => {
                        s.argv = os(&[BIN, "--mode", "attachable", "--max-life-secs", "60"]);
                        let (g, ctl) = gate();
                        windows_inject_next(WindowsFault::AfterSpawn {
                            gate: g,
                            marker: b"MARKER-0",
                        });
                        opener = Some(std::thread::spawn(move || {
                            assert!(ctl.wait_reached(WAIT), "AfterSpawn milestone");
                            ctl.open();
                        }));
                    }
                    "Writer" => config.start_fault = Some(StartFault::Writer),
                    "Waiter" => config.start_fault = Some(StartFault::Waiter),
                    _ => unreachable!(),
                }
                let r = Session::spawn(&*native(), &s, config);
                if let Some(o) = opener {
                    o.join().unwrap();
                }
                let cleanup = match r {
                    Err(e) => e.cleanup().clone(),
                    Ok(_) => panic!("{name}: spawn succeeded"),
                };
                assert_eq!(cleanup, Cleanup::Confirmed, "{name}");
                let rep = the_report();
                assert!(!rep.gate_abandoned, "{name}");
                let pid = pid_of_create(&rep).expect("a process was created");
                assert_eq!(identify(pid).unwrap(), None, "{name}: the leader is dead");
            });
        }
        return;
    }
    let _l = lock();
    run_helper("after_process_faults_leave_no_handle", "after", false);
}

// ---- CL-4 ----

#[test]
fn spawn_takes_the_job_list_branch() {
    let _l = lock();
    let r = rig(&["--mode", "sleeper"]);
    let ev = evidence(&r.session);
    assert_eq!(ev["job_branch"], "job_list");
    assert!(!ev.contains_key("job_list_error"), "{ev:?}");
    let want = Path::new(&system_root()).join(r"System32\kernel32.dll");
    assert!(
        ev["conpty_module"].eq_ignore_ascii_case(want.to_str().unwrap()),
        "{ev:?}"
    );
}

#[test]
fn child_is_in_the_job_from_creation() {
    let _l = lock();
    let listener = ReportListener::bind().unwrap();
    let s = fake_spec(
        &listener.arg(),
        &["--mode", "spawn-tree", "--roles", "plain"],
    );
    let (g, ctl) = gate();
    let spawner = std::thread::spawn(move || {
        windows_inject_next(WindowsFault::AfterCreate(g));
        let r = Session::spawn(&*native(), &s, SessionConfig::new(|_| {}));
        (r.map_err(|e| e.to_string()), windows_fault_report())
    });
    assert!(ctl.wait_reached(WAIT), "the gate after CreateProcessW");
    let mut cleanup = fixture_cleanup();
    let root = listener.accept(soon()).unwrap();
    cleanup.track(root.hello.pid);
    let got = listener.expect_roles(&["plain"], soon()).unwrap();
    let plain = got["plain"].hello.pid;
    cleanup.track(plain);
    // The role started before the backend's next step.
    ctl.open();
    let (session, rep) = spawner.join().unwrap();
    let session = session.unwrap();
    let rep = rep.unwrap();
    assert!(!rep.gate_abandoned);
    assert_eq!(
        rep.member_at_create,
        vec![Some(true)],
        "in the job at its create"
    );
    let leader = session.identity().pid;
    assert_eq!(leader, root.hello.pid);
    assert!(windows_in_job(leader, leader).unwrap());
    assert!(
        windows_in_job(leader, plain).unwrap(),
        "the role is in the job"
    );
}

// ---- CL-5 ----

#[test]
fn forced_fallback_runs_nothing_before_resume() {
    let _l = lock();
    let out = TempPath::new("fallback");
    let argv = os(&[BIN, "--mode", "report-process", "--out", &out.arg()]);
    let env: Vec<(OsString, OsString)> = vec![("K".into(), "v".into())];
    let s = spec(argv.clone(), env, PtySize::new(80, 24).unwrap());
    let (g, ctl) = gate();
    let spawner = std::thread::spawn(move || {
        windows_inject_next(WindowsFault::RejectJobList(Some(g)));
        let (config, rx) = events();
        let r = Session::spawn(&*native(), &s, config);
        (r.map_err(|e| e.to_string()), rx, windows_fault_report())
    });
    assert!(
        ctl.wait_reached(WAIT),
        "the gate after the suspended create"
    );
    assert!(!out.0.exists(), "the suspended child ran before its resume");
    ctl.open();
    let (session, rx, rep) = spawner.join().unwrap();
    let session = session.unwrap();
    let rep = rep.unwrap();
    assert!(!rep.gate_abandoned);
    assert_eq!(
        rep.member_at_create,
        vec![Some(false)],
        "assigned only later"
    );
    assert_eq!(wait_exit(&rx), 0);
    let ev = evidence(&session);
    assert_eq!(ev["job_branch"], "suspended");
    assert!(
        ev["job_list_error"].starts_with("create_process: "),
        "{ev:?}"
    );
    let _ = session.close(&[], CloseBounds::default());
    // The trace: the synthesized job-list failure, the native suspended create, a
    // successful assign, then a resume that returned 1.
    let order: Vec<_> = rep
        .trace
        .iter()
        .filter(|e| matches!(e.name, "create_process" | "assign" | "resume"))
        .map(|e| (e.name, e.native, e.flags & 0x4 != 0, e.result))
        .collect();
    assert_eq!(order.len(), 4, "{rep:?}");
    assert_eq!(
        (order[0].0, order[0].1, order[0].2),
        ("create_process", false, false)
    );
    assert!(order[0].3.is_err());
    assert_eq!(
        (order[1].0, order[1].1, order[1].2),
        ("create_process", true, true)
    );
    assert!(order[1].3.is_ok());
    assert_eq!(order[2], ("assign", true, false, Ok(0)));
    assert_eq!(order[3], ("resume", true, false, Ok(1)));
    let v = read_report(&out);
    let want: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(strings(&v["argv"]), want);
    assert_eq!(v["env"], serde_json::json!([["K", "v"]]));
}

const ROLES: &[&str] = &["plain", "double-forked", "breakaway"];

/// Every role answers and is in the session's job; a watch per role, and one on the leader.
/// A watch and what it watches.
type Watched = (String, Watch);

fn check_roles(r: &mut Rig) -> (BTreeMap<String, Fixture>, Vec<Watched>, Watched) {
    let mut got = r.roles(ROLES);
    let leader = r.session.identity().pid;
    let mut watches = Vec::new();
    for (role, f) in &mut got {
        assert!(f.ping(soon()).is_some(), "{role} pong");
        assert!(
            windows_in_job(leader, f.hello.pid).unwrap(),
            "{role} in the job"
        );
        watches.push((role.clone(), watch(f)));
    }
    (got, watches, ("spawn-tree".into(), watch(&r.root)))
}

/// The proof at a kill's return: the leader's (and each held extra's) handle is already
/// signalled, polled with a zero wait, and the job's count reads 0; members then signal,
/// observed through held watches with a watchdog only.
fn assert_tree_dead_at_return(leader: u32, held: &[Watched], members: &[Watched]) {
    for (role, w) in held {
        assert!(
            w.signalled(Duration::ZERO).unwrap(),
            "{role} alive at the kill's return"
        );
    }
    assert_eq!(
        windows_job_active(leader).unwrap(),
        0,
        "a job member at the kill's return"
    );
    for (role, w) in members {
        assert!(w.signalled(WAIT).unwrap(), "watchdog: {role} survived");
    }
}

#[test]
fn forced_fallback_takes_the_suspended_branch() {
    let _l = lock();
    windows_inject_next(WindowsFault::RejectJobList(None));
    let mut r = rig(&[
        "--mode",
        "spawn-tree",
        "--roles",
        "plain,double-forked,breakaway",
    ]);
    let mut other = outside(&mut r.cleanup, "unrelated");
    assert_eq!(evidence(&r.session)["job_branch"], "suspended");
    let (_got, watches, root) = check_roles(&mut r);
    let k = r.session.kill(&[], Instant::now() + KILL);
    assert!(k.tree_dead, "{k:?}");
    assert_tree_dead_at_return(r.session.identity().pid, &[root], &watches);
    assert!(
        other.ping(soon()).is_some(),
        "the unrelated process was killed"
    );
}

// ---- CL-7 ----

/// What the teardown's kill hook saw: the leader's watch polled, and the job's count.
#[derive(Default)]
struct AtKill {
    leader: Option<(u32, Watch)>,
    seen: Option<(bool, Option<u32>)>,
}

/// PR4-CL-7: a failure after the process exists, with a write completed while the drain's
/// reader is parked (output pending), kills the tree before the pseudoconsole closes.
#[test]
fn after_spawn_fault_with_output_pending_kills_the_tree() {
    let _l = lock();
    let listener = ReportListener::bind().unwrap();
    let s = fake_spec(
        &listener.arg(),
        &[
            "--mode",
            "spawn-tree",
            "--emit-output",
            "--roles",
            "plain,double-forked,breakaway",
        ],
    );
    let shared: Arc<Mutex<AtKill>> = Arc::default();
    let (g, ctl) = gate();
    let sh = shared.clone();
    let spawner = std::thread::spawn(move || {
        windows_on_teardown_kill(Box::new(move || {
            let mut st = sh.lock().unwrap();
            if let Some((pid, w)) = &st.leader {
                let dead = w.signalled(Duration::ZERO).unwrap_or(false);
                let active = windows_job_active(*pid).ok();
                st.seen = Some((dead, active));
            }
        }));
        windows_inject_next(WindowsFault::AfterSpawn {
            gate: g,
            marker: b"OUTPUT-0",
        });
        let r = Session::spawn(&*native(), &s, SessionConfig::new(|_| {}));
        let r = match r {
            Err(SpawnError::StartedThenFailed { stage, cleanup, .. }) => Ok((stage, cleanup)),
            Err(e) => Err(e.to_string()),
            Ok(_) => Err("spawn succeeded".into()),
        };
        (r, windows_fault_report())
    });
    let mut cleanup = fixture_cleanup();
    let mut root = listener.accept(soon()).unwrap();
    cleanup.track(root.hello.pid);
    let mut got = listener.expect_roles(ROLES, soon()).unwrap();
    let mut members = Vec::new();
    for (role, f) in &mut got {
        cleanup.track(f.hello.pid);
        assert!(f.ping(soon()).is_some(), "{role} pong");
        members.push((role.clone(), watch(f)));
    }
    let mut other = outside(&mut cleanup, "unrelated");
    root.send("emit").unwrap();
    assert_eq!(next_kind(&mut root, "output")["n"], 0);
    assert!(ctl.wait_reached(WAIT), "milestone: OUTPUT-0 seen");
    assert!(
        windows_output_parked(soon()),
        "the reader parked after OUTPUT-0"
    );
    // Written and flushed while the reader is parked: output is pending at the teardown.
    root.send("emit").unwrap();
    assert_eq!(next_kind(&mut root, "output")["n"], 1);
    shared.lock().unwrap().leader = Some((root.hello.pid, watch(&root)));
    ctl.open();
    let (r, rep) = spawner.join().unwrap();
    let (stage, cleanup_result) = r.unwrap();
    assert_eq!(stage, "after_spawn");
    assert_eq!(cleanup_result, Cleanup::Confirmed);
    let rep = rep.unwrap();
    assert!(!rep.gate_abandoned);
    assert_eq!(rep.marker_seen_running, Some(true));
    assert_eq!(
        rep.held_at_teardown_kill,
        Some(true),
        "the reader stayed parked through the kill"
    );
    let n = rep.names();
    let hook = n
        .iter()
        .position(|x| *x == "teardown_hook_ran")
        .expect("hook ran");
    let close = n.iter().rposition(|x| *x == "close").expect("close");
    let released = n.iter().position(|x| *x == "teardown_close").unwrap();
    assert!(hook < released && released < close, "{n:?}");
    let seen = shared.lock().unwrap().seen;
    assert_eq!(
        seen,
        Some((true, Some(0))),
        "at the kill: the leader signalled and the job empty"
    );
    for (role, w) in &members {
        assert!(w.signalled(WAIT).unwrap(), "watchdog: {role} survived");
    }
    assert!(
        other.ping(soon()).is_some(),
        "the unrelated process was killed"
    );
}

/// A spawn failing after the process exists: the leader (from the trace) is dead when
/// `spawn` returns.
fn process_fault(fault: Option<WindowsFault>, start: Option<StartFault>, stage: &str) {
    let _l = lock();
    let listener = ReportListener::bind().unwrap();
    let s = fake_spec(
        &listener.arg(),
        &["--mode", "spawn-tree", "--roles", "plain"],
    );
    if let Some(f) = fault {
        windows_inject_next(f);
    }
    let mut config = SessionConfig::new(|_| {});
    config.start_fault = start;
    let r = Session::spawn(&*native(), &s, config);
    match r {
        Err(SpawnError::StartedThenFailed {
            stage: st, cleanup, ..
        }) => {
            assert_eq!(st, stage);
            assert_eq!(cleanup, Cleanup::Confirmed);
        }
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("spawn succeeded"),
    }
    let rep = the_report();
    let pid = pid_of_create(&rep).expect("a process was created");
    assert_eq!(identify(pid).unwrap(), None, "the leader survived");
    let n = rep.names();
    let kill = n.iter().position(|x| *x == "terminate_job").expect("kill");
    let input = n.iter().position(|x| *x == "input_drop").expect("input");
    assert!(kill < input, "{n:?}");
}

#[test]
fn identity_fault_kills_the_tree() {
    process_fault(Some(WindowsFault::Identity), None, "identity");
}

#[test]
fn writer_start_fault_kills_the_real_tree() {
    process_fault(None, Some(StartFault::Writer), "writer_thread");
}

#[test]
fn waiter_start_fault_kills_the_real_tree() {
    process_fault(None, Some(StartFault::Waiter), "waiter_thread");
}

// ---- CL-8 ----

#[test]
fn report_shows_the_exact_env() {
    let _l = lock();
    let (v, _) = report_process(
        &[],
        vec![("b".into(), "2".into()), ("A".into(), "1".into())],
        None,
        None,
    );
    assert_eq!(v["env"], serde_json::json!([["A", "1"], ["b", "2"]]));
}

#[test]
fn empty_env_reaches_the_cli() {
    let _l = lock();
    let (v, _) = report_process(&[], Vec::new(), None, None);
    assert_eq!(v["env"], serde_json::json!([]));
}

// ---- CL-9 ----

#[test]
fn argv_reaches_the_cli_intact() {
    let _l = lock();
    let args = ["a b", "", "x\"y", r"C:\dir\", "\\\\\"", "日本語", "a\tb"];
    for fault in [None, Some(WindowsFault::RejectJobList(None))] {
        let label = format!("{fault:?}");
        let (v, want) = report_process(&args, Vec::new(), fault, None);
        assert_eq!(strings(&v["argv"]), want, "{label}");
    }
}

// ---- CL-10 ----

#[test]
fn cwd_is_honored() {
    let _l = lock();
    let canonical = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let text = canonical.to_str().unwrap().to_owned();
    assert!(text.starts_with(r"\\?\"), "{text}");
    let (v, _) = report_process(&[], Vec::new(), None, Some(canonical));
    assert_eq!(v["cwd"], text.trim_start_matches(r"\\?\"));
}

// ---- CL-11 ----

#[test]
fn size_reaches_the_cli() {
    let _l = lock();
    let listener = ReportListener::bind().unwrap();
    let mut s = fake_spec(&listener.arg(), &["--mode", "attachable"]);
    s.size = PtySize::new(100, 30).unwrap();
    let mut r = rig_spec(listener, &s);
    let size = next_kind(&mut r.root, "size");
    assert_eq!(
        (
            &size["cols"],
            &size["rows"],
            &size["xpixel"],
            &size["ypixel"]
        ),
        (
            &100.into(),
            &30.into(),
            &serde_json::Value::Null,
            &serde_json::Value::Null
        ),
        "{size}"
    );
    let _resized = r.session.resize(PtySize::new(120, 40).unwrap()).unwrap();
    let winch = next_kind(&mut r.root, "winch");
    assert_eq!(
        (&winch["cols"], &winch["rows"]),
        (&120.into(), &40.into()),
        "{winch}"
    );
}

// ---- CL-12 ----

#[test]
fn child_stdio_is_the_pseudoconsole() {
    if helper_case().is_some() {
        // The helper's own three standard handles are pipes.
        use std::io::IsTerminal;
        assert!(!std::io::stdin().is_terminal() && !std::io::stdout().is_terminal());
        let (v, _) = report_process(&[], Vec::new(), None, None);
        assert_eq!(
            (
                &v["stdin_console"],
                &v["stdout_console"],
                &v["stderr_console"]
            ),
            (&true.into(), &true.into(), &true.into()),
            "{v}"
        );
        return;
    }
    let _l = lock();
    run_helper("child_stdio_is_the_pseudoconsole", "stdio", true);
}

// ---- CL-14 ----

#[test]
fn input_reaches_the_cli() {
    let _l = lock();
    let mut r = rig(&["--mode", "echo"]);
    assert!(r.root.hello.raw, "raw mode under ConPTY");
    let (done, rx) = completion_channel();
    r.session
        .submit(
            WriteUnit::bytes(WriteOrigin::Key, b"hello".to_vec()),
            Guard::until(soon()),
            done,
        )
        .unwrap();
    let mut hex = String::new();
    while hex.len() < 10 {
        hex.push_str(next_kind(&mut r.root, "input")["hex"].as_str().unwrap());
    }
    assert_eq!(hex, "68656c6c6f");
    assert_eq!(
        rx.recv_deadline(soon()).unwrap().outcome,
        WriteOutcome::Written
    );
}

#[test]
fn large_write_is_accepted_whole() {
    let _l = lock();
    let mut r = rig(&["--mode", "echo", "--raw", "--validate-input", "65536"]);
    let payload: Vec<u8> = (0..65_536u32)
        .map(|i| b'a' + u8::try_from(i % 26).unwrap())
        .collect();
    let (done, rx) = completion_channel();
    r.session
        .submit(
            WriteUnit::bytes(WriteOrigin::Key, payload),
            Guard::until(soon()),
            done,
        )
        .unwrap();
    let rec = rx.recv_deadline(soon()).unwrap();
    assert_eq!(rec.outcome, WriteOutcome::Written);
    assert_eq!(rec.bytes_written, 65_536);
    assert_eq!(rec.split_writes, 0);
    let v = next_kind(&mut r.root, "validated");
    assert_eq!(
        (&v["bytes"], &v["ok"]),
        (&65_536.into(), &true.into()),
        "{v}"
    );
}

// ---- CL-16 ----

#[test]
fn spawn_tree_dies_and_unrelated_survives() {
    let _l = lock();
    let mut r = rig(&[
        "--mode",
        "spawn-tree",
        "--roles",
        "plain,double-forked,breakaway",
    ]);
    let mut other = outside(&mut r.cleanup, "unrelated");
    let breakaway = next_kind(&mut r.root, "breakaway");
    assert_eq!(
        breakaway["result"], "denied",
        "a member's breakaway was refused"
    );
    let (_got, watches, root) = check_roles(&mut r);
    let k = r.session.kill(&[], Instant::now() + KILL);
    assert!(k.tree_dead, "{k:?}");
    assert_eq!(k.method, "windows_job");
    assert!(k.survivors.is_empty());
    assert_tree_dead_at_return(r.session.identity().pid, &[root], &watches);
    assert!(
        other.ping(soon()).is_some(),
        "the unrelated process was killed"
    );
}

// ---- CL-18 ----

#[test]
fn extra_outside_the_job_dies_and_stale_extra_is_spared() {
    let _l = lock();
    let mut r = rig(&["--mode", "sleeper"]);
    let mut other = outside(&mut r.cleanup, "unrelated");
    let mut sleeper = outside(&mut r.cleanup, "plain");
    let k1 = r.session.kill(&[], Instant::now() + KILL);
    assert!(k1.tree_dead, "{k1:?}");
    // Outside the job and not named: outside the guarantee.
    assert!(sleeper.ping(soon()).is_some());
    let sleeper_id = id_of(&sleeper);
    let sleeper_watch = windows_watch(&sleeper_id).unwrap();
    let mut stale = id_of(&other);
    stale.start_time = (stale.start_time.parse::<u64>().unwrap() + 1).to_string();
    let k2 = r.session.kill(&[sleeper_id, stale], Instant::now() + KILL);
    assert!(k2.tree_dead, "{k2:?}");
    assert_tree_dead_at_return(
        r.session.identity().pid,
        &[("the named extra".into(), sleeper_watch)],
        &[],
    );
    assert!(
        other.ping(soon()).is_some(),
        "the stale identity was killed"
    );
}

// ---- CL-20 ----

/// PR4-CL-20: `close` with output pending (a write completed while the drain's reader is
/// parked) returns within its budgets, its close operation on time, output ending in EOF.
#[test]
fn close_with_output_flowing_is_bounded() {
    let _l = lock();
    windows_inject_next(WindowsFault::HoldOutput {
        marker: b"OUTPUT-0",
    });
    let mut r = rig(&["--mode", "spawn-tree", "--roles", "none", "--emit-output"]);
    r.root.send("emit").unwrap();
    assert_eq!(next_kind(&mut r.root, "output")["n"], 0);
    assert!(
        windows_output_parked(soon()),
        "the reader parked after OUTPUT-0"
    );
    r.root.send("emit").unwrap();
    assert_eq!(next_kind(&mut r.root, "output")["n"], 1);
    let bounds = CloseBounds::default();
    let t = Instant::now();
    let out = r.session.close(&[], bounds);
    let budget = bounds.kill + bounds.input + bounds.close + bounds.drain + bounds.join;
    assert!(t.elapsed() < budget, "{:?}", t.elapsed());
    assert_eq!(
        windows_output_parked_at_close(),
        Some(true),
        "output was pending as the close began"
    );
    assert!(out.close.elapsed.is_some(), "{out:?}");
    assert!(out.released.control, "{out:?}");
    assert_eq!(out.output, Some(OutputEnd::Eof), "{out:?}");
    assert!(out.abandoned.is_empty(), "{out:?}");
}

// ---- CL-10: unpaired surrogates ----

/// A directory removed on drop.
struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A verbatim cwd whose last unit is an unpaired surrogate names that directory, never its
/// lossy twin (`U+FFFD`): the relative output lands there.
#[test]
fn verbatim_cwd_keeps_unpaired_surrogates() {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let _l = lock();
    let base = std::env::temp_dir().join(format!("chaldea-ptywin-cwd-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let guard = DirGuard(base.clone());
    let base = std::fs::canonicalize(&base).unwrap();
    assert!(base.to_str().unwrap().starts_with(r"\\?\"));
    let named = |last: u16| {
        let mut w: Vec<u16> = base.as_os_str().encode_wide().collect();
        w.extend(r"\a".encode_utf16());
        w.push(last);
        PathBuf::from(OsString::from_wide(&w))
    };
    let (lone, twin) = (named(0xD800), named(0xFFFD));
    std::fs::create_dir(&lone).unwrap();
    std::fs::create_dir(&twin).unwrap();
    let mut s = spec(
        os(&[BIN, "--mode", "report-process", "--out", "marker.json"]),
        Vec::new(),
        PtySize::new(80, 24).unwrap(),
    );
    s.cwd = lone.clone();
    let (config, rx) = events();
    let session = Session::spawn(&*native(), &s, config).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(wait_exit(&rx), 0);
    let _ = session.close(&[], CloseBounds::default());
    assert!(
        lone.join("marker.json").exists(),
        "the marker is in the named directory"
    );
    assert!(
        !twin.join("marker.json").exists(),
        "the lossy twin was used"
    );
    drop(guard);
}
