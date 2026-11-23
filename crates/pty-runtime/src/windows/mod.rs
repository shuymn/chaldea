//! The Windows backend (`docs/design/pty-windows.md`): a pseudoconsole from the inbox
//! `kernel32.dll` with its close worker already running, the drain started before the
//! child exists, the CLI created inside a kill-on-close job from its creation (job list,
//! else suspended + assign + resume, else no launch), and a tree kill by job termination
//! plus the verified recorded descendants of each call.

mod child;
pub(crate) mod cmdline;
mod control;
#[cfg(any(test, feature = "fake"))]
pub(crate) mod hooks;
mod kill;
pub(crate) mod process;
#[allow(unsafe_code)]
pub(crate) mod sys;
#[cfg(test)]
mod tests;
mod wait;

use std::ffi::{OsStr, OsString};
use std::io::{self, ErrorKind, Write};
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use windows_sys::Win32::System::Threading::{
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

use self::child::{KILL_CODE, WindowsChild};
use self::cmdline::{Buffers, Cwd, EnvError, MAX_COMMAND_LINE};
use self::control::{CloseWorker, WindowsControl};
use crate::backend::{
    Cleanup, CloseReport, DrainSink, KillReport, NeverStartedKind, ProcessIdentity, PtyChild,
    PtyControl, PtyRuntime, SpawnError, SpawnSpec, SpawnedParts, SpawnedPty, TEARDOWN_CLOSE,
    TEARDOWN_DRAIN, sealed,
};
use crate::drain::Drain;
use crate::spawn::spawn_lock;

/// Rights of a handle that can kill and wait.
pub(crate) const KILL_SYNC: u32 =
    PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
/// Rights of a handle that can only wait and read times.
pub(crate) const QUERY_SYNC: u32 = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;

/// How long a failure path waits for the suspended child's exit (stage 5.4).
const SUSPENDED_EXIT_MS: u32 = 2_000;

const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PATH_NOT_FOUND: i32 = 3;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_NOT_ENOUGH_MEMORY: i32 = 8;
const ERROR_NOT_SUPPORTED: i32 = 50;

/// `GetProcessHandleCount` of this process (`fake::process_handle_count`).
#[cfg(feature = "fake")]
pub(crate) fn sys_handle_count() -> Option<u32> {
    sys::process_handle_count().ok()
}

/// The native backend of this OS.
#[must_use]
pub fn native() -> Arc<dyn PtyRuntime> {
    Arc::new(WindowsPty::default())
}

#[derive(Default)]
pub(crate) struct WindowsPty {
    #[cfg(test)]
    pub(crate) hooks: tests::Hooks,
}

impl sealed::Sealed for WindowsPty {}

/// Records a call in this thread's trace (test and `fake` builds only).
fn tr(name: &'static str, native: bool, flags: u32, result: Result<u64, i32>) {
    #[cfg(any(test, feature = "fake"))]
    hooks::trace(name, native, flags, result);
    #[cfg(not(any(test, feature = "fake")))]
    let _ = (name, native, flags, result);
}

/// The trace result of `r`: `value` of a success, the OS code (or -1) of an error.
fn result_of<T>(r: &io::Result<T>, value: impl FnOnce(&T) -> u64) -> Result<u64, i32> {
    match r {
        Ok(v) => Ok(value(v)),
        Err(e) => Err(e.raw_os_error().unwrap_or(-1)),
    }
}

fn res<T>(r: &io::Result<T>) -> Result<u64, i32> {
    result_of(r, |_| 0)
}

/// `Confirmed` when the close returned and the drain ended, else `Unresolved` with no kill.
fn released(close: CloseReport, drained: bool) -> Cleanup {
    if close.elapsed.is_some() && drained {
        Cleanup::Confirmed
    } else {
        Cleanup::Unresolved {
            kill: None,
            close: Some(close),
            drain_abandoned: !drained,
        }
    }
}

fn setup_with(error: io::Error, cleanup: Cleanup) -> SpawnError {
    SpawnError::NeverStarted {
        kind: NeverStartedKind::Setup,
        error,
        cleanup,
    }
}

fn injected(what: &str) -> io::Error {
    io::Error::other(format!("injected {what} failure"))
}

/// The fault points of a spawn; with no fault injected (always outside test and `fake`
/// builds) nothing fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Point {
    FirstPipe,
    SecondPipe,
    CloseWorker,
    PseudoConsole,
    DrainStart,
    JobCreate,
    JobLimits,
    AttrInit,
    PseudoConsoleAttr,
    JobListAttr,
    RejectJobList,
    CreateProcess,
    Assign,
    AssignTerminateDenied,
    ResumeFails,
    ResumeTerminateDenied,
    Identity,
}

/// This spawn's injected fault.
struct Inject {
    #[cfg(any(test, feature = "fake"))]
    fault: Option<hooks::WindowsFault>,
}

impl Inject {
    /// Runs `call`, or fails with `fault()` when `p` is injected, and traces the result.
    fn run<T>(
        &self,
        p: Point,
        name: &'static str,
        fault: impl FnOnce() -> io::Error,
        call: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        let native = !self.fails(p);
        let r = if native { call() } else { Err(fault()) };
        tr(name, native, 0, res(&r));
        r
    }

    fn take() -> Inject {
        Inject {
            #[cfg(any(test, feature = "fake"))]
            fault: hooks::take_next(),
        }
    }

    #[cfg(not(any(test, feature = "fake")))]
    #[allow(
        clippy::unused_self,
        clippy::trivially_copy_pass_by_ref,
        reason = "the same call sites as test builds"
    )]
    fn fails(&self, _p: Point) -> bool {
        false
    }

    #[cfg(any(test, feature = "fake"))]
    fn fails(&self, p: Point) -> bool {
        use hooks::WindowsFault as F;
        let Some(f) = &self.fault else {
            return false;
        };
        match p {
            Point::FirstPipe => matches!(f, F::FirstPipe),
            Point::SecondPipe => matches!(f, F::SecondPipe),
            Point::CloseWorker => matches!(f, F::CloseWorker),
            Point::PseudoConsole => matches!(f, F::PseudoConsole),
            Point::DrainStart => matches!(f, F::DrainStart),
            Point::JobCreate => matches!(f, F::JobCreate),
            Point::JobLimits => matches!(f, F::JobLimits),
            Point::AttrInit => matches!(f, F::AttrInit),
            Point::PseudoConsoleAttr => matches!(f, F::PseudoConsoleAttr),
            Point::JobListAttr => matches!(f, F::JobListAttr),
            Point::RejectJobList => f.rejects_job_list(),
            Point::CreateProcess => matches!(f, F::CreateProcess),
            Point::Assign => matches!(f, F::Assign | F::AssignTerminateDenied),
            Point::AssignTerminateDenied => matches!(f, F::AssignTerminateDenied),
            Point::ResumeFails => matches!(f, F::ResumeFails | F::ResumeTerminateDenied),
            Point::ResumeTerminateDenied => matches!(f, F::ResumeTerminateDenied),
            Point::Identity => matches!(f, F::Identity),
        }
    }

    /// Called by `sys::create_process` at its success boundary, before it returns the
    /// process: records whether the process is already in `job`.
    #[allow(clippy::unused_self, reason = "no report outside test and fake builds")]
    fn member_at_create(&self, process: &OwnedHandle, job: &OwnedHandle) {
        #[cfg(any(test, feature = "fake"))]
        {
            let member = sys::in_job(process, job).ok();
            hooks::with_report(|r| r.member_at_create.push(member));
        }
        #[cfg(not(any(test, feature = "fake")))]
        let _ = (process, job);
    }

    /// The point right after a native create returned a process (lock released), before
    /// anything else can assign it: passes the gate of `AfterCreate` (either branch) or of
    /// `RejectJobList(Some(_))` (the suspended branch).
    #[allow(clippy::unused_self, reason = "no gates outside test and fake builds")]
    fn after_create(&self, suspended: bool) {
        #[cfg(any(test, feature = "fake"))]
        match &self.fault {
            Some(hooks::WindowsFault::AfterCreate(g)) => g.pass(),
            Some(hooks::WindowsFault::RejectJobList(Some(g))) if suspended => g.pass(),
            _ => {}
        }
        #[cfg(not(any(test, feature = "fake")))]
        let _ = suspended;
    }

    /// The previous suspend count to classify: the real one, or the scripted
    /// `ResumeCount` (the real resume ran either way).
    #[allow(
        clippy::unused_self,
        reason = "no scripts outside test and fake builds"
    )]
    fn resume_count(&self, real: io::Result<u32>) -> io::Result<u32> {
        #[cfg(any(test, feature = "fake"))]
        if let (Some(hooks::WindowsFault::ResumeCount(n)), Ok(_)) = (&self.fault, &real) {
            return Ok(*n);
        }
        real
    }

    #[cfg(any(test, feature = "fake"))]
    fn kill_expired(&self) -> bool {
        matches!(self.fault, Some(hooks::WindowsFault::KillExpired))
    }
}

/// The session's input: the input pipe's write end. One `WriteFile` per write on a
/// synchronous pipe returns only when every byte is in the pipe; dropping it writes nothing.
pub(crate) struct Input(std::io::PipeWriter);

impl Write for Input {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        tr("input_drop", true, 0, Ok(0));
    }
}

/// What exists before a process does, released by `release_early`.
struct Early {
    control: WindowsControl,
    input: Input,
    drain: Drain,
    job: Option<OwnedHandle>,
}

impl Early {
    /// `release_early` (never under the lock): close the pseudoconsole while the drain
    /// reads, drop the input, finish the drain, drop the job. The close report and whether
    /// the drain ended.
    fn release(self) -> (CloseReport, bool) {
        let Early {
            control,
            input,
            drain,
            job,
        } = self;
        let close = Box::new(control).close(Instant::now() + TEARDOWN_CLOSE);
        drop(input);
        tr("drain_finish", true, 0, Ok(0));
        let end = drain.finish(TEARDOWN_DRAIN);
        if job.is_some() {
            drop(job);
            tr("job_drop", true, 0, Ok(0));
        }
        (close, end.is_some())
    }

    /// `release`, as the `Cleanup` of a path that left no process.
    fn cleanup(self) -> Cleanup {
        let (close, drained) = self.release();
        released(close, drained)
    }
}

/// A successful stage 5.
struct Launched {
    process: OwnedHandle,
    pid: u32,
    start: io::Result<u64>,
    branch: &'static str,
    job_list_error: Option<String>,
    /// `ResumeThread` returned 0: the thread may have run before its assignment.
    resumed_zero: bool,
}

/// A failed stage 5.
enum NotLaunched {
    /// No process exists (5.3, or a shared attribute).
    NoProcess {
        kind: NeverStartedKind,
        error: io::Error,
    },
    /// The suspended child was terminated (5.4); `exited` when its handle signalled.
    Suspended {
        stage: &'static str,
        error: io::Error,
        identity: ProcessIdentity,
        method: &'static str,
        exited: bool,
    },
}

impl PtyRuntime for WindowsPty {
    #[allow(
        clippy::result_large_err,
        clippy::too_many_lines,
        reason = "spawn is rare; the error carries its cleanup evidence by value; the stages read in order"
    )]
    fn spawn(&self, spec: &SpawnSpec, output: DrainSink) -> Result<SpawnedPty, SpawnError> {
        #[cfg(any(test, feature = "fake"))]
        hooks::start_report();
        let inject = Inject::take();
        let bufs = validate(spec)?;
        #[cfg(test)]
        self.hooks.on(&tests::Stage::Validated);

        // 1. Pipes.
        if inject.fails(Point::FirstPipe) {
            return Err(setup(injected("first pipe")));
        }
        let (in_r, in_w) = std::io::pipe().map_err(setup)?;
        let input = Input(in_w);
        tr("pipe", true, 0, Ok(0));
        if inject.fails(Point::SecondPipe) {
            drop((in_r, input));
            return Err(setup(injected("second pipe")));
        }
        let (out_r, out_w) = std::io::pipe().map_err(setup)?;
        tr("pipe", true, 0, Ok(0));

        // 2.1 The close worker, before the pseudoconsole exists.
        let worker = inject.run(
            Point::CloseWorker,
            "close_worker_start",
            || injected("close worker"),
            || CloseWorker::start("conpty-close"),
        );
        let worker = match worker {
            Ok(w) => w,
            Err(e) => {
                drop((in_r, input, out_r, out_w));
                return Err(setup(e));
            }
        };

        // 2.2 The pseudoconsole, under the spawn lock.
        let pcon = {
            #[cfg(test)]
            self.hooks.on(&tests::Stage::BeforeLock { stage: 2 });
            let guard = spawn_lock();
            #[cfg(test)]
            self.hooks.on(&tests::Stage::LockAcquired { stage: 2 });
            let r = inject.run(
                Point::PseudoConsole,
                "create_pseudo_console",
                || injected("pseudoconsole"),
                || sys::create_pseudo_console(&guard, spec.size, &in_r, &out_w),
            );
            #[cfg(test)]
            self.hooks.on(&tests::Stage::BeforeUnlock { stage: 2 });
            drop(guard);
            r
        };
        let pcon = match pcon {
            Ok(p) => p,
            Err(e) => {
                let acknowledged = worker.close(Instant::now() + TEARDOWN_CLOSE);
                tr("worker_close", true, 0, Ok(0));
                drop((in_r, input, out_r, out_w));
                let close = CloseReport {
                    elapsed: acknowledged,
                };
                return Err(setup_with(e, released(close, true)));
            }
        };
        #[cfg_attr(not(any(test, feature = "fake")), allow(unused_mut))]
        let mut control = WindowsControl::new(pcon, worker);
        // conhost holds its own ends: it is now the output pipe's last writer.
        drop((in_r, out_w));
        let conpty_module = sys::module_path("kernel32.dll")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        // 3. The drain, before any process exists.
        #[cfg(any(test, feature = "fake"))]
        let held = match &inject.fault {
            Some(hooks::WindowsFault::AfterSpawn { probe, .. }) => {
                let m = probe.marker();
                hooks::hold_until_teardown(m.clone());
                Some(m)
            }
            Some(hooks::WindowsFault::HoldOutput(probe)) => {
                let m = probe.marker();
                control.hold_until_close(m.clone());
                Some(m)
            }
            _ => None,
        };
        #[cfg(any(test, feature = "fake"))]
        let reader = {
            let marker = held.clone();
            #[cfg(test)]
            let marker =
                marker.or_else(|| self.hooks.probe.as_ref().map(hooks::OutputProbe::marker));
            hooks::MarkerReader::new(out_r, marker)
        };
        #[cfg(not(any(test, feature = "fake")))]
        let reader = out_r;
        let drain = if inject.fails(Point::DrainStart) {
            drop(reader);
            tr("output_drop", true, 0, Ok(0));
            Err(injected("drain start"))
        } else {
            Drain::start(reader, output)
        };
        tr(
            "drain_start",
            !inject.fails(Point::DrainStart),
            0,
            res(&drain),
        );
        let drain = match drain {
            Ok(d) => d,
            Err(e) => {
                let close = Box::new(control).close(Instant::now() + TEARDOWN_CLOSE);
                drop(input);
                return Err(setup_with(e, released(close, true)));
            }
        };
        let mut early = Early {
            control,
            input,
            drain,
            job: None,
        };

        // 4. The job.
        let job = match create_job(&inject) {
            Ok(j) => j,
            Err((e, job)) => {
                early.job = job;
                return Err(setup_with(e, early.cleanup()));
            }
        };

        // 5. Create.
        let created = self.create(spec, &bufs, &early.control, &job, &inject);
        let launched = match created {
            Ok(l) => l,
            Err(NotLaunched::NoProcess { kind, error }) => {
                early.job = Some(job);
                return Err(SpawnError::NeverStarted {
                    kind,
                    error,
                    cleanup: early.cleanup(),
                });
            }
            Err(NotLaunched::Suspended {
                stage,
                error,
                identity,
                method,
                exited,
            }) => {
                early.job = Some(job);
                let (close, drained) = early.release();
                if exited {
                    return Err(setup_with(error, released(close, drained)));
                }
                return Err(SpawnError::StartedThenFailed {
                    stage,
                    error,
                    identity: Some(identity.clone()),
                    cleanup: Cleanup::Unresolved {
                        kill: Some(KillReport {
                            method: method.to_owned(),
                            tree_dead: false,
                            survivors: vec![identity],
                        }),
                        close: Some(close),
                        drain_abandoned: !drained,
                    },
                });
            }
        };

        // 6. Settle.
        let pid = launched.pid;
        #[cfg(test)]
        let fail_identity = self.hooks.on(&tests::Stage::Spawned { pid });
        #[cfg(not(test))]
        let fail_identity = false;
        let start = if fail_identity {
            Err(injected("identity"))
        } else {
            launched.start
        };
        let Early {
            control,
            input,
            drain,
            job: _,
        } = early;
        let child = Arc::new(WindowsChild::new(
            launched.process,
            job,
            pid,
            start.as_ref().ok().copied(),
        ));
        #[cfg(any(test, feature = "fake"))]
        if inject.kill_expired() {
            child
                .kill_expired
                .store(true, std::sync::atomic::Ordering::Release);
        }
        #[cfg(test)]
        self.hooks.keep(child.clone());
        let identity = child.identity().clone();
        let mut evidence = vec![
            ("job_branch", launched.branch.to_owned()),
            ("conpty_module", conpty_module),
        ];
        if let Some(e) = launched.job_list_error {
            evidence.push(("job_list_error", e));
        }
        let parts = SpawnedParts {
            child: child.clone(),
            output: drain,
            input: Box::new(input),
            control: Box::new(control),
            size: spec.size,
            evidence,
        };
        if let Err(error) = start {
            return Err(SpawnError::StartedThenFailed {
                stage: "identity",
                error,
                identity: None,
                cleanup: parts.teardown(),
            });
        }
        if launched.resumed_zero {
            return Err(SpawnError::StartedThenFailed {
                stage: "resume",
                error: io::Error::other("ResumeThread returned 0: the thread was not suspended"),
                identity: Some(identity),
                cleanup: parts.teardown(),
            });
        }
        #[cfg(any(test, feature = "fake"))]
        if let (Some(hooks::WindowsFault::AfterSpawn { gate, .. }), Some(marker)) =
            (&inject.fault, &held)
        {
            let error = after_spawn(marker, &child, gate);
            return Err(SpawnError::StartedThenFailed {
                stage: "after_spawn",
                error,
                identity: Some(identity),
                cleanup: parts.teardown(),
            });
        }
        Ok(SpawnedPty::new(parts))
    }
}

/// `AfterSpawn`: waits for the marker with the child running (the reader then holds the
/// drain), passes the gate, and returns the error to fail with.
#[cfg(any(test, feature = "fake"))]
fn after_spawn(marker: &hooks::Marker, child: &WindowsChild, gate: &hooks::Gate) -> io::Error {
    let seen = marker.wait_seen(Instant::now() + hooks::HOOK_WAIT);
    let running = matches!(child.wait_timeout(std::time::Duration::ZERO), Ok(None));
    let reached = seen && running;
    hooks::with_report(|r| r.marker_seen_running = Some(reached));
    if !reached {
        return io::Error::other("milestone not reached");
    }
    gate.pass();
    io::Error::other("injected after-spawn failure")
}

fn setup(error: io::Error) -> SpawnError {
    setup_with(error, Cleanup::Confirmed)
}

fn invalid(msg: &str) -> SpawnError {
    setup(io::Error::new(ErrorKind::InvalidInput, msg.to_owned()))
}

/// The NUL-terminated current directory passed to `CreateProcessW` for `cwd`: without its
/// verbatim prefix when that names the same directory (`cmdline::verbatim_cwd`); `None` when
/// the stripped form would name another one.
pub(crate) fn process_cwd(cwd: &std::path::Path) -> Option<Vec<u16>> {
    let given = cmdline::wide(cwd.as_os_str());
    let absolute = |u: &[u16]| -> io::Result<Vec<u16>> {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let a = std::path::absolute(std::path::PathBuf::from(OsString::from_wide(u)))?;
        Ok(a.as_os_str().encode_wide().collect())
    };
    match cmdline::verbatim_cwd(&given, absolute) {
        Cwd::AsGiven => Some(nul_terminated(given)),
        Cwd::Stripped(s) => Some(nul_terminated(s)),
        Cwd::Refused => None,
    }
}

fn nul_terminated(mut units: Vec<u16>) -> Vec<u16> {
    units.push(0);
    units
}

fn has_nul(s: &OsStr) -> bool {
    cmdline::wide(s).contains(&0)
}

/// The checks of §2.2, before anything is created; they also build the buffers.
#[allow(
    clippy::result_large_err,
    reason = "spawn is rare; the error carries its cleanup evidence by value"
)]
fn validate(spec: &SpawnSpec) -> Result<Buffers, SpawnError> {
    let Some(argv0) = spec.argv.first() else {
        return Err(invalid("argv is empty"));
    };
    if spec.argv.iter().any(|a| has_nul(a)) {
        return Err(invalid("NUL in argv"));
    }
    let program = Path::new(argv0);
    if !program.is_absolute() || cmdline::wide(argv0).contains(&u16::from(b'"')) {
        return Err(invalid("argv[0] is not absolute or contains a quote"));
    }
    if program
        .extension()
        .is_some_and(|x| x.eq_ignore_ascii_case("bat") || x.eq_ignore_ascii_case("cmd"))
    {
        return Err(invalid(
            "argv[0] is a batch file; pass the native executable it runs",
        ));
    }
    let meta = std::fs::metadata(program).map_err(|e| {
        let kind = match e.kind() {
            ErrorKind::NotFound => NeverStartedKind::NotFound,
            ErrorKind::PermissionDenied => NeverStartedKind::PermissionDenied,
            _ => NeverStartedKind::Setup,
        };
        SpawnError::NeverStarted {
            kind,
            error: e,
            cleanup: Cleanup::Confirmed,
        }
    })?;
    if !meta.is_file() {
        return Err(SpawnError::NeverStarted {
            kind: NeverStartedKind::PermissionDenied,
            error: io::Error::new(ErrorKind::PermissionDenied, "argv[0] is not a regular file"),
            cleanup: Cleanup::Confirmed,
        });
    }
    if !spec.cwd.is_absolute() {
        return Err(invalid("cwd is not absolute"));
    }
    match std::fs::metadata(&spec.cwd) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return Err(setup(io::Error::new(
                ErrorKind::NotADirectory,
                "cwd is not a directory",
            )));
        }
        Err(e) => return Err(setup(e)),
    }
    let Some(cwd) = process_cwd(&spec.cwd) else {
        return Err(invalid(
            "verbatim cwd names another directory without its prefix",
        ));
    };
    let env = match cmdline::env_block(&spec.env, sys::compare_ordinal_ignore_case) {
        Ok(env) => env,
        Err(EnvError::Invalid) => return Err(invalid("invalid environment entry")),
        Err(EnvError::Duplicate) => return Err(invalid("duplicate environment key")),
    };
    let line = cmdline::command_line(&spec.argv);
    if line.len() > MAX_COMMAND_LINE {
        return Err(invalid("command line longer than 32767 units"));
    }
    Ok(Buffers {
        app: cmdline::wide_nul(argv0),
        cmdline: line,
        env,
        cwd,
    })
}

/// Stage 4: `CreateJobObjectW`, then the kill-on-close limit and nothing else. On a limit
/// failure the job comes back with the error, for `release_early` to drop last.
fn create_job(inject: &Inject) -> Result<OwnedHandle, (io::Error, Option<OwnedHandle>)> {
    let job = inject
        .run(
            Point::JobCreate,
            "job_create",
            || injected("job create"),
            sys::create_job,
        )
        .map_err(|e| (e, None))?;
    let limit = inject.run(
        Point::JobLimits,
        "job_limits",
        || injected("job limits"),
        || sys::set_job_limit(&job, sys::KILL_ON_JOB_CLOSE),
    );
    match limit {
        Ok(()) => Ok(job),
        Err(e) => Err((e, Some(job))),
    }
}

/// The `NeverStarted` kind of a create error (§2.2 stage 5.3).
fn kind_of(e: &io::Error, program: &Path) -> NeverStartedKind {
    match e.raw_os_error() {
        Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) if matches!(std::fs::metadata(program), Err(m) if m.kind() == ErrorKind::NotFound) => {
            NeverStartedKind::NotFound
        }
        Some(ERROR_ACCESS_DENIED) => NeverStartedKind::PermissionDenied,
        _ => NeverStartedKind::Setup,
    }
}

/// Stage 5.3: no process exists; the error keeps `msg` and takes the kind of `e`.
fn no_process(e: &io::Error, program: &Path, msg: String) -> NotLaunched {
    let kind = kind_of(e, program);
    let io_kind = match kind {
        NeverStartedKind::NotFound => ErrorKind::NotFound,
        NeverStartedKind::PermissionDenied => ErrorKind::PermissionDenied,
        NeverStartedKind::Setup => e.kind(),
    };
    NotLaunched::NoProcess {
        kind,
        error: io::Error::new(io_kind, msg),
    }
}

impl WindowsPty {
    /// One `CreateProcessW` attempt with a fresh copy of the command line, under the spawn
    /// lock; `synthesized` replaces the call with that error. A created process reaches
    /// `after_create` right after the lock is released, before any assignment.
    fn attempt(
        &self,
        bufs: &Buffers,
        attrs: &mut sys::AttrList<'_>,
        suspended: bool,
        synthesized: Option<i32>,
        (job, inject): (&OwnedHandle, &Inject),
    ) -> io::Result<sys::Created> {
        let mut line = bufs.cmdline.clone();
        #[cfg(test)]
        self.hooks.command_line(suspended, &mut line);
        let guard = spawn_lock();
        #[cfg(test)]
        self.hooks.on(&tests::Stage::LockAcquired { stage: 5 });
        let r = match synthesized {
            Some(code) => Err(io::Error::from_raw_os_error(code)),
            None => sys::create_process(
                &guard,
                &bufs.app,
                &mut line,
                &bufs.env,
                &bufs.cwd,
                attrs,
                suspended,
                &|p| inject.member_at_create(p, job),
            ),
        };
        tr(
            "create_process",
            synthesized.is_none(),
            sys::creation_flags(suspended),
            result_of(&r, |c| u64::from(c.pid)),
        );
        #[cfg(test)]
        self.hooks.on(&tests::Stage::BeforeUnlock { stage: 5 });
        drop(guard);
        if r.is_ok() {
            inject.after_create(suspended);
        }
        r
    }

    /// Stage 5: the job-list attempt, else the suspended attempt once.
    fn create(
        &self,
        spec: &SpawnSpec,
        bufs: &Buffers,
        control: &WindowsControl,
        job: &OwnedHandle,
        inject: &Inject,
    ) -> Result<Launched, NotLaunched> {
        let program = Path::new(&spec.argv[0]);
        let Some(pcon) = control.pseudo() else {
            // Unreachable: the control is closed only by `release`.
            return Err(NotLaunched::NoProcess {
                kind: NeverStartedKind::Setup,
                error: io::Error::other("the pseudoconsole is closed"),
            });
        };
        let jobs = sys::job_array(job);

        // 5.1 The job-list attempt.
        let first: Result<sys::Created, (&'static str, io::Error, bool)> = (|| {
            let mut attrs = if inject.fails(Point::AttrInit) {
                Err(injected("attribute list"))
            } else {
                sys::AttrList::new(2)
            }
            .map_err(|e| ("attr_init", e, true))?;
            if inject.fails(Point::PseudoConsoleAttr) {
                return Err((
                    "set_pseudoconsole",
                    injected("pseudoconsole attribute"),
                    true,
                ));
            }
            attrs
                .set_pseudoconsole(pcon)
                .map_err(|e| ("set_pseudoconsole", e, true))?;
            let list = if inject.fails(Point::JobListAttr) {
                Err(injected("job list attribute"))
            } else {
                attrs.set_job_list(&jobs)
            };
            list.map_err(|e| ("set_job_list", e, false))?;
            let synthesized = (inject.fails(Point::RejectJobList)
                || inject.fails(Point::CreateProcess))
            .then_some(ERROR_NOT_SUPPORTED);
            self.attempt(bufs, &mut attrs, false, synthesized, (job, inject))
                .map_err(|e| ("create_process", e, false))
        })();
        let (created, branch, job_list_error) = match first {
            Ok(c) => (c, "job_list", None),
            Err((stage, e, true)) => {
                // A shared attribute failed: no retry.
                let msg = format!("job_list: {stage}: {e}");
                return Err(no_process(&e, program, msg));
            }
            Err((stage1, e1, false)) => {
                // 5.2 The suspended attempt, once.
                let second: Result<sys::Created, (&'static str, io::Error)> = (|| {
                    let mut attrs = sys::AttrList::new(1).map_err(|e| ("attr_init", e))?;
                    attrs
                        .set_pseudoconsole(pcon)
                        .map_err(|e| ("set_pseudoconsole", e))?;
                    let synthesized = inject
                        .fails(Point::CreateProcess)
                        .then_some(ERROR_NOT_ENOUGH_MEMORY);
                    self.attempt(bufs, &mut attrs, true, synthesized, (job, inject))
                        .map_err(|e| ("create_process", e))
                })();
                match second {
                    Ok(c) => (c, "suspended", Some(format!("{stage1}: {e1}"))),
                    Err((stage2, e2)) => {
                        // 5.3 No process.
                        let msg = format!("job_list: {stage1}: {e1}; suspended: {stage2}: {e2}");
                        return Err(no_process(&e2, program, msg));
                    }
                }
            }
        };
        let sys::Created {
            process,
            thread,
            pid,
        } = created;
        // The start, right after the create and before any assignment.
        let start = inject.run(
            Point::Identity,
            "creation_time",
            || injected("identity"),
            || sys::creation_time(process.as_handle()),
        );
        let suspended = branch == "suspended";
        if !suspended {
            drop(thread);
            return Ok(Launched {
                process,
                pid,
                start,
                branch,
                job_list_error,
                resumed_zero: false,
            });
        }

        // The suspended branch: assign, then resume.
        let prefix = job_list_error.clone().unwrap_or_default();
        let unassigned = |start: Option<u64>| ProcessIdentity {
            pid,
            start_time: start.map(|s| s.to_string()).unwrap_or_default(),
            group: String::new(),
        };
        let start = match start {
            Ok(s) => s,
            Err(e) => {
                drop(thread);
                let exited = self.terminate_suspended(&process, pid, inject);
                return Err(NotLaunched::Suspended {
                    stage: "identity",
                    error: io::Error::other(format!(
                        "job_list: {prefix}; suspended: identity: {e}"
                    )),
                    identity: unassigned(None),
                    method: "windows_process",
                    exited,
                });
            }
        };
        let assigned = inject.run(
            Point::Assign,
            "assign",
            || io::Error::from_raw_os_error(ERROR_NOT_SUPPORTED),
            || sys::assign(job, &process),
        );
        if let Err(e) = assigned {
            drop(thread);
            let exited = self.terminate_suspended(&process, pid, inject);
            return Err(NotLaunched::Suspended {
                stage: "job_assign",
                error: io::Error::new(
                    e.kind(),
                    format!("job_list: {prefix}; suspended: assign: {e}"),
                ),
                identity: unassigned(Some(start)),
                method: "windows_process",
                exited,
            });
        }
        let resumed = if inject.fails(Point::ResumeFails) {
            Err(io::Error::from_raw_os_error(ERROR_NOT_SUPPORTED))
        } else {
            inject.resume_count(sys::resume(&thread))
        };
        tr(
            "resume",
            !inject.fails(Point::ResumeFails),
            0,
            result_of(&resumed, |n| u64::from(*n)),
        );
        drop(thread);
        match resumed {
            Ok(n @ (0 | 1)) => Ok(Launched {
                process,
                pid,
                start: Ok(start),
                branch,
                job_list_error,
                resumed_zero: n == 0,
            }),
            other => {
                let e = match other {
                    Err(e) => e,
                    Ok(n) => io::Error::other(format!("ResumeThread returned {n}")),
                };
                // The job owns the cleanup from here.
                let terminated = if inject.fails(Point::ResumeTerminateDenied) {
                    #[cfg(any(test, feature = "fake"))]
                    let r = sys::duplicate_with_access(job, sys::JOB_OBJECT_QUERY)
                        .and_then(|d| sys::terminate_job(&d, KILL_CODE));
                    #[cfg(not(any(test, feature = "fake")))]
                    let r = sys::terminate_job(job, KILL_CODE);
                    r
                } else {
                    sys::terminate_job(job, KILL_CODE)
                };
                tr("terminate_job", true, 0, res(&terminated));
                let exited = Self::wait_suspended(&process, pid, self);
                Err(NotLaunched::Suspended {
                    stage: "resume",
                    error: io::Error::new(
                        e.kind(),
                        format!("job_list: {prefix}; suspended: resume: {e}"),
                    ),
                    identity: ProcessIdentity {
                        pid,
                        start_time: start.to_string(),
                        group: format!("job:{pid}"),
                    },
                    method: "windows_job",
                    exited,
                })
            }
        }
    }

    /// Terminates an unassigned suspended child through its handle; true when the handle
    /// signals within the bound, whether or not the termination call succeeded.
    fn terminate_suspended(&self, process: &OwnedHandle, pid: u32, inject: &Inject) -> bool {
        let r = if inject.fails(Point::AssignTerminateDenied) {
            #[cfg(any(test, feature = "fake"))]
            let r = sys::duplicate_with_access(process, QUERY_SYNC)
                .and_then(|d| sys::terminate_process(&d, KILL_CODE));
            #[cfg(not(any(test, feature = "fake")))]
            let r = sys::terminate_process(process, KILL_CODE);
            r
        } else {
            sys::terminate_process(process, KILL_CODE)
        };
        tr("terminate_process", true, 0, res(&r));
        Self::wait_suspended(process, pid, self)
    }

    /// The bounded wait on the suspended child's handle (without the lock).
    fn wait_suspended(process: &OwnedHandle, pid: u32, pty: &WindowsPty) -> bool {
        #[cfg(test)]
        pty.hooks.on(&tests::Stage::BeforeSuspendedWait { pid });
        #[cfg(not(test))]
        let _ = (pid, pty);
        let exited = sys::wait(process.as_handle(), SUSPENDED_EXIT_MS).unwrap_or(false);
        tr("wait", true, 0, Ok(u64::from(exited)));
        #[cfg(any(test, feature = "fake"))]
        hooks::with_report(|r| r.child_exited = Some(exited));
        exited
    }
}
