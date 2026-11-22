//! Real-process unit tests of the Windows backend with system binaries and stage hooks
//! (`docs/design/pty-windows.md` §7). Every test holds `serial()`.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::child::WindowsChild;
use super::hooks::Marker;

static SERIAL: Mutex<()> = Mutex::new(());

/// Serializes the real-process unit tests of `src/windows/`.
pub(crate) fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The milestones a test hook can act on.
#[derive(Debug)]
pub(crate) enum Stage {
    /// Validation passed; nothing is created yet.
    Validated,
    /// Inside the spawn lock, before `CreatePseudoConsole` (2) or each `CreateProcessW` (5).
    LockAcquired { stage: u8 },
    /// Before the bounded wait on a terminated suspended child (stage 5.4).
    BeforeSuspendedWait { pid: u32 },
    /// Before stage 6; the hook returning true fails the identity read.
    Spawned { pid: u32 },
}

type StageFn = Box<dyn Fn(&Stage) -> bool + Send + Sync>;
type LineFn = Box<dyn Fn(bool, &mut Vec<u16>) + Send + Sync>;

/// Test-only behaviour of one `WindowsPty`.
#[derive(Default)]
pub(crate) struct Hooks {
    pub(crate) stage: Option<StageFn>,
    /// Sees (and may overwrite) each attempt's fresh command-line copy; `true` for the
    /// suspended attempt.
    pub(crate) line: Option<LineFn>,
    /// A marker the drain's reader watches (no hold).
    pub(crate) marker: Option<&'static [u8]>,
    pub(crate) seen: Mutex<Option<Arc<Marker>>>,
    /// The last child built, kept alive until the test takes it.
    pub(crate) child: Mutex<Option<Arc<WindowsChild>>>,
}

impl Hooks {
    /// Runs the stage hook; true asks the stage to fail.
    pub(crate) fn on(&self, stage: &Stage) -> bool {
        self.stage.as_ref().is_some_and(|f| f(stage))
    }

    pub(crate) fn command_line(&self, suspended: bool, line: &mut Vec<u16>) {
        if let Some(f) = &self.line {
            f(suspended, line);
        }
    }

    pub(crate) fn marker(&self) -> Option<Arc<Marker>> {
        let m = Marker::new(self.marker?, false);
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(m.clone());
        Some(m)
    }

    pub(crate) fn keep(&self, child: Arc<WindowsChild>) {
        *self.child.lock().unwrap_or_else(PoisonError::into_inner) = Some(child);
    }
}

// ---- Helpers ----

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::windows::io::AsHandle;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::hooks::{self, FaultReport, WindowsFault};
use super::{Input, WindowsPty, sys};
use crate::backend::{
    Cleanup, DrainSink, NeverStartedKind, PtyChild, PtyRuntime, PtySize, SpawnError, SpawnSpec,
    SpawnedParts, SpawnedPty,
};
use crate::drain::DrainEvent;
use crate::session::OutputEnd;

const BOUND: Duration = Duration::from_secs(10);

/// What the drain delivered.
#[derive(Default)]
pub(crate) struct Out {
    st: Mutex<(Vec<u8>, Option<OutputEnd>)>,
    cv: Condvar,
}

impl Out {
    fn sink(self: &Arc<Self>) -> DrainSink {
        let out = self.clone();
        DrainSink::test(move |ev| {
            let mut st = out.st.lock().unwrap_or_else(PoisonError::into_inner);
            match ev {
                DrainEvent::Bytes { bytes, .. } => st.0.extend_from_slice(bytes),
                DrainEvent::End(end) => st.1 = Some(end.clone()),
            }
            drop(st);
            out.cv.notify_all();
        })
    }

    fn wait_end(&self) -> Option<OutputEnd> {
        let end = Instant::now() + BOUND;
        let mut st = self.st.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(e) = &st.1 {
                return Some(e.clone());
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            st = self.cv.wait_timeout(st, left).unwrap().0;
        }
    }

    fn end(&self) -> Option<OutputEnd> {
        self.st
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .1
            .clone()
    }
}

fn system_root() -> OsString {
    std::env::var_os("SystemRoot").expect("SystemRoot is set on Windows")
}

fn cmd_exe() -> PathBuf {
    Path::new(&system_root()).join(r"System32\cmd.exe")
}

/// A fresh directory in the OS temp dir, removed on drop.
pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    pub(crate) fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("chaldea-win-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `cmd.exe /d /c <command>` in the temp dir with only `SystemRoot` set.
fn cmd(command: &str) -> SpawnSpec {
    SpawnSpec {
        argv: vec![cmd_exe().into(), "/d".into(), "/c".into(), command.into()],
        cwd: std::env::temp_dir(),
        env: vec![("SystemRoot".into(), system_root())],
        size: PtySize::new(80, 24).unwrap(),
    }
}

fn spawn(pty: &WindowsPty, spec: &SpawnSpec) -> (Result<SpawnedParts, SpawnError>, Arc<Out>) {
    let out = Arc::new(Out::default());
    let r = pty.spawn(spec, out.sink()).map(SpawnedPty::into_parts);
    (r, out)
}

#[allow(clippy::result_large_err, reason = "the backend's own error type")]
fn spawn_with(
    fault: WindowsFault,
    pty: &WindowsPty,
    spec: &SpawnSpec,
) -> Result<SpawnedParts, SpawnError> {
    hooks::inject_next(fault);
    spawn(pty, spec).0
}

fn report() -> FaultReport {
    hooks::report().expect("a native spawn ran on this thread")
}

fn take_child(pty: &WindowsPty) -> Option<Arc<WindowsChild>> {
    pty.hooks.child.lock().unwrap().take()
}

/// Releases a live session's parts by the failure teardown (kill first).
fn release(parts: SpawnedParts) -> Cleanup {
    parts.teardown()
}

fn never_started<T>(r: Result<T, SpawnError>) -> (NeverStartedKind, io::Error, Cleanup) {
    match r {
        Err(SpawnError::NeverStarted {
            kind,
            error,
            cleanup,
        }) => (kind, error, cleanup),
        other => panic!("not NeverStarted: {}", describe(&other)),
    }
}

fn started_then_failed<T>(
    r: Result<T, SpawnError>,
) -> (
    &'static str,
    Option<crate::backend::ProcessIdentity>,
    Cleanup,
) {
    match r {
        Err(SpawnError::StartedThenFailed {
            stage,
            identity,
            cleanup,
            ..
        }) => (stage, identity, cleanup),
        other => panic!("not StartedThenFailed: {}", describe(&other)),
    }
}

fn describe<T>(r: &Result<T, SpawnError>) -> String {
    match r {
        Ok(_) => "Ok".into(),
        Err(e) => format!("{e} / {:?}", e.cleanup()),
    }
}

/// A process handle on `pid` after a creation-time check against `start`.
fn open_verified(pid: u32, start: &str) -> std::os::windows::io::OwnedHandle {
    let h = sys::open_process(pid, super::QUERY_SYNC).unwrap();
    assert_eq!(
        sys::creation_time(h.as_handle()).unwrap().to_string(),
        start
    );
    h
}

fn names(r: &FaultReport) -> Vec<&'static str> {
    r.names()
}

/// Position of the first entry named `name` at or after `from`.
fn pos(n: &[&str], name: &str, from: usize) -> usize {
    n.iter()
        .skip(from)
        .position(|x| *x == name)
        .map(|i| i + from)
        .unwrap_or_else(|| panic!("no {name} after {from} in {n:?}"))
}

// ---- CL-1 ----

#[test]
fn invalid_spec_creates_nothing() {
    let _s = serial();
    let reached = Arc::new(AtomicBool::new(false));
    let r2 = reached.clone();
    let pty = WindowsPty {
        hooks: Hooks {
            stage: Some(Box::new(move |_| {
                r2.store(true, Ordering::Release);
                false
            })),
            ..Hooks::default()
        },
    };
    let mut spec = cmd("exit 0");
    spec.argv[0] = "cmd.exe".into();
    let (r, _) = spawn(&pty, &spec);
    assert!(
        matches!(
            &r,
            Err(SpawnError::NeverStarted {
                kind: NeverStartedKind::Setup,
                cleanup: Cleanup::Confirmed,
                ..
            })
        ),
        "{}",
        describe(&r)
    );
    assert!(!reached.load(Ordering::Acquire), "no stage hook reached");
    assert!(report().trace.is_empty(), "{:?}", report());
}

// ---- CL-2 ----

fn both_attempts(r: &FaultReport) -> Vec<&hooks::TraceEntry> {
    let c = r.entries("create_process");
    assert_eq!(c.len(), 2, "{r:?}");
    assert_eq!(c[0].flags & sys::SUSPENDED, 0);
    assert_eq!(c[1].flags & sys::SUSPENDED, sys::SUSPENDED);
    c
}

#[test]
fn program_removed_after_validation_is_not_found() {
    let _s = serial();
    let dir = TempDir::new("gone");
    let exe = dir.0.join("c.exe");
    std::fs::copy(cmd_exe(), &exe).unwrap();
    let gone = exe.clone();
    let pty = WindowsPty {
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
    let mut spec = cmd("exit 0");
    spec.argv[0] = exe.into();
    let (r, _) = spawn(&pty, &spec);
    let (kind, error, cleanup) = never_started(r);
    assert_eq!(kind, NeverStartedKind::NotFound, "{error}");
    assert_eq!(cleanup, Cleanup::Confirmed);
    let msg = error.to_string();
    assert!(
        msg.contains("job_list: create_process: ") && msg.contains("; suspended: create_process: "),
        "{msg}"
    );
    let rep = report();
    let c = both_attempts(&rep);
    assert!(c.iter().all(|e| e.native && e.result.is_err()), "{c:?}");
}

#[test]
fn cwd_removed_after_validation_is_setup() {
    let _s = serial();
    let dir = TempDir::new("cwd");
    let cwd = dir.0.join("d");
    std::fs::create_dir(&cwd).unwrap();
    let gone = cwd.clone();
    let pty = WindowsPty {
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
    let mut spec = cmd("exit 0");
    spec.cwd = cwd;
    let (r, _) = spawn(&pty, &spec);
    let (kind, error, cleanup) = never_started(r);
    assert_eq!(kind, NeverStartedKind::Setup, "{error}");
    assert_eq!(cleanup, Cleanup::Confirmed);
    assert!(
        error.to_string().contains("; suspended: create_process: "),
        "{error}"
    );
    both_attempts(&report());
}

#[test]
fn equal_codes_are_both_kept() {
    let _s = serial();
    let dir = TempDir::new("badexe");
    let exe = dir.0.join("x.exe");
    std::fs::write(&exe, b"not a PE file").unwrap();
    let mut spec = cmd("exit 0");
    spec.argv[0] = exe.into();
    let (r, _) = spawn(&WindowsPty::default(), &spec);
    let (kind, error, _) = never_started(r);
    assert_eq!(kind, NeverStartedKind::Setup);
    let bad = io::Error::from_raw_os_error(193).to_string();
    let msg = error.to_string();
    assert_eq!(msg.matches(&bad).count(), 2, "{msg}");
    assert!(
        msg.contains("job_list: create_process: ") && msg.contains("suspended: create_process: "),
        "{msg}"
    );
    let rep = report();
    let c = both_attempts(&rep);
    assert!(c.iter().all(|e| e.result == Err(193)), "{c:?}");
}

#[test]
fn create_process_fault_keeps_both_errors() {
    let _s = serial();
    let r = spawn_with(
        WindowsFault::CreateProcess,
        &WindowsPty::default(),
        &cmd("exit 0"),
    );
    let (kind, error, cleanup) = never_started(r);
    assert_eq!(kind, NeverStartedKind::Setup);
    assert_eq!(cleanup, Cleanup::Confirmed);
    let msg = error.to_string();
    let e50 = io::Error::from_raw_os_error(50).to_string();
    let e8 = io::Error::from_raw_os_error(8).to_string();
    assert!(
        msg.contains(&format!("job_list: create_process: {e50}")),
        "{msg}"
    );
    assert!(
        msg.contains(&format!("suspended: create_process: {e8}")),
        "{msg}"
    );
    let rep = report();
    let c = both_attempts(&rep);
    assert_eq!((c[0].native, c[0].result), (false, Err(50)));
    assert_eq!((c[1].native, c[1].result), (false, Err(8)));
}

#[test]
fn job_list_attribute_failure_falls_back() {
    let _s = serial();
    let pty = WindowsPty::default();
    let r = spawn_with(WindowsFault::JobListAttr, &pty, &cmd("pause"));
    let parts = r.unwrap_or_else(|e| panic!("{e}"));
    let ev: std::collections::HashMap<_, _> = parts.evidence.iter().cloned().collect();
    assert_eq!(ev["job_branch"], "suspended");
    assert!(ev["job_list_error"].starts_with("set_job_list: "), "{ev:?}");
    let c = report();
    let c = c.entries("create_process");
    assert_eq!(
        c.len(),
        1,
        "only the suspended attempt calls CreateProcessW"
    );
    assert!(c[0].native && c[0].flags & sys::SUSPENDED != 0);
    drop(take_child(&pty));
    assert_eq!(release(parts), Cleanup::Confirmed);
}

#[test]
fn shared_attribute_failure_does_not_retry() {
    let _s = serial();
    let r = spawn_with(
        WindowsFault::PseudoConsoleAttr,
        &WindowsPty::default(),
        &cmd("exit 0"),
    );
    assert!(
        matches!(
            &r,
            Err(SpawnError::NeverStarted {
                kind: NeverStartedKind::Setup,
                cleanup: Cleanup::Confirmed,
                ..
            })
        ),
        "{}",
        describe(&r)
    );
    let rep = report();
    assert!(rep.entries("create_process").is_empty(), "{rep:?}");
    assert_eq!(rep.entries("attr_init").len(), 1);
    assert_eq!(rep.entries("attr_delete").len(), 1);
}

// ---- CL-3 ----

/// The exact trace of each pre-process fault: what its stage acquired, then the releases
/// in order (close → input → drain → job), none under the lock.
#[test]
fn pre_process_faults_release_in_order() {
    let _s = serial();
    const OPEN: &[&str] = &[
        "pipe",
        "pipe",
        "close_worker_start",
        "create_pseudo_console",
        "drain_start",
    ];
    const JOB: &[&str] = &["job_create", "job_limits"];
    const EARLY: &[&str] = &["close", "input_drop", "drain_finish", "job_drop"];
    let cat = |parts: &[&[&'static str]]| -> Vec<&'static str> { parts.concat() };
    let cases: Vec<(WindowsFault, Vec<&'static str>)> = vec![
        (WindowsFault::FirstPipe, vec![]),
        (WindowsFault::SecondPipe, vec!["pipe", "input_drop"]),
        (
            WindowsFault::CloseWorker,
            vec!["pipe", "pipe", "close_worker_start", "input_drop"],
        ),
        (
            WindowsFault::PseudoConsole,
            vec![
                "pipe",
                "pipe",
                "close_worker_start",
                "create_pseudo_console",
                "worker_close",
                "input_drop",
            ],
        ),
        (
            WindowsFault::DrainStart,
            vec![
                "pipe",
                "pipe",
                "close_worker_start",
                "create_pseudo_console",
                "output_drop",
                "drain_start",
                "close",
                "input_drop",
            ],
        ),
        (
            WindowsFault::JobCreate,
            cat(&[OPEN, &["job_create"], &EARLY[..3]]),
        ),
        (
            WindowsFault::JobLimits,
            cat(&[OPEN, JOB, &["job_drop"], &EARLY[..3]]),
        ),
        (WindowsFault::AttrInit, cat(&[OPEN, JOB, EARLY])),
        (
            WindowsFault::PseudoConsoleAttr,
            cat(&[OPEN, JOB, &["attr_init", "attr_delete"], EARLY]),
        ),
        (
            WindowsFault::CreateProcess,
            cat(&[
                OPEN,
                JOB,
                &[
                    "attr_init",
                    "create_process",
                    "attr_delete",
                    "attr_init",
                    "create_process",
                    "attr_delete",
                ],
                EARLY,
            ]),
        ),
    ];
    for (fault, want) in cases {
        let label = format!("{fault:?}");
        let r = spawn_with(fault, &WindowsPty::default(), &cmd("exit 0"));
        assert!(
            matches!(
                &r,
                Err(SpawnError::NeverStarted {
                    kind: NeverStartedKind::Setup,
                    cleanup: Cleanup::Confirmed,
                    ..
                })
            ),
            "{label}: {}",
            describe(&r)
        );
        let rep = report();
        assert_eq!(names(&rep), want, "{label}");
        for e in &rep.trace {
            let under_lock = matches!(e.name, "create_pseudo_console" | "create_process");
            assert_eq!(e.lock_held, under_lock, "{label}: {e:?}");
        }
        assert_eq!(
            rep.entries("attr_init").len(),
            rep.entries("attr_delete").len(),
            "{label}"
        );
    }
}

#[test]
fn close_worker_fault_creates_no_pseudoconsole() {
    let _s = serial();
    let r = spawn_with(
        WindowsFault::CloseWorker,
        &WindowsPty::default(),
        &cmd("exit 0"),
    );
    assert!(
        matches!(
            &r,
            Err(SpawnError::NeverStarted {
                kind: NeverStartedKind::Setup,
                cleanup: Cleanup::Confirmed,
                ..
            })
        ),
        "{}",
        describe(&r)
    );
    assert!(
        !names(&report()).contains(&"create_pseudo_console"),
        "{:?}",
        report()
    );
}

// ---- CL-4 ----

/// The PE import directory of a file: each descriptor's DLL name and imported names.
fn pe_imports(b: &[u8]) -> Vec<(String, Vec<String>)> {
    let u16_at = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap());
    let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let pe = u32_at(0x3c) as usize;
    assert_eq!(&b[pe..pe + 4], b"PE\0\0");
    let sections = usize::from(u16_at(pe + 6));
    let opt = pe + 24;
    let opt_size = usize::from(u16_at(pe + 20));
    let pe32_plus = u16_at(opt) == 0x20b;
    let dirs = opt + if pe32_plus { 112 } else { 96 };
    let import_rva = u32_at(dirs + 8) as usize;
    let table = opt + opt_size;
    let off = |rva: usize| -> usize {
        for i in 0..sections {
            let s = table + i * 40;
            let (vsize, va, rsize, raw) = (
                u32_at(s + 8) as usize,
                u32_at(s + 12) as usize,
                u32_at(s + 16) as usize,
                u32_at(s + 20) as usize,
            );
            if rva >= va && rva < va + vsize.max(rsize) {
                return rva - va + raw;
            }
        }
        panic!("rva {rva:#x} in no section");
    };
    let cstr = |o: usize| -> String {
        let end = b[o..].iter().position(|&c| c == 0).unwrap();
        String::from_utf8_lossy(&b[o..o + end]).into_owned()
    };
    let mut out = Vec::new();
    let mut d = off(import_rva);
    loop {
        let (oft, name, ft) = (
            u32_at(d) as usize,
            u32_at(d + 12) as usize,
            u32_at(d + 16) as usize,
        );
        if name == 0 && ft == 0 {
            break;
        }
        let dll = cstr(off(name));
        let mut names = Vec::new();
        let mut t = off(if oft != 0 { oft } else { ft });
        loop {
            let (entry, ordinal) = if pe32_plus {
                let v = u64_at(t);
                (v & 0x7fff_ffff_ffff_ffff, v >> 63 == 1)
            } else {
                let v = u64::from(u32_at(t));
                (v & 0x7fff_ffff, v >> 31 == 1)
            };
            if entry == 0 && !ordinal {
                break;
            }
            if !ordinal {
                names.push(cstr(off(usize::try_from(entry).unwrap()) + 2));
            }
            t += if pe32_plus { 8 } else { 4 };
        }
        out.push((dll, names));
        d += 20;
    }
    out
}

#[test]
fn binary_imports_conpty_from_kernel32() {
    let bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    let imports = pe_imports(&bytes);
    assert!(
        !imports
            .iter()
            .any(|(d, _)| d.eq_ignore_ascii_case("conpty.dll")),
        "{:?}",
        imports.iter().map(|i| &i.0).collect::<Vec<_>>()
    );
    let from_kernel32: Vec<&String> = imports
        .iter()
        .filter(|(d, _)| d.eq_ignore_ascii_case("kernel32.dll"))
        .flat_map(|(_, n)| n)
        .collect();
    for f in [
        "CreatePseudoConsole",
        "ResizePseudoConsole",
        "ClosePseudoConsole",
    ] {
        assert!(
            from_kernel32.iter().any(|n| *n == f),
            "{f} not imported from kernel32.dll"
        );
    }
    for (d, n) in &imports {
        if !d.eq_ignore_ascii_case("kernel32.dll") {
            assert!(
                !n.iter().any(|x| x.contains("PseudoConsole")),
                "{d} supplies {n:?}"
            );
        }
    }
}

// ---- CL-6 ----

fn marker_cmd(marker: &Path) -> SpawnSpec {
    cmd(&format!("type nul > \"{}\"", marker.display()))
}

#[test]
fn assign_failure_terminates_the_suspended_child() {
    let _s = serial();
    let dir = TempDir::new("assign");
    let marker = dir.0.join("ran");
    let r = spawn_with(
        WindowsFault::Assign,
        &WindowsPty::default(),
        &marker_cmd(&marker),
    );
    let (kind, error, cleanup) = never_started(r);
    assert_eq!(kind, NeverStartedKind::Setup);
    assert_eq!(cleanup, Cleanup::Confirmed);
    assert!(error.to_string().contains("suspended: assign: "), "{error}");
    let rep = report();
    let n = names(&rep);
    let a = pos(&n, "assign", 0);
    let t = pos(&n, "terminate_process", a);
    let w = pos(&n, "wait", t);
    assert_eq!(rep.trace[t].result, Ok(0), "{rep:?}");
    assert_eq!(rep.trace[w].result, Ok(1), "{rep:?}");
    assert_eq!(rep.child_exited, Some(true));
    assert!(!marker.exists(), "the program never ran");
}

#[test]
fn resume_failure_terminates_through_the_job() {
    let _s = serial();
    let dir = TempDir::new("resume");
    let marker = dir.0.join("ran");
    let r = spawn_with(
        WindowsFault::ResumeFails,
        &WindowsPty::default(),
        &marker_cmd(&marker),
    );
    let (kind, _, cleanup) = never_started(r);
    assert_eq!(kind, NeverStartedKind::Setup);
    assert_eq!(cleanup, Cleanup::Confirmed);
    let rep = report();
    let n = names(&rep);
    let a = pos(&n, "assign", 0);
    assert_eq!(rep.trace[a].result, Ok(0));
    let r = pos(&n, "resume", a);
    let t = pos(&n, "terminate_job", r);
    let w = pos(&n, "wait", t);
    assert_eq!(rep.trace[t].result, Ok(0), "{rep:?}");
    assert_eq!(rep.trace[w].result, Ok(1), "{rep:?}");
    assert!(!marker.exists());
}

#[test]
fn denied_termination_after_assign_failure_is_unresolved() {
    let _s = serial();
    let dir = TempDir::new("denied");
    let marker = dir.0.join("ran");
    let r = spawn_with(
        WindowsFault::AssignTerminateDenied,
        &WindowsPty::default(),
        &marker_cmd(&marker),
    );
    let (stage, identity, cleanup) = started_then_failed(r);
    assert_eq!(stage, "job_assign");
    let id = identity.expect("an identity");
    assert!(!id.start_time.is_empty());
    assert_eq!(id.group, "");
    let Cleanup::Unresolved { kill: Some(k), .. } = &cleanup else {
        panic!("{cleanup:?}");
    };
    assert_eq!(k.method, "windows_process");
    assert_eq!(k.survivors, vec![id.clone()]);
    let rep = report();
    let t = rep.entries("terminate_process");
    assert_eq!(t[0].result, Err(5), "a real ACCESS_DENIED: {rep:?}");
    assert_eq!(rep.child_exited, Some(false));
    // It is a live suspended process; the test kills it through the identity.
    let w = hooks::watch(&id).unwrap();
    assert!(!w.signalled(Duration::ZERO).unwrap());
    assert!(hooks::kill_verified(id.pid, &id.start_time));
    assert!(w.signalled(BOUND).unwrap());
    assert!(!marker.exists());
}

#[test]
fn failed_termination_then_exit_is_confirmed() {
    let _s = serial();
    let dir = TempDir::new("exit");
    let marker = dir.0.join("ran");
    let pty = WindowsPty {
        hooks: Hooks {
            stage: Some(Box::new(|s| {
                if let Stage::BeforeSuspendedWait { pid } = s {
                    let h = sys::open_process(*pid, super::KILL_SYNC).unwrap();
                    sys::terminate_process(&h, 1).unwrap();
                }
                false
            })),
            ..Hooks::default()
        },
    };
    let r = spawn_with(
        WindowsFault::AssignTerminateDenied,
        &pty,
        &marker_cmd(&marker),
    );
    assert!(
        matches!(
            &r,
            Err(SpawnError::NeverStarted {
                kind: NeverStartedKind::Setup,
                ..
            })
        ),
        "{}",
        describe(&r)
    );
    let rep = report();
    assert_eq!(rep.entries("terminate_process")[0].result, Err(5));
    assert_eq!(rep.child_exited, Some(true));
}

#[test]
fn denied_job_termination_is_unresolved() {
    let _s = serial();
    let dir = TempDir::new("jobdenied");
    let marker = dir.0.join("ran");
    let watch: Arc<Mutex<Option<hooks::Watch>>> = Arc::default();
    let w2 = watch.clone();
    let pty = WindowsPty {
        hooks: Hooks {
            stage: Some(Box::new(move |s| {
                if let Stage::BeforeSuspendedWait { pid } = s {
                    let id = hooks::identify(*pid).unwrap().unwrap();
                    *w2.lock().unwrap() = Some(hooks::watch(&id).unwrap());
                }
                false
            })),
            ..Hooks::default()
        },
    };
    let r = spawn_with(
        WindowsFault::ResumeTerminateDenied,
        &pty,
        &marker_cmd(&marker),
    );
    let (stage, identity, cleanup) = started_then_failed(r);
    assert_eq!(stage, "resume");
    let id = identity.unwrap();
    assert_eq!(id.group, format!("job:{}", id.pid));
    let Cleanup::Unresolved { kill: Some(k), .. } = &cleanup else {
        panic!("{cleanup:?}");
    };
    assert_eq!(k.method, "windows_job");
    assert_eq!(report().entries("terminate_job")[0].result, Err(5));
    // Kill-on-close at the job drop (in `release_early`) ends it after `spawn` returned.
    let w = watch.lock().unwrap().take().expect("watched at the wait");
    assert!(w.signalled(BOUND).unwrap());
    assert!(!marker.exists());
}

// ---- CL-7 ----

fn identity_fault() -> WindowsPty {
    WindowsPty {
        hooks: Hooks {
            stage: Some(Box::new(|s| matches!(s, Stage::Spawned { .. }))),
            ..Hooks::default()
        },
    }
}

#[test]
fn expired_teardown_kill_reports_unresolved() {
    let _s = serial();
    let pty = identity_fault();
    let r = spawn_with(WindowsFault::KillExpired, &pty, &cmd("pause"));
    let (stage, _, cleanup) = started_then_failed(r);
    assert_eq!(stage, "identity");
    let Cleanup::Unresolved { kill: Some(k), .. } = &cleanup else {
        panic!("{cleanup:?}");
    };
    assert!(!k.tree_dead);
    // The leader still runs; dropping the last child reference closes the job.
    let child = take_child(&pty).unwrap();
    let watch = sys::open_process(child.identity().pid, super::QUERY_SYNC).unwrap();
    drop(child);
    assert!(sys::wait_floor(watch.as_handle(), BOUND).unwrap());
}

#[test]
fn teardown_order() {
    let _s = serial();
    let pty = identity_fault();
    let (r, _) = spawn(&pty, &cmd("pause"));
    assert!(
        matches!(
            &r,
            Err(SpawnError::StartedThenFailed {
                stage: "identity",
                cleanup: Cleanup::Confirmed,
                ..
            })
        ),
        "{}",
        describe(&r)
    );
    drop(take_child(&pty));
    let rep = report();
    let n = names(&rep);
    let c = pos(&n, "creation_time", 0);
    let k = pos(&n, "terminate_job", c);
    let i = pos(&n, "input_drop", k);
    let cl = pos(&n, "close", i);
    pos(&n, "drain_finish", cl);
}

// ---- CL-9 ----

#[test]
fn each_attempt_gets_a_fresh_command_line() {
    let _s = serial();
    let seen: Arc<Mutex<Vec<Vec<u16>>>> = Arc::default();
    let s2 = seen.clone();
    let pty = WindowsPty {
        hooks: Hooks {
            line: Some(Box::new(move |suspended, line| {
                s2.lock().unwrap().push(line.clone());
                if !suspended {
                    // What a `CreateProcessW` writing into the first buffer would leave.
                    line.iter_mut()
                        .rev()
                        .skip(1)
                        .for_each(|c| *c = u16::from(b'X'));
                }
            })),
            ..Hooks::default()
        },
    };
    let spec = cmd("pause");
    let r = spawn_with(WindowsFault::RejectJobList(None), &pty, &spec);
    let parts = r.unwrap_or_else(|e| panic!("{e}"));
    let want = super::cmdline::command_line(&spec.argv);
    assert_eq!(*seen.lock().unwrap(), vec![want.clone(), want]);
    drop(take_child(&pty));
    assert_eq!(release(parts), Cleanup::Confirmed);
}

// ---- CL-13 ----

#[test]
fn drain_starts_before_the_process() {
    let _s = serial();
    let pty = WindowsPty::default();
    let (r, _) = spawn(&pty, &cmd("exit 0"));
    let parts = r.unwrap_or_else(|e| panic!("{e}"));
    let n = names(&report());
    assert!(
        pos(&n, "drain_start", 0) < pos(&n, "create_process", 0),
        "{n:?}"
    );
    drop(take_child(&pty));
    let _ = release(parts);
}

#[test]
fn output_ends_only_after_close() {
    let _s = serial();
    let pty = WindowsPty {
        hooks: Hooks {
            marker: Some(b"READY"),
            ..Hooks::default()
        },
    };
    let (r, out) = spawn(&pty, &cmd("echo READY"));
    let parts = r.unwrap_or_else(|e| panic!("{e}"));
    let exit = parts.child.wait_timeout(BOUND).unwrap();
    assert_eq!(exit.map(|e| e.code), Some(Some(0)));
    let marker = pty.hooks.seen.lock().unwrap().clone().unwrap();
    assert!(marker.wait_seen(Instant::now() + BOUND), "READY not seen");
    assert!(marker.seen());
    assert_eq!(out.end(), None, "no EOF before the pseudoconsole closes");
    let SpawnedParts {
        child,
        output,
        input,
        control,
        ..
    } = parts;
    drop(input);
    let close = control.close(Instant::now() + BOUND);
    assert!(close.elapsed.is_some());
    assert_eq!(out.wait_end(), Some(OutputEnd::Eof));
    assert_eq!(output.finish(BOUND), Some(OutputEnd::Eof));
    drop(child);
    drop(take_child(&pty));
}

// ---- CL-14 ----

#[test]
fn dropping_input_writes_nothing() {
    let (mut r, w) = std::io::pipe().unwrap();
    drop(Input(w));
    let mut buf = [0u8; 16];
    assert_eq!(r.read(&mut buf).unwrap(), 0);
}

// ---- CL-15 ----

fn live_child(pty: &WindowsPty) -> (SpawnedParts, Arc<WindowsChild>) {
    let (r, _) = spawn(pty, &cmd("pause"));
    let parts = r.unwrap_or_else(|e| panic!("{e}"));
    let child = take_child(pty).unwrap();
    (parts, child)
}

#[test]
fn wait_timeout_returns_by_its_timeout() {
    let _s = serial();
    let pty = WindowsPty::default();
    let (parts, child) = live_child(&pty);
    let t = Instant::now();
    assert_eq!(
        child.wait_timeout(Duration::from_millis(200)).unwrap(),
        None
    );
    let e = t.elapsed();
    assert!(
        e >= Duration::from_millis(200) && e < Duration::from_secs(2),
        "{e:?}"
    );
    drop(child);
    assert_eq!(release(parts), Cleanup::Confirmed);
}

#[test]
fn zero_timeout_polls() {
    let _s = serial();
    let pty = WindowsPty::default();
    let (parts, child) = live_child(&pty);
    let t = Instant::now();
    assert_eq!(child.wait_timeout(Duration::ZERO).unwrap(), None);
    assert!(
        t.elapsed() < Duration::from_millis(100),
        "{:?}",
        t.elapsed()
    );
    drop(child);
    let _ = release(parts);
}

#[test]
fn sub_millisecond_timeout_waits() {
    let _s = serial();
    let pty = WindowsPty::default();
    let (parts, child) = live_child(&pty);
    let t = Instant::now();
    assert_eq!(
        child.wait_timeout(Duration::from_micros(500)).unwrap(),
        None
    );
    assert!(
        t.elapsed() >= Duration::from_micros(500),
        "{:?}",
        t.elapsed()
    );
    drop(child);
    let _ = release(parts);
}

#[test]
fn chunked_wait_continues() {
    let _s = serial();
    let pty = WindowsPty::default();
    let (parts, child) = live_child(&pty);
    super::child::CHUNK_CAP_MS.with(|c| c.set(1));
    let t = Instant::now();
    assert_eq!(child.wait_timeout(Duration::from_millis(50)).unwrap(), None);
    assert!(
        t.elapsed() >= Duration::from_millis(50),
        "the wait continued past one chunk"
    );
    sys::terminate_job(child.job(), 1).unwrap();
    let exit = child.wait_timeout(BOUND).unwrap();
    super::child::CHUNK_CAP_MS.with(|c| c.set(u32::MAX - 1));
    assert_eq!(exit.map(|e| e.code), Some(Some(1)));
    drop(child);
    let _ = release(parts);
}

#[test]
fn wait_timeout_runs_during_kill() {
    let _s = serial();
    let pty = WindowsPty::default();
    let (parts, child) = live_child(&pty);
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    *child.in_kill.lock().unwrap() = Some(Box::new(move || {
        let _ = entered_tx.send(());
        let _ = release_rx.lock().unwrap().recv_timeout(BOUND);
    }));
    let killer = {
        let c = child.clone();
        std::thread::spawn(move || c.kill_tree(&[], Instant::now() + Duration::from_secs(20)))
    };
    entered_rx.recv_timeout(BOUND).expect("kill_tree entered");
    let t = Instant::now();
    let _ = child.wait_timeout(Duration::from_millis(10)).unwrap();
    assert!(
        t.elapsed() < Duration::from_millis(200),
        "{:?}",
        t.elapsed()
    );
    release_tx.send(()).unwrap();
    assert!(killer.join().unwrap().tree_dead);
    drop(child);
    let _ = release(parts);
}

#[test]
fn exit_code_is_reported_repeatedly() {
    let _s = serial();
    let pty = WindowsPty::default();
    let (r, _) = spawn(&pty, &cmd("exit 7"));
    let parts = r.unwrap_or_else(|e| panic!("{e}"));
    let a = parts.child.wait_timeout(BOUND).unwrap();
    let b = parts.child.wait_timeout(Duration::ZERO).unwrap();
    assert_eq!(a.map(|e| e.code), Some(Some(7)));
    assert_eq!(a, b);
    drop(take_child(&pty));
    let _ = release(parts);
}

#[test]
fn large_exit_code_is_negative() {
    let _s = serial();
    let pty = WindowsPty::default();
    let (r, _) = spawn(&pty, &cmd("exit -1073741510"));
    let parts = r.unwrap_or_else(|e| panic!("{e}"));
    let e = parts.child.wait_timeout(BOUND).unwrap().unwrap();
    assert_eq!(e.code, Some(0xC000_013A_u32.cast_signed()));
    assert_eq!(e.signal, None);
    drop(take_child(&pty));
    let _ = release(parts);
}

// ---- CL-19 ----

#[test]
fn immediate_exit_still_has_an_identity() {
    let _s = serial();
    let pty = WindowsPty {
        hooks: Hooks {
            stage: Some(Box::new(|s| {
                if let Stage::Spawned { pid } = s {
                    let h = sys::open_process(*pid, super::QUERY_SYNC).unwrap();
                    assert!(sys::wait_floor(h.as_handle(), BOUND).unwrap());
                }
                false
            })),
            ..Hooks::default()
        },
    };
    let (r, _) = spawn(&pty, &cmd("exit 0"));
    let parts = r.unwrap_or_else(|e| panic!("{e}"));
    assert!(!parts.child.identity().start_time.is_empty());
    let e = parts.child.wait_timeout(BOUND).unwrap();
    assert_eq!(e.map(|e| e.code), Some(Some(0)));
    drop(take_child(&pty));
    let _ = release(parts);
}

#[test]
fn leader_identity_matches_identify() {
    let _s = serial();
    let pty = WindowsPty::default();
    let (parts, child) = live_child(&pty);
    let mine = parts.child.identity().clone();
    let got = hooks::identify(mine.pid).unwrap().unwrap();
    assert_eq!((got.pid, &got.start_time), (mine.pid, &mine.start_time));
    assert_eq!(got.group, "");
    assert_eq!(mine.group, format!("job:{}", mine.pid));
    drop(child);
    let _ = release(parts);
}

#[test]
fn identity_failure_has_an_empty_start() {
    let _s = serial();
    let pty = identity_fault();
    let (r, _) = spawn(&pty, &cmd("pause"));
    let (stage, identity, _) = started_then_failed(r);
    assert_eq!((stage, identity), ("identity", None));
    let child = take_child(&pty).unwrap();
    assert_eq!(child.identity().start_time, "");
}

// ---- CL-21 ----

#[test]
fn child_inherits_no_handle() {
    let _s = serial();
    let (r, w) = std::io::pipe().unwrap();
    sys::make_inheritable(&w).unwrap();
    let pty = WindowsPty::default();
    let (parts, child) = live_child(&pty);
    drop(w);
    assert_eq!(
        child.wait_timeout(Duration::ZERO).unwrap(),
        None,
        "the child is alive"
    );
    assert!(
        !sys::pipe_has_writer(&r).unwrap(),
        "the child holds no copy of the write end"
    );
    drop(child);
    let _ = release(parts);
}

#[test]
fn inheriting_child_keeps_the_writer() {
    let _s = serial();
    let (r, w) = std::io::pipe().unwrap();
    sys::make_inheritable(&w).unwrap();
    let mut c = crate::with_spawn_lock(|| {
        Command::new(cmd_exe())
            .args(["/d", "/c", "pause"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    })
    .unwrap();
    drop(w);
    assert!(sys::pipe_has_writer(&r).unwrap(), "a std child inherits it");
    c.kill().unwrap();
    c.wait().unwrap();
    assert!(!sys::pipe_has_writer(&r).unwrap());
}

// ---- CL-23 ----

#[test]
fn pty_spawn_waits_for_a_helper_spawn() {
    let _s = serial();
    let helper_done = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (open_tx, open_rx) = mpsc::channel::<()>();
    let hd = helper_done.clone();
    let helper = std::thread::spawn(move || {
        crate::with_spawn_lock(|| {
            let _ = entered_tx.send(());
            let _ = open_rx.recv_timeout(BOUND);
            hd.store(true, Ordering::Release);
        });
    });
    entered_rx.recv_timeout(BOUND).unwrap();
    let saw: Arc<Mutex<Option<bool>>> = Arc::default();
    let saw2 = saw.clone();
    let hd = helper_done.clone();
    let spawner = std::thread::spawn(move || {
        let pty = WindowsPty {
            hooks: Hooks {
                stage: Some(Box::new(move |s| {
                    if matches!(s, Stage::LockAcquired { stage: 2 }) {
                        saw2.lock()
                            .unwrap()
                            .get_or_insert(hd.load(Ordering::Acquire));
                    }
                    false
                })),
                ..Hooks::default()
            },
        };
        let (r, _) = spawn(&pty, &cmd("exit 0"));
        drop(take_child(&pty));
        r.map(release).map_err(|e| e.to_string())
    });
    // Give the PTY spawn time to reach the lock; it must wait there.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        *saw.lock().unwrap(),
        None,
        "stage 2 ran under the helper's lock"
    );
    open_tx.send(()).unwrap();
    helper.join().unwrap();
    spawner.join().unwrap().unwrap();
    assert_eq!(*saw.lock().unwrap(), Some(true));
}

/// A PTY spawn parked inside the lock at `stage`; a helper spawn must start only after.
fn helper_waits_in(stage: u8) {
    let _s = serial();
    let parked = Arc::new(AtomicBool::new(false));
    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let (open_tx, open_rx) = mpsc::channel::<()>();
    let open_rx = Mutex::new(open_rx);
    let p2 = parked.clone();
    let spawner = std::thread::spawn(move || {
        let pty = WindowsPty {
            hooks: Hooks {
                stage: Some(Box::new(move |s| {
                    if let Stage::LockAcquired { stage: n } = s
                        && *n == stage
                        && !p2.swap(true, Ordering::AcqRel)
                    {
                        let _ = reached_tx.send(());
                        let _ = open_rx.lock().unwrap().recv_timeout(BOUND);
                        p2.store(false, Ordering::Release);
                    }
                    false
                })),
                ..Hooks::default()
            },
        };
        let (r, _) = spawn(&pty, &cmd("exit 0"));
        drop(take_child(&pty));
        r.map(release).map_err(|e| e.to_string())
    });
    reached_rx.recv_timeout(BOUND).unwrap();
    let still_parked = Arc::new(Mutex::new(None));
    let sp = still_parked.clone();
    let p3 = parked.clone();
    let helper = std::thread::spawn(move || {
        crate::with_spawn_lock(|| *sp.lock().unwrap() = Some(p3.load(Ordering::Acquire)));
    });
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        *still_parked.lock().unwrap(),
        None,
        "the helper ran inside stage {stage}"
    );
    open_tx.send(()).unwrap();
    helper.join().unwrap();
    spawner.join().unwrap().unwrap();
    assert_eq!(*still_parked.lock().unwrap(), Some(false));
}

#[test]
fn helper_spawn_waits_in_stage_2() {
    helper_waits_in(2);
}

#[test]
fn helper_spawn_waits_in_stage_5() {
    helper_waits_in(5);
}

#[test]
fn failure_paths_release_the_lock() {
    let _s = serial();
    for fault in [
        WindowsFault::Assign,
        WindowsFault::ResumeFails,
        WindowsFault::DrainStart,
    ] {
        let label = format!("{fault:?}");
        let r = spawn_with(fault, &WindowsPty::default(), &cmd("exit 0"));
        assert!(r.is_err(), "{label}");
        let rep = report();
        let releases = [
            "wait",
            "terminate_process",
            "terminate_job",
            "close",
            "input_drop",
            "drain_finish",
            "job_drop",
            "output_drop",
            "assign",
            "resume",
        ];
        let mut n = 0;
        for e in &rep.trace {
            if releases.contains(&e.name) {
                n += 1;
                assert!(!e.lock_held, "{label}: {e:?}");
            }
        }
        assert!(n > 0, "{label}: {rep:?}");
    }
}

// ---- CL-27 ----

const HELPER_ENV: &str = "CHALDEA_WINDOWS_TEST_HELPER";

/// The helper side: waits for the go line, spawns `pause` through the backend, and checks
/// the case's outcome; a panic fails the helper (exit 101).
fn helper_main(case: &str) {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    let pty = WindowsPty::default();
    let (r, _) = spawn(&pty, &cmd("pause"));
    match case {
        "nests" => {
            let parts = r.unwrap_or_else(|e| panic!("{e}"));
            let ev: std::collections::HashMap<_, _> = parts.evidence.iter().cloned().collect();
            assert_eq!(ev["job_branch"], "job_list");
            let k = parts
                .child
                .kill_tree(&[], Instant::now() + Duration::from_secs(5));
            assert!(k.tree_dead, "{k:?}");
            drop(take_child(&pty));
            let _ = release(parts);
        }
        "refused" => {
            let Err(SpawnError::NeverStarted { kind, .. }) = &r else {
                panic!("{}", describe(&r));
            };
            assert_eq!(*kind, NeverStartedKind::Setup);
            let rep = report();
            let c = rep.entries("create_process");
            assert_eq!(c.len(), 2, "{rep:?}");
            assert!(
                c[0].native && c[0].result.is_err(),
                "the job-list attempt failed: {rep:?}"
            );
            assert!(
                c[1].native && c[1].result.is_ok(),
                "the suspended create ran: {rep:?}"
            );
            let a = rep.entries("assign");
            assert!(
                a[0].native && a[0].result.is_err(),
                "a real assign error: {rep:?}"
            );
            assert_eq!(rep.child_exited, Some(true));
        }
        other => panic!("unknown helper case {other}"),
    }
}

/// The outer side: a kill-on-close parent job (UI-restricted for `refused`) holding the
/// helper and a sibling; the helper runs only after both are verified in the job.
fn parent_job_case(test: &str, case: &str, restricted: bool) {
    let _s = serial();
    let job = sys::create_job().unwrap();
    sys::set_job_limit(&job, sys::KILL_ON_JOB_CLOSE).unwrap();
    if restricted {
        sys::set_ui_restrictions(&job).unwrap();
        assert_ne!(sys::ui_restrictions(&job).unwrap(), 0);
    }
    let (mut helper, mut sibling) = crate::with_spawn_lock(|| {
        let helper = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture", "--test-threads", "1"])
            .env(HELPER_ENV, case)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let sibling = Command::new(cmd_exe())
            .args(["/d", "/c", "pause"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        (helper, sibling)
    });
    let h = helper.as_handle().try_clone_to_owned().unwrap();
    let s = sibling.as_handle().try_clone_to_owned().unwrap();
    let assigned = sys::assign(&job, &h).and_then(|()| sys::assign(&job, &s));
    if let Err(e) = assigned {
        let _ = helper.kill();
        let _ = sibling.kill();
        panic!(
            "setup: cannot put the helper in the test's job ({e}); the runner's own job refuses nesting"
        );
    }
    assert!(
        sys::in_job(&h, &job).unwrap() && sys::in_job(&s, &job).unwrap(),
        "setup: membership"
    );
    helper.stdin.take().unwrap().write_all(b"go\n").unwrap();
    let end = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(st) = helper.try_wait().unwrap() {
            break st;
        }
        assert!(Instant::now() < end, "the helper did not finish");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "helper failed: {status:?}");
    assert!(
        !sys::wait(s.as_handle(), 0).unwrap(),
        "the sibling survived"
    );
    assert!(sys::in_job(&s, &job).unwrap());
    sys::terminate_job(&job, 1).unwrap();
    assert!(sys::wait_floor(s.as_handle(), BOUND).unwrap());
    let _ = sibling.wait();
    drop(sibling.stdin.take());
}

#[test]
fn spawn_inside_a_parent_job_nests() {
    if let Ok(case) = std::env::var(HELPER_ENV) {
        helper_main(&case);
        return;
    }
    parent_job_case(
        "windows::tests::spawn_inside_a_parent_job_nests",
        "nests",
        false,
    );
}

#[test]
fn ui_restricted_parent_job_refuses_nesting() {
    if let Ok(case) = std::env::var(HELPER_ENV) {
        helper_main(&case);
        return;
    }
    parent_job_case(
        "windows::tests::ui_restricted_parent_job_refuses_nesting",
        "refused",
        true,
    );
}

// ---- CL-28 ----

#[test]
fn dropping_the_child_kills_through_the_job() {
    let _s = serial();
    let pty = WindowsPty::default();
    let (parts, child) = live_child(&pty);
    let id = child.identity().clone();
    let watch = open_verified(id.pid, &id.start_time);
    let SpawnedParts {
        child: theirs,
        output,
        input,
        control,
        ..
    } = parts;
    // Drop the only references to the child (and with it the job's only handle) without
    // `kill_tree`, while the control and drain stay alive.
    drop(theirs);
    drop(child);
    assert!(
        sys::wait_floor(watch.as_handle(), BOUND).unwrap(),
        "kill-on-close ended the leader"
    );
    drop(input);
    let _ = control.close(Instant::now() + BOUND);
    let _ = output.finish(BOUND);
}
