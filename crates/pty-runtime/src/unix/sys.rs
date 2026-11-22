//! The only module allowed `unsafe` (`docs/design/pty-unix.md` §2.7). Every function here
//! is safe to call: the `pre_exec` body of the spawn, the fd ceiling, and libproc.

use std::ffi::c_int;
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::unix::process::CommandExt;
use std::process::Command;

unsafe extern "C" {
    /// `unistd.h`; libc 0.2.191 does not declare it for Apple targets.
    fn getdtablesize() -> c_int;
}

/// `SZOMB` of `sys/proc.h`: the status of a zombie in `proc_bsdinfo`.
const SZOMB: u32 = 5;

/// Test-only behaviour of the `pre_exec` closure, captured by value before the fork.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PreExecTest {
    /// Raised to the child itself first, while the inherited mask still blocks it.
    pub(crate) raise: Option<c_int>,
    /// The child reads one byte from this fd before anything else (a park gate).
    pub(crate) park_fd: Option<c_int>,
}

/// Installs the `pre_exec` of §2.2 stage 4 on `cmd`: `setsid`, `TIOCSCTTY` on fd 0, close
/// every fd in `3..bound` without `FD_CLOEXEC`, reset every signal disposition, then clear
/// the signal mask.
pub(crate) fn install_pre_exec(cmd: &mut Command, bound: c_int, #[cfg(test)] test: PreExecTest) {
    let default_action = libc::sigaction {
        sa_sigaction: libc::SIG_DFL,
        sa_mask: 0,
        sa_flags: 0,
    };
    let empty: libc::sigset_t = 0;
    let child_setup = move || -> io::Result<()> {
        #[cfg(test)]
        test_entry(test);
        // SAFETY: `setsid` takes no arguments and is async-signal-safe.
        if unsafe { libc::setsid() } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `TIOCSCTTY` ignores its argument; Darwin's wrapper reads it as a pointer, so a
        // null pointer is passed. fd 0 is the slave std placed there.
        if unsafe {
            libc::ioctl(
                0,
                libc::c_ulong::from(libc::TIOCSCTTY),
                std::ptr::null_mut::<libc::c_void>(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut fd = 3;
        while fd < bound {
            // SAFETY: `F_GETFD` on any integer is defined; `EBADF` means nothing is open.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::EBADF) {
                    return Err(e);
                }
            } else if flags & libc::FD_CLOEXEC == 0 {
                // SAFETY: no object in this forked child uses the descriptor again: std
                // uses only close-on-exec descriptors from here to exec. A `close` error
                // is ignored: the descriptor is released either way.
                unsafe { libc::close(fd) };
            }
            fd += 1;
        }
        for sig in 1..32 {
            if sig == libc::SIGKILL || sig == libc::SIGSTOP {
                continue;
            }
            // SAFETY: `default_action` is a valid `sigaction` owned by the closure; the old
            // action is not requested.
            unsafe { libc::sigaction(sig, &raw const default_action, std::ptr::null_mut()) };
        }
        // SAFETY: `empty` is a valid signal set owned by the closure.
        if unsafe { libc::sigprocmask(libc::SIG_SETMASK, &raw const empty, std::ptr::null_mut()) }
            < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    };
    // SAFETY: the closure runs in the forked child of a multithreaded process. It captures
    // only `Copy` values prepared before the fork, allocates nothing, takes no lock, and
    // calls only async-signal-safe functions (`setsid`, `ioctl`, `fcntl`, `close`,
    // `sigaction`, `sigprocmask`, and in tests `kill`, `getpid`, `read`).
    unsafe { cmd.pre_exec(child_setup) };
}

#[cfg(test)]
fn test_entry(test: PreExecTest) {
    if let Some(sig) = test.raise {
        // SAFETY: `getpid` takes no arguments.
        let me = unsafe { libc::getpid() };
        // SAFETY: sends a signal to this process; no memory arguments.
        unsafe { libc::kill(me, sig) };
    }
    if let Some(fd) = test.park_fd {
        let mut byte = 0u8;
        loop {
            // SAFETY: reads at most one byte into a local.
            let n = unsafe { libc::read(fd, (&raw mut byte).cast(), 1) };
            if n >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
        }
    }
}

/// The sweep bound of §2.2 stage 4: `max(getdtablesize(), highest open fd + 1)`.
/// Called before the fork, where allocation is allowed.
pub(crate) fn fd_ceiling() -> io::Result<c_int> {
    // SAFETY: no arguments.
    let table = unsafe { getdtablesize() };
    let pid = c_int::try_from(std::process::id()).map_err(io::Error::other)?;
    let entry = size_of::<libc::proc_fdinfo>();
    // SAFETY: a size query: a null buffer of length 0.
    let need =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if need <= 0 {
        return Err(io::Error::last_os_error());
    }
    let cap = usize::try_from(need).map_err(io::Error::other)? / entry + 32;
    let bytes = c_int::try_from(cap * entry).map_err(io::Error::other)?;
    let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(cap);
    // SAFETY: the buffer has capacity for `cap` entries, `bytes` long.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            bytes,
        )
    };
    if got <= 0 {
        return Err(io::Error::last_os_error());
    }
    let n = (usize::try_from(got).map_err(io::Error::other)? / entry).min(cap);
    // SAFETY: the kernel wrote `n` complete entries (`got` bytes) into the buffer.
    unsafe { fds.set_len(n) };
    let highest = fds.iter().map(|f| f.proc_fd).max().unwrap_or(2);
    Ok(table.max(highest.saturating_add(1)))
}

/// `proc_listallpids` with a null buffer: an estimate of the number of processes.
pub(crate) fn pid_count() -> io::Result<usize> {
    // SAFETY: a size query: a null buffer of length 0.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return Err(io::Error::last_os_error());
    }
    usize::try_from(n).map_err(io::Error::other)
}

/// `proc_listallpids` into `capacity` entries: the pids and whether the buffer was full
/// (saturated). A result of 0 or less is an error.
pub(crate) fn pid_list(capacity: usize) -> io::Result<(Vec<u32>, bool)> {
    let mut pids: Vec<libc::pid_t> = vec![0; capacity];
    let bytes = c_int::try_from(capacity * size_of::<libc::pid_t>()).map_err(io::Error::other)?;
    // SAFETY: the buffer holds `capacity` initialized entries, `bytes` long.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if n <= 0 {
        let e = io::Error::last_os_error();
        return Err(if e.raw_os_error().is_some_and(|c| c != 0) {
            e
        } else {
            io::Error::other("proc_listallpids returned no process")
        });
    }
    let n = usize::try_from(n).map_err(io::Error::other)?;
    pids.truncate(n.min(capacity));
    let list = pids
        .into_iter()
        .filter_map(|p| u32::try_from(p).ok())
        .collect();
    Ok((list, n >= capacity))
}

/// A `PROC_PIDTBSDINFO` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BsdInfo {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    pub(crate) pgid: u32,
    pub(crate) zombie: bool,
    /// Microseconds since the Unix epoch.
    pub(crate) start: u64,
}

/// `proc_pidinfo(pid, PROC_PIDTBSDINFO, 1)`, which also reads zombies; the errno on
/// failure (`ESRCH` gone, `EPERM` unreadable).
pub(crate) fn bsd_info(pid: u32) -> Result<BsdInfo, i32> {
    let Ok(raw_pid) = c_int::try_from(pid) else {
        return Err(libc::ESRCH);
    };
    if raw_pid == 0 {
        return Err(libc::EPERM);
    }
    let size = size_of::<libc::proc_bsdinfo>();
    let Ok(len) = c_int::try_from(size) else {
        return Err(libc::EINVAL);
    };
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    // SAFETY: the buffer is one `proc_bsdinfo`, `len` bytes long.
    let n = unsafe {
        libc::proc_pidinfo(
            raw_pid,
            libc::PROC_PIDTBSDINFO,
            1,
            info.as_mut_ptr().cast(),
            len,
        )
    };
    if n != len {
        let code = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return Err(if n <= 0 && code != 0 { code } else { libc::EIO });
    }
    // SAFETY: `proc_bsdinfo` holds only integers and `c_char` arrays, so the zeroed value
    // is valid, and the kernel filled all `len` bytes.
    let info = unsafe { info.assume_init() };
    Ok(BsdInfo {
        pid: info.pbi_pid,
        ppid: info.pbi_ppid,
        pgid: info.pbi_pgid,
        zombie: info.pbi_status == SZOMB,
        start: info
            .pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec),
    })
}

/// `proc_pidinfo(pid, PROC_PIDT_SHORTBSDINFO, 1)`: whether the process is a zombie. Unlike
/// `PROC_PIDTBSDINFO`, this flavor is allowed for another user's process; the errno on
/// failure.
pub(crate) fn short_zombie(pid: u32) -> Result<bool, i32> {
    let Ok(raw_pid) = c_int::try_from(pid) else {
        return Err(libc::ESRCH);
    };
    if raw_pid == 0 {
        return Err(libc::EPERM);
    }
    let Ok(len) = c_int::try_from(size_of::<libc::proc_bsdshortinfo>()) else {
        return Err(libc::EINVAL);
    };
    let mut info = MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    // SAFETY: the buffer is one `proc_bsdshortinfo`, `len` bytes long.
    let n = unsafe {
        libc::proc_pidinfo(
            raw_pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            1,
            info.as_mut_ptr().cast(),
            len,
        )
    };
    if n != len {
        let code = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return Err(if n <= 0 && code != 0 { code } else { libc::EIO });
    }
    // SAFETY: `proc_bsdshortinfo` holds only integers and `c_char` arrays, so the zeroed
    // value is valid, and the kernel filled all `len` bytes.
    let info = unsafe { info.assume_init() };
    Ok(info.pbsi_status == SZOMB)
}

/// The pids of this process's children (tests only).
#[cfg(test)]
pub(crate) fn child_pids() -> Vec<u32> {
    let mut buf = vec![0 as libc::pid_t; 256];
    let bytes = c_int::try_from(buf.len() * size_of::<libc::pid_t>()).unwrap();
    let pid = c_int::try_from(std::process::id()).unwrap();
    // SAFETY: the buffer holds `buf.len()` entries, `bytes` long.
    let n = unsafe { libc::proc_listchildpids(pid, buf.as_mut_ptr().cast(), bytes) };
    buf.truncate(usize::try_from(n.max(0)).unwrap().min(256));
    buf.into_iter()
        .filter_map(|p| u32::try_from(p).ok())
        .collect()
}

/// The open fds of `pid` (tests only).
#[cfg(test)]
pub(crate) fn open_fds(pid: u32) -> io::Result<Vec<i32>> {
    let pid = c_int::try_from(pid).map_err(io::Error::other)?;
    let entry = size_of::<libc::proc_fdinfo>();
    let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(4096);
    let bytes = c_int::try_from(4096 * entry).map_err(io::Error::other)?;
    // SAFETY: the buffer has capacity for 4096 entries, `bytes` long.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            bytes,
        )
    };
    if got <= 0 {
        return Err(io::Error::last_os_error());
    }
    let n = (usize::try_from(got).map_err(io::Error::other)? / entry).min(4096);
    // SAFETY: the kernel wrote `n` complete entries.
    unsafe { fds.set_len(n) };
    let mut list: Vec<i32> = fds.iter().map(|f| f.proc_fd).collect();
    list.sort_unstable();
    Ok(list)
}

#[cfg(test)]
mod tests {
    //! Spawn hygiene, the spawn lock, and `bsd_info`. Tests that change process-wide state
    //! (signal dispositions, the fd limit, a fixed high fd) run in a helper process: the
    //! test binary re-executed with `--exact` and a flag in that child's environment only.

    use std::ffi::c_int;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::backend::{PtyRuntime, SpawnSpec, SpawnedParts};
    use crate::spawn::{spawn_lock_held, with_spawn_lock};
    use crate::unix::UnixPty;
    use crate::unix::tests::{Hooks, Out, Stage, TempDir, serial};

    const BOUND: Duration = Duration::from_secs(10);
    const FLAG: &str = "CHALDEA_SYS_HELPER";

    /// True in the helper process for `name`; otherwise runs that helper and asserts it
    /// passed.
    fn in_helper(name: &str) -> bool {
        if std::env::var(FLAG).as_deref() == Ok(name) {
            return true;
        }
        let _s = serial();
        let exe = std::env::current_exe().unwrap();
        let mut child = with_spawn_lock(|| {
            Command::new(exe)
                .args([
                    "--exact",
                    &format!("unix::sys::tests::{name}"),
                    "--nocapture",
                ])
                .env(FLAG, name)
                .stdin(Stdio::null())
                .spawn()
        })
        .unwrap();
        let end = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            if Instant::now() >= end {
                let _ = child.kill();
                let _ = child.wait();
                panic!("helper {name} did not finish");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "helper {name}: {status}");
        false
    }

    fn spec(argv: &[&str], dir: &TempDir) -> SpawnSpec {
        SpawnSpec {
            argv: argv.iter().map(Into::into).collect(),
            cwd: dir.0.clone(),
            env: Vec::new(),
            size: crate::backend::PtySize::new(80, 24).unwrap(),
        }
    }

    fn spawn(pty: &UnixPty, argv: &[&str], dir: &TempDir) -> SpawnedParts {
        let out = Arc::new(Out::default());
        pty.spawn(&spec(argv, dir), out.sink())
            .unwrap()
            .into_parts()
    }

    fn sig(pid: u32, s: c_int) {
        // SAFETY: sends a signal to a pid; no memory arguments.
        unsafe { libc::kill(i32::try_from(pid).unwrap(), s) };
    }

    /// An fd at `at` without `FD_CLOEXEC`.
    fn plain_fd_at(at: c_int) {
        let f = std::fs::File::open("/dev/null").unwrap();
        // SAFETY: duplicates an open descriptor onto `at`; no memory arguments.
        let r = unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&f), at) };
        assert_eq!(r, at);
        // SAFETY: reads the flags of the descriptor just created.
        let flags = unsafe { libc::fcntl(at, libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, 0);
    }

    /// Blocks `signal` in the calling thread only.
    fn block_in_this_thread(signal: c_int) {
        let mut set: libc::sigset_t = 0;
        // SAFETY: `set` is a valid local signal set.
        unsafe { libc::sigaddset(&raw mut set, signal) };
        // SAFETY: `set` is a valid signal set; the old mask is not requested.
        let r =
            unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) };
        assert_eq!(r, 0);
    }

    // ---- CL-6 ----

    #[test]
    fn child_gets_no_extra_fd_and_default_signals() {
        if !in_helper("child_gets_no_extra_fd_and_default_signals") {
            return;
        }
        std::thread::spawn(|| plain_fd_at(200)).join().unwrap();
        block_in_this_thread(libc::SIGHUP);
        // SAFETY: sets a disposition process-wide in this helper process only.
        unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
        let dir = TempDir::new("hygiene");
        let pty = UnixPty::default();
        let a = spawn(&pty, &["/bin/sleep", "30"], &dir);
        let pid = a.child.identity().pid;
        assert_eq!(open_fds(pid).unwrap(), vec![0, 1, 2]);
        sig(pid, libc::SIGHUP);
        let e = a.child.wait_timeout(BOUND).unwrap().unwrap();
        assert_eq!(e.signal, Some(libc::SIGHUP));
        let b = spawn(&pty, &["/bin/sleep", "30"], &dir);
        sig(b.child.identity().pid, libc::SIGINT);
        let e = b.child.wait_timeout(BOUND).unwrap().unwrap();
        assert_eq!(e.signal, Some(libc::SIGINT));
        let _ = (a.teardown(), b.teardown());
    }

    #[test]
    fn fd_above_a_lowered_limit_is_closed() {
        if !in_helper("fd_above_a_lowered_limit_is_closed") {
            return;
        }
        plain_fd_at(400);
        let max = rustix::process::getrlimit(rustix::process::Resource::Nofile).maximum;
        rustix::process::setrlimit(
            rustix::process::Resource::Nofile,
            rustix::process::Rlimit {
                current: Some(128),
                maximum: max,
            },
        )
        .unwrap();
        // SAFETY: no arguments.
        assert_eq!(unsafe { getdtablesize() }, 128);
        let dir = TempDir::new("limit");
        let p = spawn(&UnixPty::default(), &["/bin/sleep", "30"], &dir);
        assert_eq!(open_fds(p.child.identity().pid).unwrap(), vec![0, 1, 2]);
        let _ = p.teardown();
    }

    /// A daemon handler that must never run in the child: it holds the child for 3 s, so a
    /// spawn that runs it misses the 1 s bound, and a broken build still ends the child.
    extern "C" fn blocking_handler(_: c_int) {
        // SAFETY: `sleep` is async-signal-safe and takes no pointer.
        unsafe { libc::sleep(3) };
    }

    #[test]
    fn pending_signal_does_not_run_a_daemon_handler() {
        if !in_helper("pending_signal_does_not_run_a_daemon_handler") {
            return;
        }
        let action = libc::sigaction {
            sa_sigaction: blocking_handler as *const () as usize,
            sa_mask: 0,
            sa_flags: 0,
        };
        // SAFETY: `action` is a valid `sigaction`; this sets a disposition process-wide in
        // this helper process only.
        let r = unsafe { libc::sigaction(libc::SIGWINCH, &raw const action, std::ptr::null_mut()) };
        assert_eq!(r, 0);
        block_in_this_thread(libc::SIGWINCH);
        let dir = TempDir::new("pending");
        let pty = UnixPty {
            hooks: Hooks {
                pre_exec: PreExecTest {
                    raise: Some(libc::SIGWINCH),
                    park_fd: None,
                },
                ..Hooks::default()
            },
        };
        let t = Instant::now();
        let p = spawn(&pty, &["/bin/sleep", "30"], &dir);
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        assert_eq!(
            p.child.wait_timeout(Duration::from_millis(100)).unwrap(),
            None
        );
        let _ = p.teardown();
    }

    // ---- CL-19 ----

    #[test]
    fn bsd_info_sees_zombies_and_reports_eperm() {
        let _s = serial();
        assert_eq!(bsd_info(1), Err(libc::EPERM));
        let mut child = with_spawn_lock(|| Command::new("/usr/bin/true").spawn()).unwrap();
        let pid = child.id();
        let end = Instant::now() + BOUND;
        while crate::unix::child::waitid_nowait(pid).unwrap().is_none() {
            assert!(Instant::now() < end);
            std::thread::yield_now();
        }
        let info = bsd_info(pid).unwrap();
        assert!(info.zombie);
        assert_eq!(info.pid, pid);
        child.wait().unwrap();
        assert_eq!(bsd_info(pid), Err(libc::ESRCH));
    }

    // ---- CL-24 ----

    /// The write end of a park gate: opens it on drop, a panic included. A parked fork
    /// holds its own copy of this end, so it would never see EOF.
    struct Gate(std::io::PipeWriter);

    impl Gate {
        fn open(self) {}
    }

    impl Drop for Gate {
        fn drop(&mut self) {
            let _ = std::io::Write::write_all(&mut self.0, b"x");
        }
    }

    /// A `Command` whose child parks before `exec` until `gate` is readable.
    fn parked(gate: c_int) -> Command {
        let mut cmd = Command::new("/usr/bin/true");
        let park = move || -> io::Result<()> {
            let mut b = 0u8;
            // SAFETY: async-signal-safe `read` of one byte into a local.
            unsafe { libc::read(gate, (&raw mut b).cast(), 1) };
            Ok(())
        };
        // SAFETY: the closure captures a `c_int`, allocates nothing, takes no lock, and
        // calls only `read`.
        unsafe { cmd.pre_exec(park) };
        cmd
    }

    fn wait_children(n: usize) {
        let end = Instant::now() + BOUND;
        while child_pids().len() < n {
            assert!(Instant::now() < end, "{:?}", child_pids());
            std::thread::yield_now();
        }
    }

    #[test]
    fn pty_spawn_waits_for_a_helper_spawn() {
        let _s = serial();
        let (gate_r, gate_w) = std::io::pipe().unwrap();
        let gate_w = Gate(gate_w);
        let gate = std::os::fd::AsRawFd::as_raw_fd(&gate_r);
        let base = child_pids().len();
        let helper = std::thread::spawn(move || {
            let mut cmd = parked(gate);
            let r = with_spawn_lock(|| cmd.spawn());
            drop(gate_r);
            r
        });
        // The helper is inside `Command::spawn`: its child exists and is parked.
        wait_children(base + 1);
        let reached = Arc::new(AtomicBool::new(false));
        let r2 = reached.clone();
        let pty = UnixPty {
            hooks: Hooks {
                stage: Some(Box::new(move |s| {
                    if matches!(s, Stage::LockAcquired) {
                        r2.store(true, Ordering::Release);
                    }
                    false
                })),
                ..Hooks::default()
            },
        };
        let dir = TempDir::new("lock1");
        let sp = spec(&["/usr/bin/true"], &dir);
        let (tx, rx) = mpsc::channel();
        let spawner = std::thread::spawn(move || {
            let out = Arc::new(Out::default());
            let r = pty
                .spawn(&sp, out.sink())
                .map(crate::backend::SpawnedPty::into_parts)
                .map_err(|e| e.to_string());
            tx.send(()).unwrap();
            r
        });
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        assert!(
            !reached.load(Ordering::Acquire),
            "the PTY spawn ran while the helper held the lock"
        );
        gate_w.open();
        rx.recv_timeout(BOUND).unwrap();
        assert!(reached.load(Ordering::Acquire));
        let mut c = helper.join().unwrap().unwrap();
        c.wait().unwrap();
        let _ = spawner.join().unwrap().unwrap().teardown();
    }

    #[test]
    fn helper_spawn_waits_for_a_pty_spawn() {
        let _s = serial();
        let (gate_r, gate_w) = std::io::pipe().unwrap();
        let gate_w = Gate(gate_w);
        let gate = std::os::fd::AsRawFd::as_raw_fd(&gate_r);
        let base = child_pids().len();
        let pty = UnixPty {
            hooks: Hooks {
                pre_exec: PreExecTest {
                    raise: None,
                    park_fd: Some(gate),
                },
                ..Hooks::default()
            },
        };
        let dir = TempDir::new("lock2");
        let sp = spec(&["/usr/bin/true"], &dir);
        let spawner = std::thread::spawn(move || {
            let out = Arc::new(Out::default());
            let r = pty
                .spawn(&sp, out.sink())
                .map(crate::backend::SpawnedPty::into_parts)
                .map_err(|e| e.to_string());
            drop(gate_r);
            r
        });
        // The PTY spawn is inside `Command::spawn`: its child exists and is parked.
        wait_children(base + 1);
        let (tx, rx) = mpsc::channel();
        let helper = std::thread::spawn(move || {
            let r = with_spawn_lock(|| Command::new("/usr/bin/true").spawn());
            tx.send(()).unwrap();
            r
        });
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        assert_eq!(
            child_pids().len(),
            base + 1,
            "the helper forked while the PTY spawn held the lock"
        );
        gate_w.open();
        rx.recv_timeout(BOUND).unwrap();
        let mut c = helper.join().unwrap().unwrap();
        c.wait().unwrap();
        let _ = spawner.join().unwrap().unwrap().teardown();
    }

    #[test]
    fn wake_pipe_is_created_under_the_lock() {
        let _s = serial();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let pty = UnixPty {
            hooks: Hooks {
                stage: Some(Box::new(move |s| {
                    if matches!(s, Stage::WakeUnderLock | Stage::LockAcquired) {
                        s2.lock()
                            .unwrap()
                            .push((format!("{s:?}"), spawn_lock_held()));
                    }
                    false
                })),
                ..Hooks::default()
            },
        };
        let dir = TempDir::new("wake");
        let p = spawn(&pty, &["/usr/bin/true"], &dir);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                ("WakeUnderLock".to_owned(), true),
                ("LockAcquired".to_owned(), true)
            ]
        );
        assert!(!spawn_lock_held());
        let _ = p.teardown();
    }
}
