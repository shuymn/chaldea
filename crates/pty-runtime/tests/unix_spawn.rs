//! Public-API spawn tests of the macOS backend (`docs/design/pty-unix.md` §7): validation
//! failures, failures in the forked child, fd and child accounting, and the environment.
//! Every test holds `LOCK`, so no other thread of this process opens fds or spawns children
//! while one counts them.

#![cfg(target_os = "macos")]

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chaldea_pty_runtime::{
    Cleanup, CloseBounds, NeverStartedKind, PtySize, Session, SessionConfig, SessionEvent,
    SpawnError, SpawnSpec, native, with_spawn_lock,
};

static LOCK: Mutex<()> = Mutex::new(());

const WAIT: Duration = Duration::from_secs(10);

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A fresh directory in the OS temp dir, removed (permissions restored) on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!("chaldea-spawn-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p.canonicalize().unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        for e in std::fs::read_dir(&self.0).into_iter().flatten().flatten() {
            let _ = std::fs::set_permissions(e.path(), std::fs::Permissions::from_mode(0o755));
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spec(argv: &[&str], cwd: &Path) -> SpawnSpec {
    SpawnSpec {
        argv: argv.iter().map(OsString::from).collect(),
        cwd: cwd.to_path_buf(),
        env: Vec::new(),
        size: PtySize::new(80, 24).unwrap(),
    }
}

#[allow(
    clippy::result_large_err,
    reason = "the error carries its cleanup evidence by value"
)]
fn spawn(spec: &SpawnSpec) -> Result<Session, SpawnError> {
    Session::spawn(&*native(), spec, SessionConfig::new(|_| {}))
}

fn open_fds() -> BTreeSet<String> {
    std::fs::read_dir("/dev/fd")
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

/// The pids of this process's children, by `pgrep -P` (which leaves itself out).
fn children() -> String {
    let me = std::process::id().to_string();
    let out =
        with_spawn_lock(|| Command::new("/usr/bin/pgrep").args(["-P", &me]).output()).unwrap();
    String::from_utf8(out.stdout).unwrap()
}

/// Asserts a `NeverStarted` of `kind` with a confirmed cleanup; returns its error.
fn never_started(r: Result<Session, SpawnError>, kind: NeverStartedKind) -> std::io::Error {
    match r {
        Err(SpawnError::NeverStarted {
            kind: k,
            error,
            cleanup,
        }) => {
            assert_eq!(k, kind, "{error}");
            assert_eq!(cleanup, Cleanup::Confirmed);
            error
        }
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("spawn succeeded"),
    }
}

/// Runs `f`, which must fail to spawn, and checks that it left no fd and no child.
fn leaves_nothing<T>(f: impl FnOnce() -> T) -> T {
    let fds = open_fds();
    let before = children();
    let r = f();
    assert_eq!(children(), before, "a child of the failed spawn remains");
    assert_eq!(open_fds(), fds, "an fd of the failed spawn remains");
    r
}

// ---- CL-1: validation ----

#[test]
fn missing_program_is_not_found() {
    let _l = lock();
    let d = TempDir::new("missing");
    let e = leaves_nothing(|| {
        never_started(
            spawn(&spec(&["/nonexistent/agent"], &d.0)),
            NeverStartedKind::NotFound,
        )
    });
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn unsearchable_directory_is_permission_denied() {
    let _l = lock();
    let d = TempDir::new("unsearchable");
    let closed = d.0.join("closed");
    std::fs::create_dir(&closed).unwrap();
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
    let prog = closed.join("agent");
    leaves_nothing(|| {
        never_started(
            spawn(&spec(&[prog.to_str().unwrap()], &d.0)),
            NeverStartedKind::PermissionDenied,
        )
    });
}

#[test]
fn nul_in_argv_is_setup() {
    let _l = lock();
    let d = TempDir::new("nul");
    let e = never_started(
        spawn(&spec(&["/bin/echo", "a\0b"], &d.0)),
        NeverStartedKind::Setup,
    );
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn unexecutable_file_is_permission_denied() {
    let _l = lock();
    let d = TempDir::new("noexec");
    let prog = d.0.join("agent");
    std::fs::write(&prog, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o644)).unwrap();
    leaves_nothing(|| {
        never_started(
            spawn(&spec(&[prog.to_str().unwrap()], &d.0)),
            NeverStartedKind::PermissionDenied,
        )
    });
    // A directory is not an executable file either.
    never_started(
        spawn(&spec(&[d.0.to_str().unwrap()], &d.0)),
        NeverStartedKind::PermissionDenied,
    );
}

#[test]
fn relative_argv0_is_setup() {
    let _l = lock();
    let d = TempDir::new("relative");
    let e = never_started(spawn(&spec(&["bin/sh"], &d.0)), NeverStartedKind::Setup);
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn missing_cwd_is_setup() {
    let _l = lock();
    let d = TempDir::new("nocwd");
    let e = never_started(
        spawn(&spec(&["/bin/echo"], &d.0.join("missing"))),
        NeverStartedKind::Setup,
    );
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
    never_started(
        spawn(&spec(&["/bin/echo"], Path::new("work"))),
        NeverStartedKind::Setup,
    );
}

#[test]
fn bad_env_is_setup() {
    let _l = lock();
    let d = TempDir::new("env");
    for env in [
        vec![("A", "1"), ("A", "2")],
        vec![("A=B", "1")],
        vec![("", "1")],
        vec![("A\0", "1")],
        vec![("A", "1\0")],
    ] {
        let mut s = spec(&["/bin/echo"], &d.0);
        s.env = env
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        let e = never_started(spawn(&s), NeverStartedKind::Setup);
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{env:?}");
    }
}

// ---- CL-2: failures in the forked child ----

#[test]
fn missing_interpreter_is_never_started() {
    let _l = lock();
    let d = TempDir::new("interp");
    let e = never_started(spawn(&forked_failure(&d)), NeverStartedKind::Setup);
    // `ENOENT` from `execvp`: the interpreter, not the program, is missing.
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
}

/// A spawn that passes validation and fails in the forked child (a missing interpreter;
/// the unit tests in `src/unix/tests.rs` cover a cwd removed after validation).
fn forked_failure(d: &TempDir) -> SpawnSpec {
    let prog = d.0.join("agent");
    std::fs::write(&prog, "#!/nonexistent/interp\n").unwrap();
    std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
    spec(&[prog.to_str().unwrap()], &d.0)
}

#[test]
fn failed_spawns_leave_no_fd() {
    let _l = lock();
    let d = TempDir::new("nofd");
    let s = forked_failure(&d);
    let fds = open_fds();
    for _ in 0..5 {
        never_started(spawn(&s), NeverStartedKind::Setup);
    }
    assert_eq!(open_fds(), fds);
}

#[test]
fn failed_spawns_leave_no_child() {
    let _l = lock();
    let d = TempDir::new("nochild");
    let s = forked_failure(&d);
    for _ in 0..5 {
        never_started(spawn(&s), NeverStartedKind::Setup);
        assert_eq!(children(), "", "the forked child was not reaped");
    }
}

// ---- CL-5 ----

/// The lines `/usr/bin/env` printed with `env`.
fn env_output(env: &[(&str, &str)]) -> Vec<String> {
    let d = TempDir::new("envout");
    let mut s = spec(&["/usr/bin/env"], &d.0);
    s.env = env
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect();
    let ended = Arc::new(Mutex::new(false));
    let e2 = ended.clone();
    let session = Session::spawn(
        &*native(),
        &s,
        SessionConfig::new(move |ev| {
            if matches!(ev, SessionEvent::OutputEnded(_)) {
                *e2.lock().unwrap() = true;
            }
        }),
    )
    .unwrap();
    let end = Instant::now() + WAIT;
    while !*ended.lock().unwrap() {
        assert!(Instant::now() < end, "no output end");
        std::thread::yield_now();
    }
    let snap = session.snapshot(Instant::now() + WAIT).unwrap();
    let lines = snap
        .lines
        .iter()
        .map(chaldea_pty_runtime::Line::text)
        .map(|l| l.trim_end().to_owned())
        .filter(|l| !l.is_empty())
        .collect();
    let out = session.close(&[], CloseBounds::default());
    assert!(out.released.tree_dead, "{out:?}");
    lines
}

#[test]
fn env_is_exactly_the_spec() {
    let _l = lock();
    // The test process has variables that must not leak into the child.
    assert!(std::env::var_os("PATH").is_some());
    assert!(std::env::var_os("CARGO_PKG_NAME").is_some());
    assert_eq!(env_output(&[("A", "1")]), vec!["A=1"]);
    assert_eq!(env_output(&[]), Vec::<String>::new());
}
