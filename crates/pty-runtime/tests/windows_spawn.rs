//! Public-API spawn tests of the Windows backend (`docs/design/pty-windows.md` §7):
//! validation failures and the environment. Every test holds `LOCK`.

#![cfg(windows)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chaldea_pty_runtime::{
    Cleanup, CloseBounds, NeverStartedKind, PtySize, Session, SessionConfig, SessionEvent,
    SpawnError, SpawnSpec, native,
};

static LOCK: Mutex<()> = Mutex::new(());

const WAIT: Duration = Duration::from_secs(10);

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A fresh directory in the OS temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!("chaldea-wspawn-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn system_root() -> OsString {
    std::env::var_os("SystemRoot").expect("SystemRoot is set on Windows")
}

fn cmd_exe() -> PathBuf {
    Path::new(&system_root()).join(r"System32\cmd.exe")
}

fn spec(argv: &[OsString], cwd: &Path) -> SpawnSpec {
    SpawnSpec {
        argv: argv.to_vec(),
        cwd: cwd.to_path_buf(),
        env: vec![("SystemRoot".into(), system_root())],
        size: PtySize::new(80, 24).unwrap(),
    }
}

fn cmd(args: &[&str], cwd: &Path) -> SpawnSpec {
    let mut argv: Vec<OsString> = vec![cmd_exe().into(), "/d".into(), "/c".into()];
    argv.extend(args.iter().map(OsString::from));
    spec(&argv, cwd)
}

#[allow(
    clippy::result_large_err,
    reason = "the error carries its cleanup evidence by value"
)]
fn spawn(spec: &SpawnSpec) -> Result<Session, SpawnError> {
    Session::spawn(&*native(), spec, SessionConfig::new(|_| {}))
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

fn invalid_input(r: Result<Session, SpawnError>) {
    let e = never_started(r, NeverStartedKind::Setup);
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{e}");
}

// ---- CL-1: validation ----

#[test]
fn missing_program_is_not_found() {
    let _l = lock();
    let d = TempDir::new("missing");
    never_started(
        spawn(&spec(&[d.0.join("agent.exe").into()], &d.0)),
        NeverStartedKind::NotFound,
    );
}

#[test]
fn relative_argv0_is_setup() {
    let _l = lock();
    let d = TempDir::new("relative");
    invalid_input(spawn(&spec(&["cmd.exe".into()], &d.0)));
}

#[test]
fn quote_in_argv0_is_setup() {
    let _l = lock();
    let d = TempDir::new("quote");
    invalid_input(spawn(&spec(&[r#"C:\a"b.exe"#.into()], &d.0)));
}

#[test]
fn nul_in_argv_is_setup() {
    let _l = lock();
    let d = TempDir::new("nul");
    invalid_input(spawn(&cmd(&["exit\u{0}0"], &d.0)));
}

#[test]
fn batch_file_is_refused() {
    let _l = lock();
    let d = TempDir::new("batch");
    for name in ["a.cmd", "b.BAT"] {
        let p = d.0.join(name);
        std::fs::write(&p, b"@exit 0\r\n").unwrap();
        invalid_input(spawn(&spec(&[p.into()], &d.0)));
    }
}

#[test]
fn directory_argv0_is_permission_denied() {
    let _l = lock();
    let d = TempDir::new("dir");
    never_started(
        spawn(&spec(&[d.0.clone().into()], &d.0)),
        NeverStartedKind::PermissionDenied,
    );
}

#[test]
fn missing_cwd_is_setup() {
    let _l = lock();
    let d = TempDir::new("nocwd");
    let e = never_started(
        spawn(&cmd(&["exit", "0"], &d.0.join("gone"))),
        NeverStartedKind::Setup,
    );
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{e}");
}

#[test]
fn verbatim_cwd_that_normalizes_differently_is_setup() {
    let _l = lock();
    let d = TempDir::new("verbatim");
    let real = std::fs::canonicalize(&d.0).unwrap();
    // `dir.` is its own directory under a verbatim path, while Win32 normalization of
    // the stripped form would name `dir`.
    std::fs::create_dir(real.join("dir")).unwrap();
    let dotted = PathBuf::from(format!(r"{}\dir.", real.display()));
    std::fs::create_dir(&dotted).unwrap();
    assert!(dotted.is_dir());
    invalid_input(spawn(&cmd(&["exit", "0"], &dotted)));
}

#[test]
fn bad_env_is_setup() {
    let _l = lock();
    let d = TempDir::new("env");
    for env in [
        vec![("A=B", "1")],
        vec![("A\u{0}", "1")],
        vec![("A", "1\u{0}")],
        vec![("", "1")],
        vec![("Path", "a"), ("PATH", "b")],
    ] {
        let mut s = cmd(&["exit", "0"], &d.0);
        s.env = env
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        invalid_input(spawn(&s));
    }
}

#[test]
fn command_line_over_the_limit_is_setup() {
    let _l = lock();
    let d = TempDir::new("long");
    let base = cmd(&[], &d.0);
    // `"<cmd.exe>" /d /c <pad>` plus the NUL: one unit over 32 767.
    let fixed = cmd_exe().as_os_str().encode_wide_len() + 2 + " /d /c ".len() + 1;
    let pad = "x".repeat(32_768 - fixed);
    let mut s = base;
    s.argv.push(pad.into());
    invalid_input(spawn(&s));
}

trait WideLen {
    fn encode_wide_len(&self) -> usize;
}

impl WideLen for std::ffi::OsStr {
    fn encode_wide_len(&self) -> usize {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().count()
    }
}

// ---- CL-8 ----

/// The non-empty screen lines of `cmd.exe /d /c set` run with `env`, read from the final
/// snapshot after the close (output ends only when the pseudoconsole closes).
fn set_output(env: &[(OsString, OsString)]) -> Vec<String> {
    let d = TempDir::new("set");
    let mut s = cmd(&["set"], &d.0);
    s.env = env.to_vec();
    s.size = PtySize::new(400, 60).unwrap();
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let session = Session::spawn(
        &*native(),
        &s,
        SessionConfig::new(move |ev| {
            if let SessionEvent::Exited(e) = ev {
                let _ = tx.lock().unwrap().send(e);
            }
        }),
    )
    .unwrap();
    let exit = rx.recv_timeout(WAIT).expect("cmd.exe exited");
    assert_eq!(exit.code, Some(0));
    let out = session.close(&[], CloseBounds::default());
    assert!(out.released.tree_dead, "{out:?}");
    out.final_snapshot
        .expect("a final snapshot")
        .lines
        .iter()
        .map(chaldea_pty_runtime::Line::text)
        .map(|l| l.trim_end().to_owned())
        .filter(|l| !l.is_empty())
        .collect()
}

#[test]
fn env_is_exactly_the_spec() {
    let _l = lock();
    let path = std::env::var("PATH").expect("the test has a PATH");
    assert!(std::env::var_os("CARGO_PKG_NAME").is_some());
    let root = system_root();
    let lines = set_output(&[
        ("A".into(), "1".into()),
        ("SystemRoot".into(), root.clone()),
    ]);
    assert!(lines.iter().any(|l| l == "A=1"), "{lines:?}");
    let want = format!("SystemRoot={}", root.to_string_lossy());
    assert!(lines.contains(&want), "{lines:?}");
    assert!(
        !lines.iter().any(|l| l.starts_with("CARGO_PKG_NAME=")),
        "{lines:?}"
    );
    let head: String = path.chars().take(40).collect();
    assert!(
        !lines.iter().any(|l| l.contains(&head)),
        "the test's PATH leaked: {lines:?}"
    );
}
