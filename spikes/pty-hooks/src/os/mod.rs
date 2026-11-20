//! The OS boundary. Every OS-specific PTY/process/console call lives under
//! this module; the rest of the crate is OS-agnostic and has no `cfg`.

use serde::Serialize;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// `WindowObserver::stop`: `(pid, ppid at event time)` per shown window, and
/// the observer's health (an error means the events may be incomplete).
pub type ObserverStop = (Vec<(u32, Option<u32>)>, Result<(), String>);

/// One row of the process table. `pgid`/`sid` are Unix-only.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: Option<u32>,
    pub sid: Option<u32>,
    /// Opaque, monotonic per boot; used with `pid` to detect pid reuse.
    pub start_time: u64,
    pub name: String,
    pub zombie: bool,
}

/// True when `pid` still names the process that started at `start_time`
/// and has not exited (zombies count as dead).
pub fn is_alive(pid: u32, start_time: u64) -> bool {
    match proc_info(pid) {
        Some(p) => p.start_time == start_time && !p.zombie,
        None => false,
    }
}

/// Ancestry chain starting at `pid`: `(pid, name)` for `pid` and up to `depth` ancestors.
pub fn ancestry(pid: u32, depth: usize) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut cur = proc_info(pid);
    while let Some(p) = cur {
        out.push((p.pid, p.name.clone()));
        if out.len() > depth || p.ppid == 0 || p.ppid == p.pid {
            break;
        }
        cur = proc_info(p.ppid);
    }
    out
}

/// Roles FakeCli `spawn-tree` starts on this OS (§4.5).
pub fn spawn_tree_roles() -> &'static [&'static str] {
    SPAWN_TREE_ROLES
}

pub fn os_name() -> &'static str {
    std::env::consts::OS
}

/// Positive liveness for recovery decisions: `Some(false)` only when a full
/// process-table read shows `pid` absent, reused, or a zombie; `None` when
/// the table or the start time cannot be read (callers must refuse).
pub fn liveness(pid: u32, start_time: u64) -> Option<bool> {
    let procs = list_processes().ok()?;
    match procs.iter().find(|p| p.pid == pid) {
        None => Some(false),
        Some(p) if p.start_time == 0 => None,
        Some(p) => Some(p.start_time == start_time && !p.zombie),
    }
}

/// An `OsString` that is not valid UTF-8 on this platform (test fixture).
pub fn non_utf8_os_string() -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![b'a', 0xff, b'b'])
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0x61, 0xD800, 0x62])
    }
}

#[cfg(test)]
mod tests {
    /// A process is named after its executable image: the test binary's
    /// full name (longer than the 16-byte `pbi_name`) comes back intact.
    #[test]
    fn process_name_is_the_executable() {
        let exe = std::env::current_exe().unwrap();
        let want = exe.file_name().unwrap().to_string_lossy().into_owned();
        let got = super::proc_info(std::process::id()).unwrap().name;
        assert_eq!(got, want);
    }
}
