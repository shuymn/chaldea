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
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::{
    Access, Breakaway, EntryKind, JobState, LaunchctlOutput, Privacy, ProcessIdentity, ReadyArg,
    ReadyRead, SigchldDisposition, SignalState, StopTarget, UnixSignal,
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

/// How often a child's exit is polled.
const CHILD_POLL: Duration = Duration::from_millis(10);

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

/// Waits for `child` until `deadline`; past it, kills and reaps it and returns `TimedOut`.
/// Deadline first: an exit first observed after the deadline counts as a timeout.
fn wait_until(child: &mut Child, deadline: Instant, what: &str) -> io::Result<ExitStatus> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{what} did not finish in time"),
            ));
        }
        if let Some(s) = child.try_wait()? {
            return Ok(s);
        }
        std::thread::sleep(CHILD_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

pub(super) fn launchctl(args: &[&OsStr], deadline: Instant) -> io::Result<LaunchctlOutput> {
    let mut child = crate::with_spawn_lock(|| {
        Command::new("/bin/launchctl")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    })?;
    let out = child.stdout.take().map(drain);
    let err = child.stderr.take().map(drain);
    let status = wait_until(&mut child, deadline, "launchctl")?;
    // The pipes close with the exit; a grandchild holding them is not waited for.
    let take = |rx: Option<mpsc::Receiver<Vec<u8>>>| {
        rx.and_then(|r| r.recv_timeout(Duration::from_millis(500)).ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };
    Ok(LaunchctlOutput {
        code: status.code(),
        stdout: take(out),
        stderr: take(err),
    })
}

pub(super) fn configure_detached(_cmd: &mut Command, _breakaway: Breakaway) {}

pub(super) fn uninherit_std_handles() -> io::Result<()> {
    Ok(())
}

// ---- Readiness channel ----

pub(super) struct ReadyInner {
    dir: PathBuf,
    listener: UnixListener,
    stream: Option<UnixStream>,
    buf: Vec<u8>,
}

impl Drop for ReadyInner {
    fn drop(&mut self) {
        // The directory this starter created, with the socket and the plist in it.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(super) fn ready_dir(inner: &ReadyInner) -> Option<&Path> {
    Some(&inner.dir)
}

pub(super) fn ready_attach(_inner: &mut ReadyInner, _stdout: ChildStdout) {}

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
    let probe = temp.join("chd-0123456789abcdef").join("r");
    let base = if probe.as_os_str().len() > super::READY_SOCKET_MAX {
        Path::new("/tmp").to_path_buf()
    } else {
        temp.to_path_buf()
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
    let path = dir.join("r");
    let listener = match UnixListener::bind(&path).and_then(|l| {
        l.set_nonblocking(true)?;
        Ok(l)
    }) {
        Ok(l) => l,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    Ok((
        ReadyInner {
            dir,
            listener,
            stream: None,
            buf: Vec::new(),
        },
        ReadyArg::Socket(path),
    ))
}

/// The longest verdict line the starter accepts.
const MAX_READY_LINE: usize = 4096;
const READY_POLL: Duration = Duration::from_millis(20);

pub(super) fn ready_read(
    inner: &mut ReadyInner,
    deadline: Instant,
    mut child: Option<&mut Child>,
) -> io::Result<ReadyRead> {
    loop {
        // Deadline first: a line first observed after the deadline does not win.
        if Instant::now() >= deadline {
            return Ok(ReadyRead::TimedOut);
        }
        match inner.stream.as_mut() {
            None => match inner.listener.accept() {
                Ok((s, _)) => {
                    // Polled like the listener: no socket option is set on it, since Darwin
                    // refuses `setsockopt` once the peer shut its end down (`EINVAL`).
                    s.set_nonblocking(true)?;
                    inner.stream = Some(s);
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if let Some(c) = child.as_deref_mut()
                        && c.try_wait()?.is_some()
                    {
                        // A direct child that exited without connecting decided nothing.
                        return Ok(ReadyRead::Eof);
                    }
                }
                Err(e) => return Err(e),
            },
            Some(stream) => {
                let mut chunk = [0u8; 512];
                match stream.read(&mut chunk) {
                    Ok(0) => return Ok(ReadyRead::Eof),
                    Ok(n) => {
                        inner.buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = inner.buf.iter().position(|b| *b == b'\n') {
                            let line = String::from_utf8_lossy(&inner.buf[..i]).into_owned();
                            inner.buf.drain(..=i);
                            return Ok(ReadyRead::Line(line));
                        }
                        if inner.buf.len() > MAX_READY_LINE {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "the readiness line is too long",
                            ));
                        }
                        continue;
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(READY_POLL.min(left));
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

pub(super) fn deliver_stop(target: &StopTarget) -> io::Result<()> {
    let pid = i32::try_from(target.pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a pid"))?;
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)?;
    Ok(())
}

// ---- Browser ----

pub(super) fn open_url(url: &str) -> io::Result<()> {
    let mut child = crate::with_spawn_lock(|| {
        Command::new("/usr/bin/open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    })?;
    let status = wait_until(&mut child, Instant::now() + super::OPEN_URL_WAIT, "open")?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("open exited {status}")))
    }
}
