//! The leader process (`docs/design/pty-windows.md` §2.4): its held handle and job, exit
//! observation through the handle, and the tree kill over the real OS.

use std::io;
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::kill::{self, Access, JobOs, KillState, Leader, PidList};
use super::sys;
use crate::backend::{ChildExit, KillReport, ProcessIdentity, PtyChild};

/// The longest single wait of `wait_timeout` (below `INFINITE`).
const MAX_CHUNK_MS: u32 = u32::MAX - 1;
/// The exit code of a killed process.
pub(crate) const KILL_CODE: u32 = 1;

#[cfg(test)]
thread_local! {
    /// Caps each `wait_timeout` chunk (tests: CL-15 `chunked_wait_continues`).
    pub(crate) static CHUNK_CAP_MS: std::cell::Cell<u32> = const { std::cell::Cell::new(MAX_CHUNK_MS) };
}

fn chunk_cap() -> u32 {
    #[cfg(test)]
    return CHUNK_CAP_MS.with(std::cell::Cell::get);
    #[cfg(not(test))]
    MAX_CHUNK_MS
}

pub(crate) struct WindowsChild {
    process: OwnedHandle,
    /// `KILL_ON_JOB_CLOSE`: closing the last handle terminates what is left.
    job: Arc<OwnedHandle>,
    identity: ProcessIdentity,
    exit: OnceLock<ChildExit>,
    kill: Mutex<KillState<OwnedHandle>>,
    me: u32,
    /// Every kill sees an expired deadline (`WindowsFault::KillExpired`).
    #[cfg(any(test, feature = "fake"))]
    pub(crate) kill_expired: std::sync::atomic::AtomicBool,
    /// Runs inside `kill_tree` while it holds its lock (tests: CL-15).
    #[cfg(test)]
    pub(crate) in_kill: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl WindowsChild {
    /// `start`: the creation time read right after `CreateProcessW`, `None` when that read
    /// failed. `in_job`: the group `job:<pid>` (always, from stage 6 on).
    pub(crate) fn new(
        process: OwnedHandle,
        job: OwnedHandle,
        pid: u32,
        start: Option<u64>,
    ) -> WindowsChild {
        let job = Arc::new(job);
        #[cfg(any(test, feature = "fake"))]
        super::hooks::register_job(pid, &job);
        WindowsChild {
            process,
            job,
            identity: ProcessIdentity {
                pid,
                start_time: start.map(|s| s.to_string()).unwrap_or_default(),
                group: format!("job:{pid}"),
            },
            exit: OnceLock::new(),
            kill: Mutex::new(KillState::default()),
            me: std::process::id(),
            #[cfg(any(test, feature = "fake"))]
            kill_expired: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            in_kill: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn job(&self) -> &OwnedHandle {
        &self.job
    }
}

impl PtyChild for WindowsChild {
    fn identity(&self) -> &ProcessIdentity {
        &self.identity
    }

    fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<ChildExit>> {
        if let Some(e) = self.exit.get() {
            return Ok(Some(*e));
        }
        let end = Instant::now() + timeout;
        let mut first = true;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() && !first {
                return Ok(None);
            }
            // A zero timeout is one poll; otherwise a sub-millisecond remainder rounds up to
            // 1 ms, never to `INFINITE`.
            let ms = if timeout.is_zero() {
                0
            } else {
                let ceil = left.as_nanos().div_ceil(1_000_000);
                u32::try_from(ceil)
                    .unwrap_or(u32::MAX)
                    .clamp(1, chunk_cap())
            };
            first = false;
            if sys::wait(self.process.as_handle(), ms)? {
                let code = sys::exit_code(&self.process)?;
                let e = ChildExit {
                    code: Some(code.cast_signed()),
                    signal: None,
                };
                return Ok(Some(*self.exit.get_or_init(|| e)));
            }
            if timeout.is_zero() {
                return Ok(None);
            }
        }
    }

    fn kill_tree(&self, extra: &[ProcessIdentity], deadline: Instant) -> KillReport {
        #[cfg(any(test, feature = "fake"))]
        let deadline = if self.kill_expired.load(std::sync::atomic::Ordering::Acquire) {
            Instant::now()
        } else {
            deadline
        };
        let leader = Leader {
            identity: &self.identity,
            handle: &self.process,
            me: self.me,
        };
        let os = RealJobOs {
            job: &self.job,
            #[cfg(test)]
            in_kill: &self.in_kill,
        };
        kill::kill_tree(&os, &leader, extra, deadline, &self.kill)
    }
}

impl Drop for WindowsChild {
    fn drop(&mut self) {
        #[cfg(any(test, feature = "fake"))]
        super::hooks::unregister_job(self.identity.pid);
        // The process and job handles close as the fields drop; the job's last handle
        // closing terminates any member still running (kill-on-close).
    }
}

/// The kill engine over the real OS.
struct RealJobOs<'a> {
    job: &'a OwnedHandle,
    #[cfg(test)]
    in_kill: &'a Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl JobOs for RealJobOs<'_> {
    type Held = OwnedHandle;

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }

    fn terminate_job(&self) -> io::Result<()> {
        #[cfg(test)]
        if let Some(f) = self
            .in_kill
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            f();
        }
        let r = sys::terminate_job(self.job, KILL_CODE);
        super::tr("terminate_job", true, 0, super::res(&r));
        r
    }

    fn active_processes(&self) -> io::Result<u32> {
        sys::active_processes(self.job)
    }

    fn job_pids(&self, capacity: usize) -> io::Result<PidList> {
        sys::job_pids(self.job, capacity)
    }

    fn open_process(&self, pid: u32, access: Access) -> io::Result<OwnedHandle> {
        let mask = match access {
            Access::Kill => super::KILL_SYNC,
            Access::Wait => super::QUERY_SYNC,
        };
        sys::open_process(pid, mask)
    }

    fn creation_time(&self, h: &OwnedHandle) -> io::Result<u64> {
        sys::creation_time(h.as_handle())
    }

    fn in_job(&self, h: &OwnedHandle) -> io::Result<bool> {
        sys::in_job(h, self.job)
    }

    fn signalled(&self, h: &OwnedHandle, wait: Duration) -> io::Result<bool> {
        sys::wait_floor(h.as_handle(), wait)
    }

    fn terminate(&self, h: &OwnedHandle) -> io::Result<()> {
        match sys::terminate_process(h, KILL_CODE) {
            Err(e)
                if e.raw_os_error() == Some(super::ERROR_ACCESS_DENIED)
                    && sys::wait(h.as_handle(), 0).unwrap_or(false) =>
            {
                Ok(())
            }
            r => r,
        }
    }
}
