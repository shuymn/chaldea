//! The only module allowed `unsafe` (`docs/design/pty-unix.md` §2.7). Every function here
//! is safe to call: the `pre_exec` body of the spawn, the fd ceiling, libproc, and the
//! process-level calls of `host` (signal state, extended ACLs).

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

/// `_exit(code)`: ends the process now; no `atexit` handler runs and no stdio stream is
/// flushed.
pub(crate) fn exit_now(code: c_int) -> ! {
    // SAFETY: `_exit` accepts any status and does not return.
    unsafe { libc::_exit(code) }
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
    /// The image name, NUL-padded: `pbi_name` (at most 31 bytes), else `pbi_comm` (at most
    /// 15) when `pbi_name` is empty.
    pub(crate) name: [u8; 32],
}

/// `pbi_name` when it is not empty, else `pbi_comm`, as bytes padded with NULs.
fn image_name(name: &[libc::c_char; 32], comm: &[libc::c_char; 16]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let src: &[libc::c_char] = if name[0] == 0 { comm } else { name };
    for (o, c) in out.iter_mut().zip(src) {
        *o = c.cast_unsigned();
    }
    out
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
        name: image_name(&info.pbi_name, &info.pbi_comm),
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

// ---- Process-level primitives of `host` (docs/design/daemon-lifecycle.md §2.2) ----

/// `sys/acl.h`: the extended ACL type, the entry iteration ids, and the allow tag.
const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
const ACL_FIRST_ENTRY: c_int = 0;
const ACL_NEXT_ENTRY: c_int = -1;
const ACL_EXTENDED_ALLOW: c_int = 1;

unsafe extern "C" {
    /// `sys/acl.h` (libSystem); libc 0.2.191 has no Apple ACL bindings.
    fn acl_get_link_np(path: *const std::ffi::c_char, ty: c_int) -> *mut std::ffi::c_void;
    fn acl_get_fd_np(fd: c_int, ty: c_int) -> *mut std::ffi::c_void;
    fn acl_get_entry(
        acl: *mut std::ffi::c_void,
        entry_id: c_int,
        entry: *mut *mut std::ffi::c_void,
    ) -> c_int;
    fn acl_get_tag_type(entry: *mut std::ffi::c_void, tag: *mut c_int) -> c_int;
    fn acl_free(obj: *mut std::ffi::c_void) -> c_int;
}

/// An ACL `acl_get_*_np` returned; freed once on drop.
struct Acl(*mut std::ffi::c_void);

impl Drop for Acl {
    fn drop(&mut self) {
        // SAFETY: the pointer is an ACL the library allocated and nothing else frees.
        let _ = unsafe { acl_free(self.0) };
    }
}

/// Whether an ACL pointer (or the error of its lookup) has an entry of type allow; a null
/// pointer with `ENOENT` is no extended ACL.
fn acl_has_allow(acl: *mut std::ffi::c_void) -> io::Result<bool> {
    if acl.is_null() {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::ENOENT) {
            Ok(false)
        } else {
            Err(e)
        };
    }
    let acl = Acl(acl);
    let mut id = ACL_FIRST_ENTRY;
    loop {
        let mut entry = std::ptr::null_mut();
        // SAFETY: `acl.0` is a live ACL; `entry` is a local out-pointer.
        if unsafe { acl_get_entry(acl.0, id, &raw mut entry) } != 0 {
            return Ok(false);
        }
        let mut tag: c_int = 0;
        // SAFETY: `entry` was just returned for this live ACL; `tag` is a local.
        if unsafe { acl_get_tag_type(entry, &raw mut tag) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if tag == ACL_EXTENDED_ALLOW {
            return Ok(true);
        }
        id = ACL_NEXT_ENTRY;
    }
}

/// Whether the extended ACL of `path` (not following a final symlink) has an allow entry.
pub(crate) fn acl_allows_link(path: &std::ffi::CStr) -> io::Result<bool> {
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    let acl = unsafe { acl_get_link_np(path.as_ptr(), ACL_TYPE_EXTENDED) };
    acl_has_allow(acl)
}

/// Whether the extended ACL of the open file `fd` has an allow entry.
pub(crate) fn acl_allows_fd(fd: std::os::fd::BorrowedFd<'_>) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: `fd` is a borrowed, open descriptor for the duration of the call.
    let acl = unsafe { acl_get_fd_np(fd.as_raw_fd(), ACL_TYPE_EXTENDED) };
    acl_has_allow(acl)
}

fn sig_bit(sig: c_int) -> libc::sigset_t {
    // Darwin's `sigaddset`: bit `sig - 1` of a 32-bit set.
    1u32 << (sig - 1)
}

/// The signals `prepare_process` unblocks.
const RESET_SIGNALS: [c_int; 4] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGCHLD];

/// `SIGCHLD` to `SIG_DFL` with no flags, then `TERM`, `INT`, `HUP`, `CHLD` unblocked in the
/// calling thread's mask (inherited by threads created later).
pub(crate) fn reset_process_signals() -> io::Result<()> {
    let default_action = libc::sigaction {
        sa_sigaction: libc::SIG_DFL,
        sa_mask: 0,
        sa_flags: 0,
    };
    // SAFETY: `default_action` is a valid action; the old action is not requested.
    if unsafe {
        libc::sigaction(
            libc::SIGCHLD,
            &raw const default_action,
            std::ptr::null_mut(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let set = RESET_SIGNALS.iter().fold(0, |m, s| m | sig_bit(*s));
    // SAFETY: `set` is a valid set; the old mask is not requested.
    let rc =
        unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &raw const set, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

/// The `SIGCHLD` handler value and flags, and the calling thread's blocked set.
pub(crate) fn signal_state() -> io::Result<(usize, c_int, libc::sigset_t)> {
    let mut old = MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: a null new action only reads; `old` is a writable `sigaction`.
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), old.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `sigaction` holds only integers, so the zeroed value is valid, and the call
    // filled it.
    let old = unsafe { old.assume_init() };
    let mut mask: libc::sigset_t = 0;
    // SAFETY: a null new set only reads; `mask` is a writable set.
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &raw mut mask) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok((old.sa_sigaction, old.sa_flags, mask))
}

/// Whether `sig` is in `mask`.
pub(crate) fn in_set(mask: libc::sigset_t, sig: c_int) -> bool {
    mask & sig_bit(sig) != 0
}

/// Test helper: `SIGCHLD` ignored (the state a careless parent can leave).
#[cfg(feature = "fake")]
pub(crate) fn ignore_sigchld() -> io::Result<()> {
    let ignore = libc::sigaction {
        sa_sigaction: libc::SIG_IGN,
        sa_mask: 0,
        sa_flags: 0,
    };
    // SAFETY: `ignore` is a valid action; the old action is not requested.
    if unsafe { libc::sigaction(libc::SIGCHLD, &raw const ignore, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Test helper: blocks `signals` in the calling thread's mask.
#[cfg(feature = "fake")]
pub(crate) fn block_signals(signals: &[c_int]) -> io::Result<()> {
    let set = signals.iter().fold(0, |m, s| m | sig_bit(*s));
    // SAFETY: `set` is a valid set; the old mask is not requested.
    let rc =
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

/// Which side of a `fork` the caller is on.
pub(crate) enum Forked {
    Parent,
    Child,
}

/// `fork()`. Call only while the process has one thread (`host::leave_launchd_job`, from
/// `dispatch` before process setup), so the child holds no lock another thread owned.
pub(crate) fn fork() -> io::Result<Forked> {
    // SAFETY: the caller guarantees a single-threaded process; the child continues normal
    // Rust execution, which is sound without other threads.
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()),
        0 => Ok(Forked::Child),
        _ => Ok(Forked::Parent),
    }
}

/// One `kevent` change: `filter` on `ident`, added for this wait only.
fn kevent_change(ident: usize, filter: i16, fflags: u32) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    }
}

/// Blocks for at most `timeout` until `fd` is readable or the process `pid` exits, with a
/// kqueue of its own. Returns early, without an error, when the process is already gone
/// (its registration fails with `ESRCH`) or a signal interrupts the wait; the caller looks
/// again either way.
///
/// # Errors
///
/// The kqueue cannot be created or waited on.
pub(crate) fn wait_readable_or_exit(
    fd: Option<std::os::fd::BorrowedFd<'_>>,
    pid: Option<u32>,
    timeout: std::time::Duration,
) -> io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let mut changes = Vec::with_capacity(2);
    if let Some(fd) = fd {
        let ident = usize::try_from(fd.as_raw_fd()).map_err(io::Error::other)?;
        changes.push(kevent_change(ident, libc::EVFILT_READ, 0));
    }
    if let Some(pid) = pid {
        let ident = usize::try_from(pid).map_err(io::Error::other)?;
        changes.push(kevent_change(ident, libc::EVFILT_PROC, libc::NOTE_EXIT));
    }
    // SAFETY: `kqueue` takes no arguments.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `kq` is a new descriptor that nothing else owns.
    let kq = unsafe { OwnedFd::from_raw_fd(kq) };
    let mut events = [kevent_change(0, 0, 0); 2];
    let ts = libc::timespec {
        tv_sec: libc::time_t::try_from(timeout.as_secs()).unwrap_or(libc::time_t::MAX),
        tv_nsec: libc::c_long::from(timeout.subsec_nanos()),
    };
    let n_changes = c_int::try_from(changes.len()).map_err(io::Error::other)?;
    let n_events = c_int::try_from(events.len()).map_err(io::Error::other)?;
    // SAFETY: `changes` holds `n_changes` initialized entries and `events` room for
    // `n_events`; both outlive the call, `ts` is a valid timespec, and `kq` is open.
    let n = unsafe {
        libc::kevent(
            kq.as_raw_fd(),
            changes.as_ptr(),
            n_changes,
            events.as_mut_ptr(),
            n_events,
            &raw const ts,
        )
    };
    if n < 0 {
        let e = io::Error::last_os_error();
        if matches!(e.raw_os_error(), Some(libc::EINTR | libc::ESRCH)) {
            return Ok(());
        }
        return Err(e);
    }
    Ok(())
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

    /// What [`mark_exit_handlers`]'s handler writes to fd 2.
    const EXIT_MARK: &str = "EXIT-HANDLER-RAN\n";

    extern "C" fn exit_marker() {
        let msg = EXIT_MARK.as_bytes();
        // SAFETY: writes a static buffer of `msg.len()` bytes to fd 2.
        let _ = unsafe { libc::write(2, msg.as_ptr().cast(), msg.len()) };
    }

    /// Registers an `atexit` handler that writes [`EXIT_MARK`] to fd 2, so a test sees whether
    /// a process ended through the C exit path.
    fn mark_exit_handlers() -> io::Result<()> {
        // SAFETY: registers a handler that only writes a static buffer to fd 2.
        if unsafe { libc::atexit(exit_marker) } == 0 {
            Ok(())
        } else {
            Err(io::Error::other("atexit refused the handler"))
        }
    }

    /// `exit_now` ends the process with its code and runs no `atexit` handler;
    /// `std::process::exit`, the control, runs it.
    #[test]
    fn exit_now_skips_exit_handlers() {
        const MODE: &str = "CHALDEA_EXIT_NOW_MODE";
        if let Ok(mode) = std::env::var(MODE) {
            mark_exit_handlers().unwrap();
            if mode == "now" {
                exit_now(7);
            }
            std::process::exit(7);
        }
        for (mode, ran) in [("now", false), ("std", true)] {
            let exe = std::env::current_exe().unwrap();
            let mut child = with_spawn_lock(|| {
                Command::new(exe)
                    .args([
                        "--exact",
                        "unix::sys::tests::exit_now_skips_exit_handlers",
                        "--nocapture",
                    ])
                    .env(MODE, mode)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
            })
            .unwrap();
            let mut err = child.stderr.take().unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = std::io::Read::read_to_string(&mut err, &mut s);
                let _ = tx.send(s);
            });
            let end = Instant::now() + Duration::from_secs(30);
            let status = loop {
                if let Some(s) = child.try_wait().unwrap() {
                    break s;
                }
                if Instant::now() >= end {
                    let _ = child.kill();
                    let reap = Instant::now() + Duration::from_secs(30);
                    while matches!(child.try_wait(), Ok(None)) && Instant::now() < reap {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    panic!("{mode}: the helper did not exit");
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let err = rx
                .recv_timeout(Duration::from_secs(30))
                .expect("the helper's stderr did not close");
            assert_eq!(status.code(), Some(7), "{mode}: {err}");
            assert_eq!(err.contains(EXIT_MARK), ran, "{mode}: {err}");
        }
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

    /// Both record flavours (F10, F26): pid 1 (root) refuses the full record and allows
    /// the short one, so a lookup falls back to `Unreadable`; an exited, unreaped child is
    /// a zombie in both, and gone from both after the reap.
    #[test]
    fn libproc_records_read_zombies_and_another_users_process() {
        let _s = serial();
        assert_eq!(bsd_info(1), Err(libc::EPERM));
        assert_eq!(short_zombie(1), Ok(false));
        assert_eq!(
            crate::unix::table::lookup(1),
            crate::unix::table::Lookup::Unreadable
        );
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
        assert_eq!(short_zombie(pid), Ok(true));
        child.wait().unwrap();
        assert_eq!(bsd_info(pid), Err(libc::ESRCH));
        assert_eq!(short_zombie(pid), Err(libc::ESRCH));
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
        let s3 = seen.clone();
        let pty = UnixPty {
            hooks: Hooks {
                // The constructor itself records whether the lock is held when it runs.
                pipe: Some(Box::new(move || {
                    s3.lock()
                        .unwrap()
                        .push(("pipe()".to_owned(), spawn_lock_held()));
                    std::io::pipe()
                })),
                stage: Some(Box::new(move |s| {
                    if matches!(s, Stage::LockAcquired) {
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
                ("pipe()".to_owned(), true),
                ("LockAcquired".to_owned(), true)
            ]
        );
        assert!(!spawn_lock_held());
        let _ = p.teardown();
    }
}
