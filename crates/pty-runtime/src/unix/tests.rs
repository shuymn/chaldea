//! Real-process unit tests of the spawn stages, exit, kill, and ownership loss, with
//! system binaries and stage hooks (`docs/design/pty-unix.md` §7).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, WaitOptions};

use super::UnixPty;
use super::child::{UnixChild, waitid_nowait};
use super::sys::PreExecTest;
use super::table::{self, Lookup};
use super::trace;
use crate::backend::{
    Cleanup, DrainSink, NeverStartedKind, PtyChild, PtyRuntime, PtySize, SpawnError, SpawnSpec,
    SpawnedParts,
};
use crate::drain::DrainEvent;
use crate::session::OutputEnd;

static SERIAL: Mutex<()> = Mutex::new(());

/// Serializes the real-process and real-PTY unit tests of `src/unix/` among themselves.
pub(crate) fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The milestones a test hook can act on.
#[derive(Debug)]
pub(crate) enum Stage {
    /// Validation passed; nothing is created yet.
    Validated,
    /// Stage 2: inside the spawn lock, before the wake pipe.
    WakeUnderLock,
    /// Stage 4: the lock is held, before `Command::spawn`.
    LockAcquired,
    /// Stage 5, before the identity read; the hook returning true fails the read.
    Spawned { pid: u32 },
}

type StageFn = Box<dyn Fn(&Stage) -> bool + Send + Sync>;

/// Test-only behaviour of one `UnixPty`.
#[derive(Default)]
pub(crate) struct Hooks {
    pub(crate) stage: Option<StageFn>,
    pub(crate) fail_drain_start: bool,
    pub(crate) stdio_dev_null: bool,
    pub(crate) pre_exec: PreExecTest,
    pub(crate) kill_expired: bool,
    /// The last child built, kept alive (its `Drop` runs when the test takes it out).
    pub(crate) child: Mutex<Option<Arc<UnixChild>>>,
}

impl Hooks {
    /// Runs the stage hook; true asks the stage to fail.
    pub(crate) fn on(&self, stage: &Stage) -> bool {
        self.stage.as_ref().is_some_and(|f| f(stage))
    }
}

const BOUND: Duration = Duration::from_secs(10);

/// What the drain delivered.
#[derive(Default)]
pub(crate) struct Out {
    st: Mutex<(Vec<u8>, Option<OutputEnd>)>,
    cv: Condvar,
}

impl Out {
    pub(crate) fn sink(self: &Arc<Self>) -> DrainSink {
        let out = self.clone();
        DrainSink::test(move |ev| {
            let mut st = out.st.lock().unwrap();
            match ev {
                DrainEvent::Bytes { bytes, .. } => st.0.extend_from_slice(bytes),
                DrainEvent::End(end) => st.1 = Some(end.clone()),
            }
            drop(st);
            out.cv.notify_all();
        })
    }

    /// Waits until `f` holds on the bytes and end so far.
    pub(crate) fn wait(&self, f: impl Fn(&[u8], Option<&OutputEnd>) -> bool) -> bool {
        let end = Instant::now() + BOUND;
        let mut st = self.st.lock().unwrap();
        loop {
            if f(&st.0, st.1.as_ref()) {
                return true;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self.cv.wait_timeout(st, left).unwrap().0;
        }
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.st.lock().unwrap().0).into_owned()
    }

    fn end(&self) -> Option<OutputEnd> {
        self.st.lock().unwrap().1.clone()
    }
}

fn spec(argv: &[&str], cwd: &Path) -> SpawnSpec {
    SpawnSpec {
        argv: argv.iter().map(OsString::from).collect(),
        cwd: cwd.to_path_buf(),
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        size: PtySize::new(80, 24).unwrap(),
    }
}

/// A fresh directory in the OS temp dir, removed on drop.
pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    pub(crate) fn new(tag: &str) -> TempDir {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("chaldea-unix-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p.canonicalize().unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spawn(pty: &UnixPty, spec: &SpawnSpec) -> (Result<SpawnedParts, SpawnError>, Arc<Out>) {
    let out = Arc::new(Out::default());
    let r = pty
        .spawn(spec, out.sink())
        .map(crate::backend::SpawnedPty::into_parts);
    (r, out)
}

fn take_child(pty: &UnixPty) -> Arc<UnixChild> {
    pty.hooks.child.lock().unwrap().take().unwrap()
}

fn pid(p: u32) -> Pid {
    Pid::from_raw(i32::try_from(p).unwrap()).unwrap()
}

fn is_live(p: u32) -> bool {
    matches!(table::lookup(p), Lookup::Live(_))
}

/// Waits until `p` is no longer live.
fn wait_dead(p: u32) -> bool {
    let end = Instant::now() + BOUND;
    while is_live(p) {
        if Instant::now() >= end {
            return false;
        }
        std::thread::yield_now();
    }
    true
}

/// SIGKILL to `p` only if it still has the start `start`.
fn kill_verified(p: u32, start: u64) {
    if let Lookup::Live(proc) = table::lookup(p)
        && proc.start == start
    {
        let _ = rustix::process::kill_process(pid(p), Signal::KILL);
    }
}

/// Reaps our own child `p` by exact pid, within the bound.
fn reap(p: u32) -> bool {
    let end = Instant::now() + BOUND;
    loop {
        match rustix::process::waitpid(Some(pid(p)), WaitOptions::NOHANG) {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(_) => return false,
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::yield_now();
    }
}

fn start_of(p: u32) -> u64 {
    match table::lookup(p) {
        Lookup::Live(proc) => proc.start,
        Lookup::Zombie(s) => s,
        other => panic!("no process {p}: {other:?}"),
    }
}

/// A leader that ignores HUP and TERM (dispositions survive `exec`).
const STUBBORN: &[&str] = &[
    "/bin/sh",
    "-c",
    "trap '' HUP TERM; echo READY; exec /bin/sleep 30",
];

fn ready(out: &Out) {
    assert!(
        out.wait(|b, _| b.windows(5).any(|w| w == b"READY")),
        "{}",
        out.text()
    );
}

// ---- CL-2 ----

#[test]
fn cwd_removed_after_validation_is_never_started() {
    let _s = serial();
    let cwd = TempDir::new("cwd");
    let marks = TempDir::new("mark");
    let marker = marks.0.join("ran");
    let gone = cwd.0.clone();
    let pty = UnixPty {
        hooks: Hooks {
            stage: Some(Box::new(move |s| {
                if matches!(s, Stage::Validated) {
                    std::fs::remove_dir(&gone).unwrap();
                }
                false
            })),
            ..Hooks::default()
        },
    };
    let (r, _) = spawn(
        &pty,
        &spec(&["/usr/bin/touch", marker.to_str().unwrap()], &cwd.0),
    );
    match r {
        Err(SpawnError::NeverStarted {
            kind,
            error,
            cleanup,
        }) => {
            assert_eq!(kind, NeverStartedKind::Setup);
            assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
            assert_eq!(cleanup, Cleanup::Confirmed);
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
    assert!(!marker.exists());
}

#[test]
fn pre_exec_failure_is_never_started() {
    let _s = serial();
    let dir = TempDir::new("pre");
    let pty = UnixPty {
        hooks: Hooks {
            stdio_dev_null: true,
            ..Hooks::default()
        },
    };
    let (r, _) = spawn(&pty, &spec(&["/usr/bin/true"], &dir.0));
    match r {
        Err(SpawnError::NeverStarted {
            kind,
            error,
            cleanup,
        }) => {
            assert_eq!(kind, NeverStartedKind::Setup);
            // macOS reports `ENODEV` for `TIOCSCTTY` on a non-terminal device.
            assert_eq!(error.raw_os_error(), Some(libc::ENODEV));
            assert_eq!(cleanup, Cleanup::Confirmed);
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
}

// ---- CL-3 ----

#[test]
fn drain_start_failure_releases_everything() {
    let _s = serial();
    let dir = TempDir::new("drain");
    let reached = Arc::new(Mutex::new(Vec::new()));
    let r2 = reached.clone();
    let pty = UnixPty {
        hooks: Hooks {
            fail_drain_start: true,
            stage: Some(Box::new(move |s| {
                r2.lock().unwrap().push(format!("{s:?}"));
                false
            })),
            ..Hooks::default()
        },
    };
    let before = super::sys::child_pids();
    let (r, _) = spawn(&pty, &spec(&["/bin/sleep", "30"], &dir.0));
    match r {
        Err(SpawnError::NeverStarted { kind, cleanup, .. }) => {
            assert_eq!(kind, NeverStartedKind::Setup);
            assert_eq!(cleanup, Cleanup::Confirmed);
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
    assert_eq!(*reached.lock().unwrap(), vec!["Validated", "WakeUnderLock"]);
    assert_eq!(super::sys::child_pids(), before);
}

/// `sh` with a background `sleep` and itself exec'd into `sleep`; the stage 5 hook waits
/// until the drain has seen the background pid and both are live, then fails the read.
fn identity_failure(kill_expired: bool) -> (SpawnError, u32, u32, Vec<&'static str>, UnixPty) {
    let dir = TempDir::new("ident");
    let out = Arc::new(Out::default());
    let seen = Arc::new(Mutex::new((0u32, 0u32)));
    let (o2, s2) = (out.clone(), seen.clone());
    let pty = UnixPty {
        hooks: Hooks {
            kill_expired,
            stage: Some(Box::new(move |s| {
                let Stage::Spawned { pid } = s else {
                    return false;
                };
                assert!(o2.wait(|b, _| b.contains(&b'\n')), "{}", o2.text());
                let bg: u32 = o2.text().trim().parse().unwrap();
                assert!(is_live(*pid) && is_live(bg));
                *s2.lock().unwrap() = (*pid, bg);
                true
            })),
            ..Hooks::default()
        },
    };
    trace::start();
    let r = pty.spawn(
        &spec(
            &[
                "/bin/sh",
                "-c",
                "/bin/sleep 30 & echo $!; exec /bin/sleep 30",
            ],
            &dir.0,
        ),
        out.sink(),
    );
    let order = trace::take();
    let (leader, bg) = *seen.lock().unwrap();
    (r.err().unwrap(), leader, bg, order, pty)
}

#[test]
fn identity_failure_kills_the_started_tree() {
    let _s = serial();
    let (e, leader, bg, order, pty) = identity_failure(false);
    match e {
        SpawnError::StartedThenFailed {
            stage,
            identity,
            cleanup,
            ..
        } => {
            assert_eq!(stage, "identity");
            assert_eq!(identity, None);
            assert_eq!(cleanup, Cleanup::Confirmed);
        }
        SpawnError::NeverStarted { .. } => panic!("{e}"),
    }
    assert_eq!(
        order,
        ["kill_tree", "input_drop", "control_close", "drain_finish"]
    );
    assert!(wait_dead(leader) && wait_dead(bg));
    drop(take_child(&pty));
    assert_eq!(table::lookup(leader), Lookup::Gone);
}

#[test]
fn zero_kill_budget_reports_unresolved() {
    let _s = serial();
    let (e, leader, bg, _, pty) = identity_failure(true);
    let (ls, bs) = (start_of(leader), start_of(bg));
    match e {
        SpawnError::StartedThenFailed { cleanup, .. } => match cleanup {
            Cleanup::Unresolved { kill: Some(k), .. } => assert!(!k.tree_dead),
            Cleanup::Unresolved { kill: None, .. } | Cleanup::Confirmed => {
                panic!("{cleanup:?}")
            }
        },
        SpawnError::NeverStarted { .. } => panic!("{e}"),
    }
    assert!(is_live(leader) && is_live(bg));
    kill_verified(bg, bs);
    kill_verified(leader, ls);
    drop(take_child(&pty));
    assert!(reap(leader));
    assert!(wait_dead(bg));
}

// ---- CL-4, CL-6, CL-7 ----

#[test]
fn pty_opens_at_spec_size() {
    let _s = serial();
    let dir = TempDir::new("size");
    let mut sp = spec(&["/bin/stty", "size"], &dir.0);
    sp.size = PtySize::new(100, 40).unwrap();
    let (r, out) = spawn(&UnixPty::default(), &sp);
    let parts = r.unwrap();
    assert!(out.wait(|_, e| e.is_some()), "{}", out.text());
    assert_eq!(out.text().trim(), "40 100");
    let _ = parts.teardown();
}

#[test]
fn cwd_is_honored() {
    let _s = serial();
    let dir = TempDir::new("pwd");
    let (r, out) = spawn(&UnixPty::default(), &spec(&["/bin/pwd"], &dir.0));
    let parts = r.unwrap();
    assert!(out.wait(|_, e| e.is_some()));
    assert_eq!(out.text().trim(), dir.0.to_str().unwrap());
    let _ = parts.teardown();
}

#[test]
fn exit_ends_output_with_eof() {
    let _s = serial();
    let dir = TempDir::new("eof");
    let (r, out) = spawn(&UnixPty::default(), &spec(&["/bin/echo", "READY"], &dir.0));
    let parts = r.unwrap();
    assert!(out.wait(|_, e| e.is_some()));
    assert_eq!(out.text().trim(), "READY");
    assert_eq!(out.end(), Some(OutputEnd::Eof));
    let _ = parts.teardown();
}

// ---- CL-11 ----

#[test]
fn wait_timeout_returns_by_its_timeout() {
    let _s = serial();
    let dir = TempDir::new("wait");
    let (r, _) = spawn(&UnixPty::default(), &spec(&["/bin/sleep", "30"], &dir.0));
    let parts = r.unwrap();
    for ms in [0, 10, 100] {
        let t = Instant::now();
        assert_eq!(
            parts.child.wait_timeout(Duration::from_millis(ms)).unwrap(),
            None
        );
        assert!(t.elapsed() < Duration::from_millis(ms + 500));
    }
    assert!(parts.teardown() == Cleanup::Confirmed);
}

#[test]
fn wait_timeout_runs_during_kill() {
    let _s = serial();
    let dir = TempDir::new("waitkill");
    let pty = UnixPty::default();
    let (r, out) = spawn(&pty, &spec(STUBBORN, &dir.0));
    let parts = r.unwrap();
    ready(&out);
    let child = take_child(&pty);
    let k = child.clone();
    let killer =
        std::thread::spawn(move || k.kill_tree(&[], Instant::now() + Duration::from_secs(5)));
    // The kill holds its lock through the HUP and TERM grace waits.
    let end = Instant::now() + BOUND;
    while !child.kill_lock_held() {
        assert!(Instant::now() < end);
        std::thread::yield_now();
    }
    let t = Instant::now();
    let w = parts.child.wait_timeout(Duration::from_millis(10)).unwrap();
    assert!(
        t.elapsed() < Duration::from_millis(200),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(w, None);
    let report = killer.join().unwrap();
    assert!(report.tree_dead, "{report:?}");
    drop(child);
    let _ = parts.teardown();
}

// ---- CL-12 ----

#[test]
fn exit_reports_code_and_signal() {
    let _s = serial();
    let dir = TempDir::new("code");
    for (argv, code, signal) in [
        (&["/bin/sh", "-c", "exit 7"][..], Some(7), None),
        (&["/bin/sh", "-c", "kill -9 $$"][..], None, Some(9)),
    ] {
        let (r, _) = spawn(&UnixPty::default(), &spec(argv, &dir.0));
        let parts = r.unwrap();
        let a = parts.child.wait_timeout(BOUND).unwrap().unwrap();
        let b = parts.child.wait_timeout(Duration::ZERO).unwrap().unwrap();
        assert_eq!((a.code, a.signal), (code, signal));
        assert_eq!(a, b);
        let _ = parts.teardown();
    }
}

#[test]
fn exit_is_observed_without_reaping() {
    let _s = serial();
    let dir = TempDir::new("zombie");
    let pty = UnixPty::default();
    let (r, _) = spawn(&pty, &spec(&["/usr/bin/true"], &dir.0));
    let parts = r.unwrap();
    let leader = parts.child.identity().pid;
    assert!(parts.child.wait_timeout(BOUND).unwrap().is_some());
    assert!(matches!(table::lookup(leader), Lookup::Zombie(_)));
    let _ = parts.teardown();
    assert!(
        matches!(table::lookup(leader), Lookup::Zombie(_)),
        "the hook still holds it"
    );
    drop(take_child(&pty));
    assert_eq!(table::lookup(leader), Lookup::Gone);
}

#[test]
fn live_leader_is_left_unreaped_at_drop() {
    let _s = serial();
    let dir = TempDir::new("live");
    let pty = UnixPty {
        hooks: Hooks {
            kill_expired: true,
            ..Hooks::default()
        },
    };
    let (r, out) = spawn(&pty, &spec(STUBBORN, &dir.0));
    let parts = r.unwrap();
    ready(&out);
    let leader = parts.child.identity().pid;
    let start = start_of(leader);
    assert!(matches!(parts.teardown(), Cleanup::Unresolved { .. }));
    trace::start();
    drop(take_child(&pty));
    assert_eq!(trace::take(), ["reap"]);
    assert!(is_live(leader));
    kill_verified(leader, start);
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(500) {
        assert!(!is_live(leader) || t.elapsed() < BOUND);
        if !is_live(leader) {
            assert!(matches!(table::lookup(leader), Lookup::Zombie(_)));
        }
        std::thread::yield_now();
    }
    assert!(matches!(table::lookup(leader), Lookup::Zombie(_)));
    assert!(reap(leader));
}

// ---- CL-16 ----

#[test]
fn loss_before_observation() {
    let _s = serial();
    let dir = TempDir::new("loss1");
    let pty = UnixPty::default();
    let (r, _) = spawn(&pty, &spec(&["/usr/bin/true"], &dir.0));
    let parts = r.unwrap();
    let leader = parts.child.identity().pid;
    // An outside reaper takes the leader before the backend observed its exit.
    assert!(reap(leader));
    let e = parts
        .child
        .wait_timeout(Duration::from_millis(10))
        .unwrap_err();
    assert!(e.to_string().contains("reaped outside"), "{e}");
    let k = parts
        .child
        .kill_tree(&[], Instant::now() + Duration::from_secs(5));
    assert_eq!(k.method, "unix_ownership_lost");
    assert!(!k.tree_dead);
    let _ = parts.teardown();
    trace::start();
    drop(take_child(&pty));
    assert!(trace::take().is_empty());
}

#[test]
fn loss_after_cached_exit() {
    let _s = serial();
    let dir = TempDir::new("loss2");
    let pty = UnixPty::default();
    let (r, _) = spawn(&pty, &spec(&["/usr/bin/true"], &dir.0));
    let parts = r.unwrap();
    let leader = parts.child.identity().pid;
    assert!(parts.child.wait_timeout(BOUND).unwrap().is_some());
    assert!(reap(leader));
    let k = parts
        .child
        .kill_tree(&[], Instant::now() + Duration::from_secs(5));
    assert_eq!(k.method, "unix_ownership_lost");
    assert!(!k.tree_dead);
    let _ = parts.teardown();
    trace::start();
    drop(take_child(&pty));
    assert!(trace::take().is_empty(), "Drop called waitpid after loss");
}

// ---- CL-19 ----

#[test]
fn immediate_exit_still_has_an_identity() {
    let _s = serial();
    let dir = TempDir::new("quick");
    let pty = UnixPty {
        hooks: Hooks {
            stage: Some(Box::new(|s| {
                if let Stage::Spawned { pid } = s {
                    let end = Instant::now() + BOUND;
                    while waitid_nowait(*pid).unwrap().is_none() {
                        assert!(Instant::now() < end);
                        std::thread::yield_now();
                    }
                }
                false
            })),
            ..Hooks::default()
        },
    };
    let (r, _) = spawn(&pty, &spec(&["/usr/bin/true"], &dir.0));
    let parts = r.unwrap();
    let id = parts.child.identity().clone();
    assert!(!id.start_time.is_empty());
    assert_eq!(id.group, id.pid.to_string());
    let e = parts.child.wait_timeout(Duration::ZERO).unwrap().unwrap();
    assert_eq!((e.code, e.signal), (Some(0), None));
    let _ = parts.teardown();
}

#[test]
fn leader_identity_matches_lookup() {
    let _s = serial();
    let dir = TempDir::new("ident2");
    let (r, _) = spawn(&UnixPty::default(), &spec(&["/bin/sleep", "30"], &dir.0));
    let parts = r.unwrap();
    let id = parts.child.identity().clone();
    match table::lookup(id.pid) {
        Lookup::Live(p) => {
            assert_eq!(id.start_time, p.start.to_string());
            assert_eq!(id.group, p.pgid.to_string());
            assert_eq!(p.pgid, id.pid);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(parts.teardown(), Cleanup::Confirmed);
}

#[test]
fn identity_without_a_record_has_an_empty_start() {
    let _s = serial();
    let dir = TempDir::new("norecord");
    let pty = UnixPty {
        hooks: Hooks {
            stage: Some(Box::new(|s| matches!(s, Stage::Spawned { .. }))),
            ..Hooks::default()
        },
    };
    let (r, _) = spawn(&pty, &spec(&["/bin/sleep", "30"], &dir.0));
    assert!(matches!(
        r,
        Err(SpawnError::StartedThenFailed { identity: None, .. })
    ));
    let child = take_child(&pty);
    let id = child.identity();
    assert_eq!(id.group, id.pid.to_string());
    assert_eq!(id.start_time, "");
    assert!(wait_dead(id.pid));
}

#[test]
fn vanished_symlink_target_is_not_found() {
    let _s = serial();
    let dir = TempDir::new("link");
    let outer = dir.0.join("agent");
    let inner = dir.0.join("inner");
    std::os::unix::fs::symlink("/usr/bin/true", &inner).unwrap();
    std::os::unix::fs::symlink(&inner, &outer).unwrap();
    let gone = inner.clone();
    let pty = UnixPty {
        hooks: Hooks {
            stage: Some(Box::new(move |s| {
                if matches!(s, Stage::Validated) {
                    std::fs::remove_file(&gone).unwrap();
                }
                false
            })),
            ..Hooks::default()
        },
    };
    let (r, _) = spawn(&pty, &spec(&[outer.to_str().unwrap()], &dir.0));
    match r {
        Err(SpawnError::NeverStarted {
            kind,
            error,
            cleanup,
        }) => {
            assert_eq!(kind, NeverStartedKind::NotFound, "{error}");
            assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
            assert_eq!(cleanup, Cleanup::Confirmed);
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
}
