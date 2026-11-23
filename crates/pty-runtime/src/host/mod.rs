//! Process-level OS primitives of the daemon and the CLI (`docs/design/daemon-lifecycle.md`
//! §2.2): process identity, the process-wide signal state, detaching (sessions, the macOS
//! launchd job, Windows creation flags), the readiness channel, the state directory's privacy
//! checks and owner-only files, opens that never follow a final link, the Windows parent job,
//! the stop channel, and the browser launch.
//!
//! Every function is safe; the `unsafe` calls are in `unix::sys` and `windows::sys`, the only
//! modules allowed them (`docs/design/pty-unix.md` §2.7).

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use crate::backend::ProcessIdentity;

#[cfg(target_os = "macos")]
#[path = "unix.rs"]
mod macos;
#[cfg(not(any(target_os = "macos", windows)))]
mod other;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(not(any(target_os = "macos", windows)))]
use other as imp;
#[cfg(windows)]
use windows as imp;

// ---- Exit ----

/// Ends this process at once with `code`: no exit handler, static destructor, stdio flush,
/// or DLL detach runs (`_exit` on macOS; `TerminateProcess` of the current process on
/// Windows). For a process with nothing to flush whose exit must not wait on anything
/// (`chaldea hook-emit`, docs/design/hook-ingest.md §2.13).
pub fn exit_now(code: i32) -> ! {
    imp::exit_now(code)
}

// ---- Identity ----

/// The identity of a live process by pid; `None` when no process has that pid or it has
/// exited (a zombie on Unix).
///
/// # Errors
///
/// The process exists but its record cannot be read (another user's process).
pub fn process_identity(pid: u32) -> io::Result<Option<ProcessIdentity>> {
    imp::process_identity(pid)
}

/// The identity of this process.
///
/// # Errors
///
/// The OS refused the lookup.
pub fn current_identity() -> io::Result<ProcessIdentity> {
    process_identity(std::process::id())?
        .ok_or_else(|| io::Error::other("this process has no identity record"))
}

// ---- Process-wide signal state (Unix; Windows has none) ----

/// The signals whose state `prepare_process` resets (`docs/design/daemon-lifecycle.md` §2.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixSignal {
    Term,
    Int,
    Hup,
    Chld,
}

impl UnixSignal {
    pub const ALL: [UnixSignal; 4] = [
        UnixSignal::Term,
        UnixSignal::Int,
        UnixSignal::Hup,
        UnixSignal::Chld,
    ];
}

/// The `SIGCHLD` disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigchldDisposition {
    Default,
    Ignore,
    Handler,
}

/// The process-wide signal state that decides whether children stay waitable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalState {
    pub sigchld: SigchldDisposition,
    /// `SA_NOCLDWAIT` is set on `SIGCHLD`.
    pub no_cldwait: bool,
    /// `sa_flags` of `SIGCHLD` as read.
    pub sigchld_flags: i32,
    /// Which of [`UnixSignal::ALL`] the calling thread blocks.
    pub blocked: Vec<UnixSignal>,
}

/// `SIGCHLD` to `SIG_DFL` with no flags; `TERM`, `INT`, `HUP`, `CHLD` unblocked. Call before
/// any thread exists, so every later thread inherits the mask. `Ok` on Windows.
///
/// # Errors
///
/// The OS refused the change.
pub fn reset_process_signals() -> io::Result<()> {
    imp::reset_process_signals()
}

/// The current [`SignalState`] (Windows: the default state).
///
/// # Errors
///
/// The OS refused the query.
pub fn signal_state() -> io::Result<SignalState> {
    imp::signal_state()
}

// ---- Files and privacy (docs/design/daemon-lifecycle.md §2.4) ----

/// The directory the default state directory lives in: `$HOME` on macOS,
/// `FOLDERID_LocalAppData` on Windows.
///
/// # Errors
///
/// `$HOME` is unset or relative, or the known folder lookup failed.
pub fn default_state_base() -> io::Result<PathBuf> {
    imp::default_state_base()
}

/// What an entry must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

/// The outcome of a privacy check: private to the account, or the first check that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privacy {
    Private,
    /// A symlink or a Windows reparse point.
    Link,
    /// Not owned by the account (checked first).
    NotOwned,
    /// Group or other mode bits are set (macOS).
    ModeTooOpen {
        mode: u32,
    },
    /// An ACL entry grants access outside the private trustees (an extended allow entry on
    /// macOS; an allow ACE for another trustee, or no DACL, on Windows).
    AclGrantsOthers,
    /// Not of the requested kind.
    NotOfKind,
}

impl Privacy {
    /// The check that failed, for a refusal message; empty for `Private`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Privacy::Private => String::new(),
            Privacy::Link => "it is a link or reparse point".to_owned(),
            Privacy::NotOwned => "it is not owned by this account".to_owned(),
            Privacy::ModeTooOpen { mode } => {
                format!("its mode {mode:o} grants group or other access")
            }
            Privacy::AclGrantsOthers => {
                "its access control list grants access to others".to_owned()
            }
            Privacy::NotOfKind => "it is not of the expected kind".to_owned(),
        }
    }
}

/// Creates the last component of `path` private to the account: mode `0700` on macOS, an
/// explicit protected DACL (owner and `SYSTEM`) on Windows. The parent must exist.
///
/// # Errors
///
/// The OS error (`AlreadyExists` when something is there).
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    imp::create_private_dir(path)
}

/// Whether `path` is private to the account and of `kind`, never following a final link.
///
/// # Errors
///
/// The entry cannot be inspected (`NotFound` when absent).
pub fn privacy(path: &Path, kind: EntryKind) -> io::Result<Privacy> {
    imp::privacy(path, kind)
}

/// The checks of [`privacy`] on an open regular file.
///
/// # Errors
///
/// The handle cannot be inspected.
pub fn handle_privacy(file: &File) -> io::Result<Privacy> {
    imp::handle_privacy(file)
}

/// How [`open_no_follow`] opens a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read only; never creates.
    Read,
    /// Read and write, created (mode `0600`) if missing, never truncated.
    ReadWriteCreate,
    /// Append, created (mode `0600`) if missing.
    AppendCreate,
}

/// Opens `path` without following a final link (macOS `O_NOFOLLOW`; Windows
/// `FILE_FLAG_OPEN_REPARSE_POINT` and a reparse point refused), and requires the opened handle
/// to be a regular file.
///
/// # Errors
///
/// The OS error; `InvalidInput` for a link, a reparse point, or a non-regular file.
pub fn open_no_follow(path: &Path, access: Access) -> io::Result<File> {
    imp::open_no_follow(path, access)
}

/// Creates `path` (it must not exist) for reading and writing with an explicit owner-only
/// descriptor (macOS mode `0600`; Windows a protected DACL for the owner and `SYSTEM`),
/// never through a link, and verifies the handle with [`handle_privacy`] before returning
/// it; a file that fails the check is removed (this call created it).
///
/// # Errors
///
/// The OS error, or `PermissionDenied` when the created file is not private.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    let file = imp::create_new_private(path)?;
    match handle_privacy(&file) {
        Ok(Privacy::Private) => Ok(file),
        Ok(p) => {
            drop(file);
            let _ = std::fs::remove_file(path);
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the new file is not private: {}", p.describe()),
            ))
        }
        Err(e) => {
            drop(file);
            let _ = std::fs::remove_file(path);
            Err(e)
        }
    }
}

/// Whether `path` is absent or a regular file that is not a link or a reparse point.
///
/// # Errors
///
/// The entry cannot be inspected for another reason than its absence.
pub fn is_plain_file_or_absent(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
        Ok(m) => Ok(m.file_type().is_file() && !imp::is_reparse(&m)),
    }
}

/// How many hard links name the entry at `path` (never following a final link): above 1,
/// another path shares the file.
///
/// # Errors
///
/// The entry cannot be inspected (`NotFound` when absent).
pub fn hard_links(path: &Path) -> io::Result<u64> {
    imp::hard_links(path)
}

// ---- Stop channel (docs/design/daemon-lifecycle.md §2.10) ----

/// The daemon's stop listener: on Windows a `Global\chaldea-stop-<instance>` event with an
/// account-only DACL, waited on by the `daemon-stop` thread; on Unix nothing (a stop is
/// `SIGTERM`). Dropping it ends the thread without calling the callback.
pub struct StopListener {
    name: String,
    _inner: imp::StopInner,
}

impl StopListener {
    /// The event's name (Windows); empty on Unix.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Debug for StopListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StopListener")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Starts the stop listener of `instance` (the instance's UUID text): `on_stop` runs once
/// when a stop request arrives.
///
/// # Errors
///
/// The event cannot be created; `AlreadyExists` when its name is taken (Windows).
pub fn listen_for_stop(
    instance: &str,
    on_stop: Box<dyn FnOnce() + Send>,
) -> io::Result<StopListener> {
    if instance.is_empty() || !instance.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the instance is not a UUID",
        ));
    }
    let (name, inner) = imp::listen_for_stop(instance, on_stop)?;
    Ok(StopListener {
        name,
        _inner: inner,
    })
}

/// The stop event's name of `instance` (Windows).
#[must_use]
pub fn stop_event_name(instance: &str) -> String {
    format!("Global\\chaldea-stop-{instance}")
}

// ---- Detaching (docs/design/daemon-lifecycle.md §2.10, §2.12) ----

/// Puts this process in a session of its own (`setsid`), so it leaves the launching
/// terminal's session and process group. `Ok` on Windows.
///
/// # Errors
///
/// The OS refused (`EPERM` when this process already leads a process group).
pub fn detach_session() -> io::Result<()> {
    imp::detach_session()
}

/// macOS: forks; the parent `_exit(0)`s at once (no destructor or stdio flush runs), so
/// launchd sees its one-shot job finished, and the child returns. Call only while the
/// process has one thread. `Ok` without forking elsewhere.
///
/// # Errors
///
/// `fork` failed (the caller is still the job's process).
pub fn leave_launchd_job() -> io::Result<()> {
    imp::leave_launchd_job()
}

/// What `launchctl` printed and how it exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchctlOutput {
    /// The exit code; `None` when a signal ended it.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `/bin/launchctl <args>` under the spawn lock and returns what it printed and how it
/// exited, all observed before `deadline` (macOS only). Nothing is spawned once the deadline
/// has passed; past it, the command is killed and handed to a reaper thread, and an exit or
/// output first observed at or after it is a timeout.
///
/// # Errors
///
/// The spawn error; `TimedOut` at or past the deadline; `Unsupported` off macOS.
pub fn launchctl(args: &[&OsStr], deadline: Instant) -> io::Result<LaunchctlOutput> {
    imp::launchctl(args, deadline)
}

/// Whether the user's GUI launchd domain exists (design §2.10 "Choosing the macOS path").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuiDomain {
    /// `launchctl print gui/<uid>` exited 0.
    Available,
    /// An SSH session (no probe), or `launchctl` reported the domain does not exist.
    Absent { why: String },
    /// Anything else: a timeout, a spawn error, another exit.
    ProbeFailed { why: String },
}

/// The exit status of `launchctl print gui/<uid>` for a domain that does not exist (L23).
pub const LAUNCHCTL_NO_DOMAIN: i32 = 112;

/// Classifies the outcome of `launchctl print gui/<uid>` (L23): exit 0 is `Available`; exit
/// 112 with "Could not find domain" is `Absent`; anything else is `ProbeFailed`.
#[must_use]
pub fn classify_gui_probe(out: &io::Result<LaunchctlOutput>) -> GuiDomain {
    match out {
        Ok(o) if o.code == Some(0) => GuiDomain::Available,
        Ok(o)
            if o.code == Some(LAUNCHCTL_NO_DOMAIN)
                && (o.stderr.contains("Could not find domain")
                    || o.stdout.contains("Could not find domain")) =>
        {
            GuiDomain::Absent {
                why: "launchctl reports no GUI domain for this user".to_owned(),
            }
        }
        Ok(o) => GuiDomain::ProbeFailed {
            why: format!(
                "launchctl print exited {}: {}",
                o.code
                    .map_or_else(|| "by a signal".to_owned(), |c| c.to_string()),
                o.stderr.trim()
            ),
        },
        Err(e) => GuiDomain::ProbeFailed {
            why: format!("launchctl print failed: {e}"),
        },
    }
}

/// The user's GUI launchd domain, `gui/<euid>`.
#[must_use]
pub fn gui_domain_target() -> String {
    format!("gui/{}", imp::euid())
}

/// [`gui_domain_via`] with [`launchctl`].
#[must_use]
pub fn gui_domain(deadline: Instant) -> GuiDomain {
    gui_domain_via(deadline, &|args, d| launchctl(args, d))
}

/// [`gui_domain_with`] with this process's environment.
#[must_use]
pub fn gui_domain_via(
    deadline: Instant,
    run: &dyn Fn(&[&OsStr], Instant) -> io::Result<LaunchctlOutput>,
) -> GuiDomain {
    gui_domain_with(deadline, &|name| std::env::var_os(name), run)
}

/// Decides the macOS launch path: `Absent` without a probe when `env` gives a non-empty
/// `SSH_CONNECTION` or `SSH_TTY` (the deliberate SSH path); otherwise `run` probes
/// `print gui/<uid>` by `deadline` and [`classify_gui_probe`] decides.
#[must_use]
pub fn gui_domain_with(
    deadline: Instant,
    env: &dyn Fn(&str) -> Option<OsString>,
    run: &dyn Fn(&[&OsStr], Instant) -> io::Result<LaunchctlOutput>,
) -> GuiDomain {
    for var in ["SSH_CONNECTION", "SSH_TTY"] {
        if env(var).is_some_and(|v| !v.is_empty()) {
            return GuiDomain::Absent {
                why: format!("an SSH session ({var} is set)"),
            };
        }
    }
    let target = gui_domain_target();
    classify_gui_probe(&run(&["print".as_ref(), target.as_ref()], deadline))
}

/// This process's file mode creation mask (macOS; `0` elsewhere).
#[must_use]
pub fn current_umask() -> u32 {
    imp::current_umask()
}

/// Whether a detached child is created with `CREATE_BREAKAWAY_FROM_JOB` (Windows, design
/// §2.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breakaway {
    /// Not in a job, or only `SILENT_BREAKAWAY_OK` (the child breaks away implicitly).
    NotRequested,
    /// The immediate job has `BREAKAWAY_OK`.
    Explicit,
}

/// The starter's launch from its own [`JobState`] (design §2.12): `None` when the immediate
/// job allows no breakaway (the start is refused before spawning).
#[must_use]
pub fn breakaway_for(state: &JobState) -> Option<Breakaway> {
    if !state.in_job {
        return Some(Breakaway::NotRequested);
    }
    match state.immediate {
        Some(l) if l.breakaway_ok => Some(Breakaway::Explicit),
        Some(l) if l.silent_breakaway_ok => Some(Breakaway::NotRequested),
        _ => None,
    }
}

/// A child `daemon start` launched directly (design §2.10): the `setsid` path on macOS, every
/// launch on Windows.
pub struct DetachedChild {
    inner: imp::DetachedInner,
}

impl DetachedChild {
    #[must_use]
    pub fn id(&self) -> u32 {
        imp::detached_id(&self.inner)
    }

    /// Its exit, described, once it exited; `None` while it runs.
    ///
    /// # Errors
    ///
    /// Its status cannot be read.
    pub fn try_wait(&mut self) -> io::Result<Option<String>> {
        imp::detached_try_wait(&mut self.inner)
    }
}

impl std::fmt::Debug for DetachedChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DetachedChild")
            .field("pid", &self.id())
            .finish_non_exhaustive()
    }
}

/// Starts `exe args` in `cwd` under the spawn lock, with this process's environment and stdin
/// and stderr the null device. macOS: stdout the null device (the child calls `setsid`
/// itself). Windows: `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, plus
/// `CREATE_BREAKAWAY_FROM_JOB` for [`Breakaway::Explicit`]; stdout a pipe whose read end
/// `ready` takes; and only those three handles inherited (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`),
/// so the child holds nothing else of this process's caller.
///
/// # Errors
///
/// The spawn error (Windows `ERROR_ACCESS_DENIED` when the job refused the breakaway).
pub fn spawn_detached(
    exe: &Path,
    args: &[OsString],
    cwd: &Path,
    breakaway: Breakaway,
    ready: &mut ReadyReader,
) -> io::Result<DetachedChild> {
    Ok(DetachedChild {
        inner: imp::spawn_detached(exe, args, cwd, breakaway, &mut ready.inner)?,
    })
}

// ---- Deadlines ----

/// `observe`'s result when it was observed before `deadline`. The deadline is checked before
/// observing and again after, so a result first seen at or after the deadline loses to it and
/// nothing is observed once it has passed.
pub fn observed_before<T>(deadline: Instant, observe: impl FnOnce() -> T) -> Option<T> {
    if Instant::now() >= deadline {
        return None;
    }
    let seen = observe();
    if Instant::now() >= deadline {
        return None;
    }
    Some(seen)
}

/// How often the readiness channel is polled.
const READY_POLL: Duration = Duration::from_millis(20);

/// Polls `step` every 20 ms until it decides, each step through [`observed_before`]: a
/// verdict, end, or error first seen at or after `deadline` is `TimedOut`.
fn ready_poll(
    deadline: Instant,
    mut step: impl FnMut() -> io::Result<Option<ReadyRead>>,
) -> io::Result<ReadyRead> {
    loop {
        match observed_before(deadline, &mut step) {
            None => return Ok(ReadyRead::TimedOut),
            Some(Err(e)) => return Err(e),
            Some(Ok(Some(r))) => return Ok(r),
            Some(Ok(None)) => {}
        }
        std::thread::sleep(READY_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

// ---- Readiness channel (docs/design/daemon-lifecycle.md §2.10) ----

/// Where a detached child writes its verdict: a Unix socket path (macOS) or its stdout pipe
/// (Windows). Passed as `--ready <channel>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyArg {
    Socket(PathBuf),
    Stdout,
}

impl ReadyArg {
    /// The `--ready` value: the socket path, or `stdout`.
    #[must_use]
    pub fn to_os_string(&self) -> OsString {
        match self {
            ReadyArg::Socket(p) => p.clone().into_os_string(),
            ReadyArg::Stdout => OsString::from("stdout"),
        }
    }

    /// Parses a `--ready` value: `stdout`, or an absolute socket path.
    ///
    /// # Errors
    ///
    /// `InvalidInput` for anything else.
    pub fn parse(s: &OsStr) -> io::Result<ReadyArg> {
        if s == "stdout" {
            return Ok(ReadyArg::Stdout);
        }
        let p = Path::new(s);
        if p.is_absolute() {
            Ok(ReadyArg::Socket(p.to_path_buf()))
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--ready takes `stdout` or an absolute socket path",
            ))
        }
    }
}

/// The longest readiness socket path (`<dir>/r`) placed under the temp directory; a longer
/// one goes under `/tmp` (Darwin's `sun_path` holds 104 bytes with its NUL, L20).
pub const READY_SOCKET_MAX: usize = 100;

/// What one wait on the readiness channel saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyRead {
    /// One line, without its `\n`.
    Line(String),
    /// The channel closed (or the direct child exited) without a line.
    Eof,
    TimedOut,
}

/// The starter's end of the readiness channel. Dropping it removes what it created (macOS:
/// the socket, the launchd plist [`READY_PLIST`], then the directory if that left it empty).
pub struct ReadyReader {
    inner: imp::ReadyInner,
}

impl ReadyReader {
    /// The private directory holding the socket (macOS), also used for the launchd plist.
    #[must_use]
    pub fn dir(&self) -> Option<&Path> {
        imp::ready_dir(&self.inner)
    }

    /// Waits until `deadline` for one line, polling every 20 ms; anything first observed at
    /// or after `deadline` is `TimedOut`. `child`, when given, is the directly spawned child:
    /// its exit before it connected counts as `Eof` (macOS).
    ///
    /// # Errors
    ///
    /// The channel failed for another reason than its end.
    pub fn read_line(
        &mut self,
        deadline: Instant,
        child: Option<&mut DetachedChild>,
    ) -> io::Result<ReadyRead> {
        imp::ready_read(&mut self.inner, deadline, child.map(|c| &mut c.inner))
    }
}

impl std::fmt::Debug for ReadyReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadyReader")
            .field("dir", &self.dir())
            .finish_non_exhaustive()
    }
}

/// The name of the launchd plist the starter writes into the readiness directory (macOS).
pub const READY_PLIST: &str = "job.plist";

/// The starter's channel: macOS a Unix socket `r` in a fresh private directory `chd-<16 hex>`
/// under the temp directory, made absolute against the working directory (under `/tmp` when
/// the socket path would exceed [`READY_SOCKET_MAX`] bytes); Windows the child's stdout,
/// attached by [`spawn_detached`].
///
/// # Errors
///
/// The directory or socket cannot be created.
pub fn ready_channel() -> io::Result<(ReadyReader, ReadyArg)> {
    ready_channel_in(&std::env::temp_dir())
}

/// [`ready_channel`] with `temp` as the temp directory (tests of the length rule).
///
/// # Errors
///
/// As [`ready_channel`].
pub fn ready_channel_in(temp: &Path) -> io::Result<(ReadyReader, ReadyArg)> {
    let (inner, arg) = imp::ready_channel_in(temp)?;
    Ok((ReadyReader { inner }, arg))
}

/// The child's end of the readiness channel.
pub struct ReadyWriter {
    inner: imp::ReadyWriterInner,
}

impl ReadyWriter {
    /// Writes `line` (with its `\n`) in one write.
    ///
    /// # Errors
    ///
    /// The starter stopped reading.
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        imp::ready_write(&mut self.inner, line)
    }

    /// Closes the channel explicitly (macOS: shuts the socket down; Windows: closes the
    /// stdout handle, which dropping Rust's `Stdout` does not).
    pub fn close(self) {
        imp::ready_close(self.inner);
    }
}

impl std::fmt::Debug for ReadyWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadyWriter")
    }
}

/// Opens the child's end: connects to the socket (macOS) or takes over the stdout handle,
/// leaving the process's standard output pointing nowhere (Windows).
///
/// # Errors
///
/// The socket is gone (the starter left) or the handle is missing.
pub fn ready_writer(arg: &ReadyArg) -> io::Result<ReadyWriter> {
    Ok(ReadyWriter {
        inner: imp::ready_writer(arg)?,
    })
}

// ---- Windows parent job (docs/design/daemon-lifecycle.md §2.12) ----

/// The limits of the immediate job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLimits {
    pub kill_on_close: bool,
    pub breakaway_ok: bool,
    pub silent_breakaway_ok: bool,
}

/// Whether this process is in any job, and the immediate job's limits (L13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobState {
    pub in_job: bool,
    pub immediate: Option<JobLimits>,
}

impl JobState {
    /// In no job (every non-Windows host).
    pub const NONE: JobState = JobState {
        in_job: false,
        immediate: None,
    };
}

/// This process's [`JobState`]: `IsProcessInJob(self, NULL)` and the immediate job's extended
/// limits (Windows); [`JobState::NONE`] elsewhere.
///
/// # Errors
///
/// The OS refused the query.
pub fn job_state() -> io::Result<JobState> {
    imp::job_state()
}

/// Whether this process has a console window (`GetConsoleWindow`, Windows); `None` elsewhere.
#[must_use]
pub fn has_console() -> Option<bool> {
    imp::has_console()
}

// ---- Stop request (docs/design/daemon-lifecycle.md §2.10) ----

/// The published daemon a stop targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopTarget {
    pub pid: u32,
    pub start_time: String,
    /// The instance UUID text.
    pub instance: String,
}

/// What became of a stop request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSent {
    /// `SIGTERM` was delivered (Unix) or the instance's event set (Windows).
    Sent,
    /// The target exited before or during the request.
    Exited,
    /// The pid is alive with another start time: nothing was signalled.
    IdentityMismatch,
    AccessDenied,
    /// The target is alive and matching but has no stop event (Windows).
    NoListener,
}

/// Sends a stop to `target`: Unix `SIGTERM` after checking `(pid, start_time)` (the window
/// between check and signal is the PID race of design §6.2); Windows sets
/// `Global\chaldea-stop-<instance>`.
///
/// # Errors
///
/// The identity lookup failed, or the delivery failed for another reason than the target's
/// absence or denied access.
pub fn send_stop(target: &StopTarget) -> io::Result<StopSent> {
    send_stop_with(target, &imp::deliver_stop)
}

/// [`send_stop`] with the delivery (`kill`, or the event's open and set) replaced, for fault
/// tests.
///
/// # Errors
///
/// As [`send_stop`].
#[doc(hidden)]
pub fn send_stop_with(
    target: &StopTarget,
    deliver: &dyn Fn(&StopTarget) -> io::Result<()>,
) -> io::Result<StopSent> {
    if imp::CHECK_BEFORE_DELIVERY {
        match identity_matches(target)? {
            Some(true) => {}
            Some(false) => return Ok(StopSent::IdentityMismatch),
            None => return Ok(StopSent::Exited),
        }
    }
    match deliver(target) {
        Ok(()) => Ok(StopSent::Sent),
        Err(e) if e.raw_os_error() == Some(imp::DELIVERY_GONE) => {
            if imp::CHECK_BEFORE_DELIVERY {
                // `ESRCH` after a matching check: the target exited in between.
                return Ok(StopSent::Exited);
            }
            // No event: tell an exited target from a live one without a listener.
            Ok(match identity_matches(target)? {
                None => StopSent::Exited,
                Some(false) => StopSent::IdentityMismatch,
                Some(true) => StopSent::NoListener,
            })
        }
        Err(e)
            if e.kind() == io::ErrorKind::PermissionDenied
                || e.raw_os_error() == Some(imp::DELIVERY_DENIED) =>
        {
            Ok(StopSent::AccessDenied)
        }
        Err(e) => Err(e),
    }
}

/// `None`: no live process has the pid; `Some(true)`: it has the recorded start time.
fn identity_matches(target: &StopTarget) -> io::Result<Option<bool>> {
    Ok(process_identity(target.pid)?.map(|id| id.start_time == target.start_time))
}

// ---- Browser (docs/design/daemon-lifecycle.md §2.13) ----

/// How long `/usr/bin/open` may take before it is killed (macOS).
pub const OPEN_URL_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Hands `url` to the OS: macOS `/usr/bin/open <url>` under the spawn lock, waited for up to
/// [`OPEN_URL_WAIT`]; Windows `ShellExecuteExW` with the `open` verb.
///
/// # Errors
///
/// The launcher failed, exited non-zero, or timed out.
pub fn open_url(url: &str) -> io::Result<()> {
    imp::open_url(url)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::{Duration, Instant};

    use super::{GuiDomain, LaunchctlOutput, classify_gui_probe};

    fn out(code: Option<i32>, stderr: &str) -> io::Result<LaunchctlOutput> {
        Ok(LaunchctlOutput {
            code,
            stdout: String::new(),
            stderr: stderr.to_owned(),
        })
    }

    /// PR8-CL-13: exit 0, exit 112 with "Could not find domain", and other outputs (L23).
    #[test]
    fn classify_gui_probe_outputs() {
        assert_eq!(classify_gui_probe(&out(Some(0), "")), GuiDomain::Available);
        assert!(matches!(
            classify_gui_probe(&out(Some(112), "Could not find domain for user gui: 9999")),
            GuiDomain::Absent { .. }
        ));
        assert!(matches!(
            classify_gui_probe(&out(Some(112), "something else")),
            GuiDomain::ProbeFailed { .. }
        ));
        assert!(matches!(
            classify_gui_probe(&out(Some(1), "")),
            GuiDomain::ProbeFailed { .. }
        ));
        assert!(matches!(
            classify_gui_probe(&Err(io::Error::from(io::ErrorKind::TimedOut))),
            GuiDomain::ProbeFailed { .. }
        ));
    }

    /// PR8-CL-13: a non-empty `SSH_CONNECTION` or `SSH_TTY` decides `Absent` without running
    /// the probe; empty or unset, the probe decides.
    #[test]
    fn ssh_session_skips_the_probe() {
        use std::ffi::OsString;
        let cases: [(&[(&str, &str)], bool); 5] = [
            (&[("SSH_CONNECTION", "10.0.0.1 1 10.0.0.2 22")], false),
            (&[("SSH_TTY", "/dev/ttys001")], false),
            (&[("SSH_CONNECTION", ""), ("SSH_TTY", "")], true),
            (&[("SSH_CONNECTION", "")], true),
            (&[], true),
        ];
        for (vars, probes) in cases {
            let called = std::cell::Cell::new(false);
            let probe = |_: &[&std::ffi::OsStr], _: Instant| {
                called.set(true);
                out(Some(0), "")
            };
            let env = |name: &str| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| OsString::from(v))
            };
            let got = super::gui_domain_with(Instant::now() + Duration::from_secs(1), &env, &probe);
            assert_eq!(called.get(), probes, "{vars:?}");
            if probes {
                assert_eq!(got, GuiDomain::Available, "{vars:?}");
            } else {
                assert!(matches!(got, GuiDomain::Absent { .. }), "{vars:?}: {got:?}");
            }
        }
    }

    /// A result first observed at or after the deadline loses to it; one observed before
    /// wins; nothing is observed once the deadline has passed.
    #[test]
    fn observations_that_cross_the_deadline_lose() {
        let end = Instant::now() + Duration::from_millis(100);
        let late = super::observed_before(end, || {
            std::thread::sleep(end.saturating_duration_since(Instant::now()));
            true
        });
        assert_eq!(late, None);
        let soon = Instant::now() + Duration::from_secs(5);
        assert_eq!(super::observed_before(soon, || 7), Some(7));
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        assert_eq!(
            super::observed_before(past, || panic!("observed after the deadline")),
            None::<()>
        );
    }

    /// The readiness poll: a verdict first observed at or after the deadline is `TimedOut`,
    /// as is one that a step found only at the deadline.
    #[test]
    fn readiness_poll_crossing_the_deadline_times_out() {
        use super::{ReadyRead, ready_poll};
        let end = Instant::now() + Duration::from_millis(100);
        let got = ready_poll(end, || {
            std::thread::sleep(end.saturating_duration_since(Instant::now()));
            Ok(Some(ReadyRead::Line("ready 1 2".to_owned())))
        })
        .unwrap();
        assert_eq!(got, ReadyRead::TimedOut);
        let mut n = 0;
        let got = ready_poll(Instant::now() + Duration::from_secs(5), || {
            n += 1;
            Ok((n == 3).then_some(ReadyRead::Eof))
        })
        .unwrap();
        assert_eq!((got, n), (ReadyRead::Eof, 3));
    }

    /// Dropping the reader removes only what it created (macOS): the socket and the plist by
    /// name, then the directory, which another file in it keeps.
    #[cfg(target_os = "macos")]
    #[test]
    fn reader_removes_only_its_own_files() {
        use super::{READY_PLIST, ready_channel_in};
        let (r, _) = ready_channel_in(&std::env::temp_dir()).unwrap();
        let dir = r.dir().unwrap().to_path_buf();
        std::fs::write(dir.join(READY_PLIST), b"plist").unwrap();
        std::fs::write(dir.join("other"), b"x").unwrap();
        drop(r);
        let mut left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(left, vec![std::ffi::OsString::from("other")]);
        std::fs::remove_dir_all(&dir).unwrap();
        let (r, _) = ready_channel_in(&std::env::temp_dir()).unwrap();
        let dir = r.dir().unwrap().to_path_buf();
        drop(r);
        assert!(!dir.exists());
    }

    /// A relative temp directory is made absolute against the working directory (macOS), so
    /// `--ready` carries an absolute path the child accepts. The working directory is
    /// process-wide, so a helper (this test re-run in a temp directory) resolves `rel`.
    #[cfg(target_os = "macos")]
    #[test]
    fn relative_tmpdir_is_made_absolute() {
        use super::{ReadyArg, ready_channel_in};
        const HELPER: &str = "CHALDEA_HOST_RELATIVE_TMPDIR";
        if std::env::var_os(HELPER).is_some() {
            let (r, arg) = ready_channel_in(std::path::Path::new("rel")).unwrap();
            let ReadyArg::Socket(path) = &arg else {
                panic!("{arg:?}")
            };
            assert!(path.is_absolute(), "{}", path.display());
            assert_eq!(ReadyArg::parse(path.as_os_str()).unwrap(), arg);
            drop(r);
            return;
        }
        let base = std::env::temp_dir().join(format!("chaldea-rel-{}", std::process::id()));
        std::fs::create_dir_all(base.join("rel")).unwrap();
        let out = crate::with_spawn_lock(|| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "host::tests::relative_tmpdir_is_made_absolute",
                    "--exact",
                    "--test-threads=1",
                ])
                .env(HELPER, "1")
                .current_dir(&base)
                .stdin(std::process::Stdio::null())
                .output()
        })
        .unwrap();
        std::fs::remove_dir_all(&base).unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && text.contains("1 passed"),
            "{text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The readiness channel (macOS): a socket path of at most 100 bytes, under `/tmp` when
    /// the temp directory is long (PR8-CL-14); a line written by the child is read; a line
    /// already there when the deadline passed does not win (deadline first); dropping the
    /// reader removes its directory.
    #[cfg(target_os = "macos")]
    #[test]
    fn long_tmpdir_uses_tmp() {
        use super::{ReadyArg, ReadyRead, ready_channel_in, ready_writer};
        let base = std::env::temp_dir().join(format!("chaldea-ready-{}", std::process::id()));
        let long = base.join("x".repeat(120usize.saturating_sub(base.as_os_str().len()).max(1)));
        std::fs::create_dir_all(&long).unwrap();
        assert!(long.as_os_str().len() >= 100);
        let (mut r, arg) = ready_channel_in(&long).unwrap();
        let ReadyArg::Socket(path) = &arg else {
            panic!("{arg:?}")
        };
        assert!(path.starts_with("/tmp"), "{}", path.display());
        assert!(path.as_os_str().len() <= 100, "{}", path.display());
        let dir = r.dir().unwrap().to_path_buf();
        let mut w = ready_writer(&arg).unwrap();
        w.write_line("ready 7373 1\n").unwrap();
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        assert_eq!(r.read_line(past, None).unwrap(), ReadyRead::TimedOut);
        assert_eq!(
            r.read_line(Instant::now() + Duration::from_secs(5), None)
                .unwrap(),
            ReadyRead::Line("ready 7373 1".to_owned())
        );
        w.close();
        assert_eq!(
            r.read_line(Instant::now() + Duration::from_secs(5), None)
                .unwrap(),
            ReadyRead::Eof
        );
        drop(r);
        assert!(!dir.exists());
        let (r, arg) = ready_channel_in(&std::env::temp_dir()).unwrap();
        let ReadyArg::Socket(path) = &arg else {
            panic!("{arg:?}")
        };
        assert!(path.as_os_str().len() <= 100, "{}", path.display());
        drop(r);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
