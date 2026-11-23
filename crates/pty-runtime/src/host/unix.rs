//! `host` on macOS: rustix and std for the safe calls (sessions, signals, sockets, the
//! `launchctl` and `open` children), `unix::sys` for `sigaction`, the signal mask, the
//! extended ACL reads, and `fork`.

use std::ffi::{CString, OsStr};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::Instant;

use super::{
    Access, Breakaway, EntryKind, JobState, LaunchctlOutput, Privacy, ProcessIdentity, ReadyArg,
    ReadyRead, Received, SigchldDisposition, SignalState, StopTarget, UnixSignal, receive_before,
    recv_before, spawn_before, timed_out,
};
use crate::unix::sys;

pub(super) fn exit_now(code: i32) -> ! {
    sys::exit_now(code)
}

pub(super) fn process_identity(pid: u32) -> io::Result<Option<ProcessIdentity>> {
    crate::unix::identify_live(pid)
}

fn raw_signal(s: UnixSignal) -> i32 {
    match s {
        UnixSignal::Term => libc::SIGTERM,
        UnixSignal::Int => libc::SIGINT,
        UnixSignal::Hup => libc::SIGHUP,
        UnixSignal::Chld => libc::SIGCHLD,
    }
}

pub(super) fn reset_process_signals() -> io::Result<()> {
    sys::reset_process_signals()
}

pub(super) fn signal_state() -> io::Result<SignalState> {
    let (handler, flags, mask) = sys::signal_state()?;
    let sigchld = if handler == libc::SIG_DFL {
        SigchldDisposition::Default
    } else if handler == libc::SIG_IGN {
        SigchldDisposition::Ignore
    } else {
        SigchldDisposition::Handler
    };
    Ok(SignalState {
        sigchld,
        no_cldwait: flags & libc::SA_NOCLDWAIT != 0,
        sigchld_flags: flags,
        blocked: UnixSignal::ALL
            .into_iter()
            .filter(|s| sys::in_set(mask, raw_signal(*s)))
            .collect(),
    })
}

pub(super) fn default_state_base() -> io::Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(h) if Path::new(&h).is_absolute() => Ok(PathBuf::from(h)),
        _ => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "HOME is not set to an absolute path",
        )),
    }
}

pub(super) fn create_private_dir(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

fn judge(
    m: &Metadata,
    kind: EntryKind,
    acl_allows: impl FnOnce() -> io::Result<bool>,
) -> io::Result<Privacy> {
    let ft = m.file_type();
    if ft.is_symlink() {
        return Ok(Privacy::Link);
    }
    let of_kind = match kind {
        EntryKind::File => ft.is_file(),
        EntryKind::Directory => ft.is_dir(),
    };
    if !of_kind {
        return Ok(Privacy::NotOfKind);
    }
    if m.uid() != rustix::process::geteuid().as_raw() {
        return Ok(Privacy::NotOwned);
    }
    let mode = m.mode() & 0o7777;
    if mode & 0o077 != 0 {
        return Ok(Privacy::ModeTooOpen { mode });
    }
    if acl_allows()? {
        return Ok(Privacy::AclGrantsOthers);
    }
    Ok(Privacy::Private)
}

pub(super) fn privacy(path: &Path, kind: EntryKind) -> io::Result<Privacy> {
    let m = std::fs::symlink_metadata(path)?;
    judge(&m, kind, || {
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in the path"))?;
        sys::acl_allows_link(&c)
    })
}

pub(super) fn handle_privacy(file: &File) -> io::Result<Privacy> {
    let m = file.metadata()?;
    judge(&m, EntryKind::File, || sys::acl_allows_fd(file.as_fd()))
}

fn not_regular() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")
}

pub(super) fn open_no_follow(path: &Path, access: Access) -> io::Result<File> {
    let mut o = OpenOptions::new();
    match access {
        Access::Read => {
            o.read(true);
        }
        Access::ReadWriteCreate => {
            o.read(true).write(true).create(true).mode(0o600);
        }
        Access::AppendCreate => {
            o.append(true).create(true).mode(0o600);
        }
    }
    // Non-blocking, so a FIFO or device fails the type check below instead of blocking the
    // open; for a regular file the flag changes nothing.
    o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let f = o.open(path).map_err(|e| {
        if e.raw_os_error() == Some(libc::ELOOP) {
            io::Error::new(io::ErrorKind::InvalidInput, "a link")
        } else {
            e
        }
    })?;
    if !f.metadata()?.file_type().is_file() {
        return Err(not_regular());
    }
    Ok(f)
}

pub(super) fn create_new_private(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

pub(super) fn hard_links(path: &Path) -> io::Result<u64> {
    Ok(std::fs::symlink_metadata(path)?.nlink())
}

pub(super) fn is_reparse(_m: &Metadata) -> bool {
    false
}

/// Nothing: a stop on Unix is `SIGTERM`.
pub(super) struct StopInner;

pub(super) fn listen_for_stop(
    _instance: &str,
    _on_stop: Box<dyn FnOnce() + Send>,
) -> io::Result<(String, StopInner)> {
    Ok((String::new(), StopInner))
}

// ---- Detaching ----

pub(super) fn detach_session() -> io::Result<()> {
    rustix::process::setsid()?;
    Ok(())
}

pub(super) fn leave_launchd_job() -> io::Result<()> {
    match sys::fork()? {
        sys::Forked::Parent => sys::exit_now(0),
        sys::Forked::Child => Ok(()),
    }
}

pub(super) fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

pub(super) fn current_umask() -> u32 {
    use rustix::fs::Mode;
    // Read by setting and restoring: the CLI's starter is single-threaded here.
    let old = rustix::process::umask(Mode::from_raw_mode(0o077));
    rustix::process::umask(old);
    u32::from(old.bits())
}

/// Reads `r` to its end on a thread; the bytes arrive on the returned channel.
fn drain(mut r: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

/// Kills `child` and leaves its reaping to a thread, so no wait outlives the caller's deadline.
fn abandon(mut child: Child) {
    let _ = child.kill();
    if !matches!(child.try_wait(), Ok(Some(_))) {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// `child`'s exit when it comes before `deadline`, through [`receive_before`] woken by the
/// exit itself (kqueue); `None` at the deadline.
fn exit_before(child: &mut Child, deadline: Instant) -> io::Result<Option<ExitStatus>> {
    let pid = child.id();
    receive_before(deadline, |left| {
        if let Some(s) = child.try_wait()? {
            return Ok(Some(s));
        }
        sys::wait_readable_or_exit(None, Some(pid), left)?;
        child.try_wait()
    })
}

/// Spawns `cmd` through [`spawn_before`], hands the child to `spawned`, and waits for its exit
/// through [`exit_before`]; past the deadline the child is killed and handed to a reaper
/// thread.
fn run_until(
    cmd: &mut Command,
    deadline: Instant,
    what: &str,
    spawned: impl FnOnce(&mut Child),
) -> io::Result<ExitStatus> {
    let mut child = spawn_before(deadline, |_| cmd.spawn()).ok_or_else(|| timed_out(what))??;
    spawned(&mut child);
    match exit_before(&mut child, deadline) {
        Ok(Some(s)) => Ok(s),
        Ok(None) => {
            abandon(child);
            Err(timed_out(what))
        }
        Err(e) => {
            abandon(child);
            Err(e)
        }
    }
}

/// The drained output, if it arrived before `deadline` ([`recv_before`]).
fn output_before(rx: Option<mpsc::Receiver<Vec<u8>>>, deadline: Instant) -> Option<String> {
    let Some(rx) = rx else {
        return Some(String::new());
    };
    match recv_before(&rx, deadline) {
        Received::Got(b) => Some(String::from_utf8_lossy(&b).into_owned()),
        Received::Closed | Received::TimedOut => None,
    }
}

/// `cmd`'s exit and output, all observed before `deadline` ([`run_until`]); output not read to
/// its end by then (a grandchild holding the pipes) is a timeout too.
fn output_until(cmd: &mut Command, deadline: Instant, what: &str) -> io::Result<LaunchctlOutput> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut pipes = (None, None);
    let status = run_until(cmd, deadline, what, |c| {
        pipes = (c.stdout.take().map(drain), c.stderr.take().map(drain));
    })?;
    let (Some(stdout), Some(stderr)) = (
        output_before(pipes.0, deadline),
        output_before(pipes.1, deadline),
    ) else {
        return Err(timed_out(what));
    };
    Ok(LaunchctlOutput {
        code: status.code(),
        stdout,
        stderr,
    })
}

pub(super) fn launchctl(args: &[&OsStr], deadline: Instant) -> io::Result<LaunchctlOutput> {
    output_until(
        Command::new("/bin/launchctl").args(args),
        deadline,
        "launchctl",
    )
}

pub(super) struct DetachedInner(Child);

pub(super) fn spawn_detached(
    exe: &Path,
    args: &[std::ffi::OsString],
    cwd: &Path,
    _breakaway: Breakaway,
    _ready: &mut ReadyInner,
    deadline: Instant,
) -> io::Result<DetachedInner> {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = spawn_before(deadline, |_| cmd.spawn())
        .ok_or_else(|| timed_out("the daemon's launch"))??;
    Ok(DetachedInner(child))
}

pub(super) fn detached_id(inner: &DetachedInner) -> u32 {
    inner.0.id()
}

pub(super) fn detached_exit_before(
    inner: &mut DetachedInner,
    deadline: Instant,
) -> io::Result<Option<String>> {
    Ok(exit_before(&mut inner.0, deadline)?.map(|s| s.to_string()))
}

// ---- Readiness channel ----

pub(super) struct ReadyInner {
    dir: PathBuf,
    listener: UnixListener,
    stream: Option<UnixStream>,
    buf: Vec<u8>,
    /// This reader created the plist.
    plist: bool,
}

impl Drop for ReadyInner {
    fn drop(&mut self) {
        // Only what this reader created: the socket (bound by it), the plist if it created
        // it, then the directory, which anything else in it keeps.
        let _ = std::fs::remove_file(self.dir.join(SOCKET));
        if self.plist {
            let _ = std::fs::remove_file(self.dir.join(super::READY_PLIST));
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

pub(super) fn ready_write_plist(inner: &mut ReadyInner, contents: &[u8]) -> io::Result<PathBuf> {
    let path = inner.dir.join(super::READY_PLIST);
    let mut f = super::create_private_file(&path)?;
    inner.plist = true;
    f.write_all(contents)?;
    f.sync_all()?;
    Ok(path)
}

/// The socket's name in the readiness directory.
const SOCKET: &str = "r";

pub(super) fn ready_dir(inner: &ReadyInner) -> Option<&Path> {
    Some(&inner.dir)
}

/// `chd-<16 hex>`: random per call (std's per-process random hash keys plus the time).
fn fresh_name() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u32(std::process::id());
    format!("chd-{:016x}", h.finish())
}

pub(super) fn ready_channel_in(temp: &Path) -> io::Result<(ReadyInner, ReadyArg)> {
    // `--ready` carries an absolute path (a relative `TMPDIR` names the caller's directory).
    let temp = std::path::absolute(temp)?;
    let probe = temp.join("chd-0123456789abcdef").join(SOCKET);
    let base = if probe.as_os_str().len() > super::READY_SOCKET_MAX {
        Path::new("/tmp").to_path_buf()
    } else {
        temp
    };
    let mut tries = 0;
    let dir = loop {
        let dir = base.join(fresh_name());
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => break dir,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && tries < 16 => tries += 1,
            Err(e) => return Err(e),
        }
    };
    // The owner and mode of what was created (the umask may only narrow 0700).
    match privacy(&dir, EntryKind::Directory) {
        Ok(Privacy::Private) => {}
        Ok(p) => {
            let _ = std::fs::remove_dir(&dir);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the readiness directory is not private: {}", p.describe()),
            ));
        }
        Err(e) => {
            let _ = std::fs::remove_dir(&dir);
            return Err(e);
        }
    }
    let path = dir.join(SOCKET);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            // Nothing was bound: only the directory is this attempt's.
            let _ = std::fs::remove_dir(&dir);
            return Err(e);
        }
    };
    let inner = ReadyInner {
        dir,
        listener,
        stream: None,
        buf: Vec::new(),
        plist: false,
    };
    // From here dropping `inner` removes the socket and the directory.
    inner.listener.set_nonblocking(true)?;
    Ok((inner, ReadyArg::Socket(path)))
}

/// The longest verdict line the starter accepts.
const MAX_READY_LINE: usize = 4096;

pub(super) fn ready_read(
    inner: &mut ReadyInner,
    deadline: Instant,
    mut child: Option<&mut DetachedInner>,
) -> io::Result<ReadyRead> {
    let got = receive_before(deadline, |left| {
        if let Some(r) = ready_step(inner, child.as_deref_mut())? {
            return Ok(Some(r));
        }
        // Woken by a connection, data, the peer's end, or the direct child's exit.
        let fd = match &inner.stream {
            Some(s) => s.as_fd(),
            None => inner.listener.as_fd(),
        };
        sys::wait_readable_or_exit(Some(fd), child.as_deref().map(|c| c.0.id()), left)?;
        ready_step(inner, child.as_deref_mut())
    })?;
    Ok(got.unwrap_or(ReadyRead::TimedOut))
}

/// One look at the channel: a connection accepted and read until it would block; `None` while
/// nothing is decided.
fn ready_step(
    inner: &mut ReadyInner,
    child: Option<&mut DetachedInner>,
) -> io::Result<Option<ReadyRead>> {
    if inner.stream.is_none() {
        match inner.listener.accept() {
            Ok((s, _)) => {
                // Polled like the listener: no socket option is set on it, since Darwin
                // refuses `setsockopt` once the peer shut its end down (`EINVAL`).
                s.set_nonblocking(true)?;
                inner.stream = Some(s);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if let Some(c) = child
                    && c.0.try_wait()?.is_some()
                {
                    // A direct child that exited without connecting decided nothing.
                    return Ok(Some(ReadyRead::Eof));
                }
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
    }
    let Some(stream) = inner.stream.as_mut() else {
        return Ok(None);
    };
    loop {
        let mut chunk = [0u8; 512];
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(Some(ReadyRead::Eof)),
            Ok(n) => {
                inner.buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = inner.buf.iter().position(|b| *b == b'\n') {
                    let line = String::from_utf8_lossy(&inner.buf[..i]).into_owned();
                    inner.buf.drain(..=i);
                    return Ok(Some(ReadyRead::Line(line)));
                }
                if inner.buf.len() > MAX_READY_LINE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the readiness line is too long",
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        }
    }
}

pub(super) struct ReadyWriterInner(UnixStream);

pub(super) fn ready_writer(arg: &ReadyArg) -> io::Result<ReadyWriterInner> {
    match arg {
        ReadyArg::Socket(p) => Ok(ReadyWriterInner(UnixStream::connect(p)?)),
        ReadyArg::Stdout => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the readiness channel on macOS is a socket",
        )),
    }
}

pub(super) fn ready_write(inner: &mut ReadyWriterInner, line: &str) -> io::Result<()> {
    inner.0.write_all(line.as_bytes())
}

pub(super) fn ready_close(inner: ReadyWriterInner) {
    let ReadyWriterInner(stream) = inner;
    let _ = stream.shutdown(std::net::Shutdown::Both);
    drop(stream);
}

// ---- Job, console (Windows only) ----

pub(super) fn job_state() -> io::Result<JobState> {
    Ok(JobState::NONE)
}

pub(super) fn has_console() -> Option<bool> {
    None
}

// ---- Stop ----

/// Unix checks the identity before signalling.
pub(super) const CHECK_BEFORE_DELIVERY: bool = true;
pub(super) const DELIVERY_GONE: i32 = libc::ESRCH;
pub(super) const DELIVERY_DENIED: i32 = libc::EPERM;

/// The process a stop signals.
pub(super) struct StopDelivery(rustix::process::Pid);

pub(super) fn open_stop(target: &StopTarget) -> io::Result<StopDelivery> {
    i32::try_from(target.pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .map(StopDelivery)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a pid"))
}

pub(super) fn fire_stop(d: &StopDelivery) -> io::Result<()> {
    rustix::process::kill_process(d.0, rustix::process::Signal::TERM)?;
    Ok(())
}

// ---- Browser ----

pub(super) fn open_url(url: &str) -> io::Result<()> {
    let status = run_until(
        Command::new("/usr/bin/open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        Instant::now() + super::OPEN_URL_WAIT,
        "open",
        |_| {},
    )?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("open exited {status}")))
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::output_until;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", script]);
        c
    }

    /// Nothing is spawned once the deadline has passed: a program that does not exist gives
    /// the timeout, not the spawn error a spawn attempt would.
    #[test]
    fn nothing_runs_after_the_deadline() {
        let _s = super::super::tests::serial();
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        let got = output_until(&mut Command::new("/nonexistent/chaldea-test"), past, "test");
        assert_eq!(got.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        let got = output_until(
            &mut Command::new("/nonexistent/chaldea-test"),
            Instant::now() + Duration::from_secs(5),
            "test",
        );
        assert_eq!(got.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    /// A command still running at the deadline, or whose output a grandchild keeps open, is a
    /// timeout returned at the deadline, not after the command or the grandchild ends.
    #[test]
    fn a_command_past_its_deadline_returns_at_the_deadline() {
        let _s = super::super::tests::serial();
        for script in ["sleep 3", "sleep 3 & echo started"] {
            let t = Instant::now();
            let got = output_until(&mut sh(script), t + Duration::from_millis(300), "test");
            assert_eq!(
                got.unwrap_err().kind(),
                std::io::ErrorKind::TimedOut,
                "{script}"
            );
            assert!(
                t.elapsed() < Duration::from_millis(1500),
                "{script}: {:?}",
                t.elapsed()
            );
        }
        let got = output_until(
            &mut sh("echo out; echo err >&2; exit 3"),
            Instant::now() + Duration::from_secs(5),
            "test",
        )
        .unwrap();
        assert_eq!(
            (got.code, got.stdout.as_str(), got.stderr.as_str()),
            (Some(3), "out\n", "err\n")
        );
        super::super::tests::no_children_left();
    }
}
