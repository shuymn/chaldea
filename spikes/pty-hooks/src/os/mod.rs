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
