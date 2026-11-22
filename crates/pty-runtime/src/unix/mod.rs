//! The macOS backend (`docs/design/pty-unix.md`): a rustix PTY, the CLI started by
//! `std::process::Command` with a minimal `pre_exec` under the process-wide spawn lock,
//! a nonblocking master, and a tree kill over libproc.

mod child;
mod io;
mod kill;
#[allow(unsafe_code)]
mod sys;
mod table;
#[cfg(test)]
mod tests;

use std::ffi::{OsStr, OsString};
use std::io::{Error, ErrorKind};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use rustix::fs::{Access, Mode, OFlags};
use rustix::pty::OpenptFlags;

use self::child::UnixChild;
use self::io::{MasterReader, MasterWriter, UnixControl, Wake, winsize};
use crate::backend::{
    Cleanup, DrainSink, NeverStartedKind, PtyRuntime, SpawnError, SpawnSpec, SpawnedParts,
    SpawnedPty, TEARDOWN_DRAIN, sealed,
};
use crate::drain::Drain;
use crate::spawn::spawn_lock;

#[cfg(any(test, feature = "fake"))]
pub(crate) use self::io::WRITER_POLLS;

/// The native backend of this OS.
#[must_use]
pub fn native() -> Arc<dyn PtyRuntime> {
    Arc::new(UnixPty::default())
}

/// Process identity by pid, for tests (`fake::identify`).
#[cfg(feature = "fake")]
pub(crate) fn identify(pid: u32) -> std::io::Result<Option<crate::ProcessIdentity>> {
    use self::table::Lookup;
    match table::lookup(pid) {
        Lookup::Live(p) => Ok(Some(crate::ProcessIdentity {
            pid,
            start_time: p.start.to_string(),
            group: p.pgid.to_string(),
        })),
        Lookup::Zombie(start) => Ok(Some(crate::ProcessIdentity {
            pid,
            start_time: start.to_string(),
            group: String::new(),
        })),
        Lookup::Gone => Ok(None),
        Lookup::Unreadable => Err(Error::from(ErrorKind::PermissionDenied)),
    }
}

#[derive(Default)]
pub(crate) struct UnixPty {
    #[cfg(test)]
    hooks: tests::Hooks,
}

impl sealed::Sealed for UnixPty {}

impl PtyRuntime for UnixPty {
    #[allow(
        clippy::result_large_err,
        reason = "spawn is rare; the error carries its cleanup evidence by value"
    )]
    fn spawn(&self, spec: &SpawnSpec, output: DrainSink) -> Result<SpawnedPty, SpawnError> {
        self.spawn_inner(spec, output)
    }
}

fn setup(error: Error) -> SpawnError {
    SpawnError::NeverStarted {
        kind: NeverStartedKind::Setup,
        error,
        cleanup: Cleanup::Confirmed,
    }
}

fn invalid(msg: &str) -> SpawnError {
    setup(Error::new(ErrorKind::InvalidInput, msg.to_owned()))
}

fn has_nul(s: &OsStr) -> bool {
    s.as_bytes().contains(&0)
}

/// The checks of §2.2, before anything is created.
#[allow(
    clippy::result_large_err,
    reason = "spawn is rare; the error carries its cleanup evidence by value"
)]
fn validate(spec: &SpawnSpec) -> Result<(), SpawnError> {
    let Some(argv0) = spec.argv.first() else {
        return Err(invalid("argv is empty"));
    };
    if spec.argv.iter().any(|a| has_nul(a)) {
        return Err(invalid("NUL in argv"));
    }
    let program = Path::new(argv0);
    if !program.is_absolute() {
        return Err(invalid("argv[0] is not absolute"));
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
    let exec_ok = rustix::fs::access(program, Access::EXEC_OK).is_ok();
    if !meta.file_type().is_file() || meta.file_type().is_fifo() || !exec_ok {
        return Err(SpawnError::NeverStarted {
            kind: NeverStartedKind::PermissionDenied,
            error: Error::new(
                ErrorKind::PermissionDenied,
                "argv[0] is not an executable file",
            ),
            cleanup: Cleanup::Confirmed,
        });
    }
    if !spec.cwd.is_absolute() {
        return Err(invalid("cwd is not absolute"));
    }
    match std::fs::metadata(&spec.cwd) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return Err(setup(Error::new(
                ErrorKind::NotADirectory,
                "cwd is not a directory",
            )));
        }
        Err(e) => return Err(setup(e)),
    }
    let mut keys: Vec<&OsString> = Vec::with_capacity(spec.env.len());
    for (k, v) in &spec.env {
        if k.is_empty() || k.as_bytes().contains(&b'=') || has_nul(k) || has_nul(v) {
            return Err(invalid("invalid environment entry"));
        }
        keys.push(k);
    }
    keys.sort();
    if keys.windows(2).any(|w| w[0] == w[1]) {
        return Err(invalid("duplicate environment key"));
    }
    Ok(())
}

/// The kind of a failure the forked child reported (§2.2 stage 4).
fn kind_of(e: &Error, program: &Path) -> NeverStartedKind {
    match e.raw_os_error() {
        // `metadata` follows symlinks: a link whose target vanished is a missing program.
        Some(libc::ENOENT) if std::fs::metadata(program).is_err() => NeverStartedKind::NotFound,
        Some(libc::EACCES) => NeverStartedKind::PermissionDenied,
        _ => NeverStartedKind::Setup,
    }
}

/// The PTY ends after stage 2.
struct Ends {
    slave: OwnedFd,
    reader: MasterReader,
    input: MasterWriter,
    control: UnixControl,
}

impl UnixPty {
    fn open_ends(&self, spec: &SpawnSpec) -> std::io::Result<Ends> {
        // Stage 1: close-on-exec from creation (rustix declares CLOEXEC only for Linux and BSDs;
        // macOS honours the bit).
        let flags = OpenptFlags::RDWR
            | OpenptFlags::NOCTTY
            | OpenptFlags::from_bits_retain(libc::O_CLOEXEC.cast_unsigned());
        let master = rustix::pty::openpt(flags)?;
        rustix::pty::grantpt(&master)?;
        rustix::pty::unlockpt(&master)?;
        let name = rustix::pty::ptsname(&master, Vec::new())?;
        let fl = rustix::fs::fcntl_getfl(&master)?;
        rustix::fs::fcntl_setfl(&master, fl | OFlags::NONBLOCK)?;
        // Stage 2: the slave first; the size needs it open.
        let slave = rustix::fs::open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        rustix::termios::tcsetwinsize(&master, winsize(spec.size))?;
        let dup = || rustix::io::fcntl_dupfd_cloexec(&master, 0);
        let (r, w, c) = (dup()?, dup()?, dup()?);
        drop(master);
        let (wake_r, wake_w) = {
            let _g = spawn_lock();
            #[cfg(test)]
            self.hooks.on(&tests::Stage::WakeUnderLock);
            std::io::pipe()?
        };
        let fl = rustix::fs::fcntl_getfl(&wake_w)?;
        rustix::fs::fcntl_setfl(&wake_w, fl | OFlags::NONBLOCK)?;
        let wake = Wake::new(wake_r);
        Ok(Ends {
            slave,
            reader: MasterReader::new(r, wake.clone()),
            input: MasterWriter::new(w, wake.clone()),
            control: UnixControl::new(c, wake, wake_w),
        })
    }

    #[allow(
        clippy::result_large_err,
        reason = "spawn is rare; the error carries its cleanup evidence by value"
    )]
    fn spawn_inner(&self, spec: &SpawnSpec, output: DrainSink) -> Result<SpawnedPty, SpawnError> {
        validate(spec)?;
        #[cfg(test)]
        self.hooks.on(&tests::Stage::Validated);
        let Ends {
            slave,
            reader,
            input,
            control,
        } = self.open_ends(spec).map_err(setup)?;

        // Stage 3.
        #[cfg(test)]
        let drain = if self.hooks.fail_drain_start {
            drop(reader);
            Err(Error::other("drain start failed (test)"))
        } else {
            Drain::start(reader, output)
        };
        #[cfg(not(test))]
        let drain = Drain::start(reader, output);
        let drain = match drain {
            Ok(d) => d,
            Err(e) => {
                drop((input, control, slave));
                return Err(setup(e));
            }
        };

        // Stage 4.
        let program = Path::new(&spec.argv[0]);
        let spawned = self.start_child(spec, &slave);
        let pid = match spawned {
            Ok(pid) => pid,
            Err(e) => {
                drop(slave);
                drop(input);
                let close = Box::new(control).close_now();
                let end = drain.finish(TEARDOWN_DRAIN);
                let cleanup = if end.is_some() {
                    Cleanup::Confirmed
                } else {
                    Cleanup::Unresolved {
                        kill: None,
                        close: Some(close),
                        drain_abandoned: true,
                    }
                };
                return Err(SpawnError::NeverStarted {
                    kind: kind_of(&e, program),
                    error: e,
                    cleanup,
                });
            }
        };

        // Stage 5.
        drop(slave);
        #[cfg(test)]
        let fail = self.hooks.on(&tests::Stage::Spawned { pid });
        #[cfg(not(test))]
        let fail = false;
        let start = if fail {
            Err(Error::other("identity read failed (test)"))
        } else {
            sys::bsd_info(pid)
                .map(|i| i.start)
                .map_err(Error::from_raw_os_error)
        };
        let child = Arc::new(UnixChild::new(pid, start.as_ref().ok().copied()));
        #[cfg(test)]
        {
            if self.hooks.kill_expired {
                child
                    .kill_expired
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            *self
                .hooks
                .child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(child.clone());
        }
        let parts = SpawnedParts {
            child,
            output: drain,
            input: Box::new(input),
            control: Box::new(control),
            size: spec.size,
            evidence: Vec::new(),
        };
        match start {
            Ok(_) => Ok(SpawnedPty::new(parts)),
            Err(error) => Err(SpawnError::StartedThenFailed {
                stage: "identity",
                error,
                identity: None,
                cleanup: parts.teardown(),
            }),
        }
    }

    /// Stage 4: the `Command` under the spawn lock; the pid on success.
    fn start_child(&self, spec: &SpawnSpec, slave: &OwnedFd) -> std::io::Result<u32> {
        let stdio = || -> std::io::Result<Stdio> {
            #[cfg(test)]
            if self.hooks.stdio_dev_null {
                return Ok(Stdio::null());
            }
            Ok(Stdio::from(slave.try_clone()?))
        };
        let mut cmd = Command::new(&spec.argv[0]);
        cmd.args(&spec.argv[1..])
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&spec.cwd)
            .stdin(stdio()?)
            .stdout(stdio()?)
            .stderr(stdio()?);
        let bound = sys::fd_ceiling()?;
        sys::install_pre_exec(
            &mut cmd,
            bound,
            #[cfg(test)]
            self.hooks.pre_exec,
        );
        let child = {
            let _g = spawn_lock();
            #[cfg(test)]
            self.hooks.on(&tests::Stage::LockAcquired);
            cmd.spawn()?
        };
        // std never waits for it; the `UnixChild` owns its wait status from here.
        Ok(child.id())
    }
}

impl UnixControl {
    /// `close` with no meaningful deadline: it never blocks.
    fn close_now(self: Box<Self>) -> crate::backend::CloseReport {
        crate::backend::PtyControl::close(self, std::time::Instant::now())
    }
}

/// A per-thread release trace (tests: CL-3).
#[cfg(test)]
pub(crate) mod trace {
    use std::cell::RefCell;

    thread_local! {
        static TRACE: RefCell<Option<Vec<&'static str>>> = const { RefCell::new(None) };
    }

    pub(crate) fn start() {
        TRACE.with(|t| *t.borrow_mut() = Some(Vec::new()));
    }

    pub(crate) fn take() -> Vec<&'static str> {
        TRACE.with(|t| t.borrow_mut().take().unwrap_or_default())
    }

    pub(crate) fn record(point: &'static str) {
        TRACE.with(|t| {
            if let Some(v) = t.borrow_mut().as_mut() {
                v.push(point);
            }
        });
    }
}
