//! Unix (macOS) side of the OS boundary: process enumeration via libproc,
//! `killpg` tree kill, termios raw mode, and file permission checks.
//! Signals appear here only as the kill mechanism and as a simulated Ctrl-C.

#[cfg(not(target_os = "macos"))]
compile_error!("the spike's Unix process enumeration is implemented for macOS only");

use std::ffi::{CStr, OsString};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use super::ProcInfo;

pub const SPAWN_TREE_ROLES: &[&str] = &["plain", "ignore-term", "double-forked", "setsid"];

pub fn default_root() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| OsString::from("/tmp"));
    PathBuf::from(home).join(".chs")
}

fn bsdinfo(pid: u32) -> Option<libc::proc_bsdinfo> {
    // SAFETY: proc_bsdinfo is plain old data; proc_pidinfo fills at most `size` bytes.
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        );
        if n == size { Some(info) } else { None }
    }
}

fn cstr_field(field: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn to_info(info: &libc::proc_bsdinfo) -> ProcInfo {
    let name = {
        let n = cstr_field(&info.pbi_name);
        if n.is_empty() {
            cstr_field(&info.pbi_comm)
        } else {
            n
        }
    };
    // SAFETY: getsid has no memory-safety preconditions.
    let sid = unsafe { libc::getsid(info.pbi_pid as libc::pid_t) };
    ProcInfo {
        pid: info.pbi_pid,
        ppid: info.pbi_ppid,
        pgid: Some(info.pbi_pgid),
        sid: if sid > 0 { Some(sid as u32) } else { None },
        start_time: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
        name,
        zombie: info.pbi_status == libc::SZOMB,
    }
}

pub fn proc_info(pid: u32) -> Option<ProcInfo> {
    bsdinfo(pid).map(|i| to_info(&i))
}

pub fn list_processes() -> io::Result<Vec<ProcInfo>> {
    // SAFETY: a null buffer asks for the pid count; the second call fills our buffer.
    let pids = unsafe {
        let n = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if n <= 0 {
            return Err(io::Error::last_os_error());
        }
        let cap = n as usize + 64;
        let mut buf = vec![0 as libc::pid_t; cap];
        let got = libc::proc_listallpids(
            buf.as_mut_ptr() as *mut libc::c_void,
            (cap * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        );
        if got <= 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(got as usize);
        buf
    };
    Ok(pids
        .into_iter()
        .filter(|p| *p > 0)
        .filter_map(|p| proc_info(p as u32))
        .collect())
}

/// Sends SIGKILL to `pid` only if it is still the process we recorded.
pub fn force_kill(pid: u32, start_time: u64) -> bool {
    if !super::is_alive(pid, start_time) {
        return false;
    }
    // SAFETY: plain syscall; pid identity was rechecked just above.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) == 0 }
}

/// Simulated user Ctrl-C for B-10 (the harness never uses signals as IPC).
pub fn send_interrupt(pid: u32) -> io::Result<()> {
    // SAFETY: plain syscall.
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Makes `cmd` interruptible by `send_interrupt` (no-op on Unix).
pub fn interruptible(_cmd: &mut Command) {}

/// Non-interactive harness launch: no console window (Windows only).
pub fn no_window(_cmd: &mut Command) {}

/// Harness-established ownership of the launched tree: the process group
/// portable-pty created with `setsid()` (pgid = sid = child pid).
pub struct Tree {
    pgid: u32,
}

impl Tree {
    pub fn attach(child: &dyn portable_pty::Child) -> io::Result<Tree> {
        let pid = child
            .process_id()
            .ok_or_else(|| io::Error::other("child has no pid"))?;
        // SAFETY: plain syscall.
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        if pgid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pgid as u32 != pid {
            return Err(io::Error::other(format!(
                "child {pid} is not a group leader (pgid {pgid})"
            )));
        }
        Ok(Tree { pgid: pid })
    }

    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({"mechanism": "killpg", "pgid": self.pgid})
    }

    /// No live (non-zombie) member left; zombies of unreaped children still count as members
    /// for `killpg(0)`, so fall back to a table scan.
    fn group_empty(&self) -> bool {
        // SAFETY: signal 0 only probes for existence.
        let r = unsafe { libc::killpg(self.pgid as libc::pid_t, 0) };
        if r != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return true;
        }
        list_processes().is_ok_and(|ps| !ps.iter().any(|p| p.pgid == Some(self.pgid) && !p.zombie))
    }

    /// SIGHUP, SIGTERM, `grace`, SIGKILL to the whole group.
    pub fn kill(&self, grace: Duration) -> Vec<String> {
        let mut steps = Vec::new();
        for (sig, name) in [(libc::SIGHUP, "SIGHUP"), (libc::SIGTERM, "SIGTERM")] {
            // SAFETY: plain syscall on a group we established.
            let r = unsafe { libc::killpg(self.pgid as libc::pid_t, sig) };
            steps.push(format!("killpg({},{name})={r}", self.pgid));
        }
        let end = Instant::now() + grace;
        while Instant::now() < end {
            if self.group_empty() {
                steps.push("group empty before SIGKILL".into());
                return steps;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // SAFETY: plain syscall.
        let r = unsafe { libc::killpg(self.pgid as libc::pid_t, libc::SIGKILL) };
        steps.push(format!("killpg({},SIGKILL)={r}", self.pgid));
        steps
    }

    /// Unix has no job membership list; the scan in `session` covers pgid/sid.
    pub fn members(&self) -> Vec<u32> {
        Vec::new()
    }
}

/// Starts `cmd` so that it leaves the caller's session (`setsid`).
pub fn spawn_escapee(cmd: &mut Command) -> io::Result<(Child, &'static str)> {
    // SAFETY: setsid is async-signal-safe and runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().map(|c| (c, "setsid"))
}

pub fn ignore_term_signals() {
    // SAFETY: installing SIG_IGN has no preconditions.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

/// Raw-mode guard for FakeCli's stdin (termios).
pub struct RawMode {
    orig: libc::termios,
}

pub fn enable_raw_mode() -> io::Result<RawMode> {
    // SAFETY: termios is plain data; tcgetattr fills it.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) != 0 {
            return Err(io::Error::last_os_error());
        }
        let orig = t;
        libc::cfmakeraw(&mut t);
        if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(RawMode { orig })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restores the attributes read in enable_raw_mode.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.orig);
        }
    }
}

pub fn enable_vt_output() {}

pub fn stdin_reader() -> Box<dyn Read + Send> {
    Box::new(io::stdin())
}

pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::other(format!("{} is a symlink", path.display())));
    }
    // SAFETY: geteuid has no preconditions.
    if meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn set_private_file(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

pub fn file_mode(path: &Path) -> Option<u32> {
    std::fs::metadata(path).ok().map(|m| m.mode() & 0o7777)
}

pub fn set_file_mode(path: &Path, mode: Option<u32>) -> io::Result<()> {
    match mode {
        Some(m) => std::fs::set_permissions(path, std::fs::Permissions::from_mode(m)),
        None => Ok(()),
    }
}

pub fn make_executable(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// Refuses a path owned by someone else or writable by group/others.
pub fn check_secure_path(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink", path.display()));
    }
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if meta.uid() != uid {
        return Err(format!(
            "{} is owned by uid {}, not {uid}",
            path.display(),
            meta.uid()
        ));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is group- or world-writable (mode {:o})",
            path.display(),
            meta.mode() & 0o777
        ));
    }
    Ok(())
}

pub fn os_version() -> String {
    // SAFETY: utsname is plain data filled by uname.
    unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut u) == 0 {
            let s = CStr::from_ptr(u.release.as_ptr()).to_string_lossy();
            let m = CStr::from_ptr(u.machine.as_ptr()).to_string_lossy();
            return format!("Darwin {s} {m}");
        }
    }
    "unknown".into()
}

// ---- Windows-only probes: refuse on Unix ----

pub fn conpty_module() -> Option<String> {
    None
}

pub fn self_job_info() -> serde_json::Value {
    serde_json::json!({"in_job": false, "applicable": false})
}

pub fn console_window_present() -> Option<bool> {
    None
}

pub fn console_code_page() -> Option<u32> {
    None
}

pub fn spawn_detached(_cmd: &mut Command) -> Result<(Child, bool), String> {
    Err("windows_only".into())
}

pub struct WindowObserver;

impl WindowObserver {
    pub fn start() -> Result<WindowObserver, String> {
        Err("windows_only".into())
    }
    /// `(pid, ppid at event time)` of every shown window, and observer health.
    pub fn stop(self) -> super::ObserverStop {
        (Vec::new(), Err("windows_only".into()))
    }
}

pub struct JobFirst;

impl JobFirst {
    pub fn launch(
        _argv: &[OsString],
        _env: &[(String, String)],
        _remove: &[String],
        _cwd: &Path,
    ) -> Result<(JobFirst, String), String> {
        Err("windows_only".into())
    }
    pub fn pid(&self) -> u32 {
        0
    }
    pub fn members(&self) -> Result<Vec<u32>, String> {
        Err("windows_only".into())
    }
    pub fn start_time(&self) -> u64 {
        0
    }
    pub fn terminate_job(&mut self) -> Vec<String> {
        Vec::new()
    }
    pub fn close_conpty(&mut self) {}
}
