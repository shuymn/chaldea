//! The leader process (`docs/design/pty-windows.md` §2.4): its held handle and job, exit
//! observation through the handle, and the tree kill over the real OS.

use std::io;
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::kill::{self, Access, JobOs, KillState, Leader, PidList, ProcessOs};
use super::sys;
use super::wait::{self, MAX_CHUNK_MS};
use crate::backend::{ChildExit, KillReport, ProcessIdentity, PtyChild};

/// The exit code of a killed process.
pub(crate) const KILL_CODE: u32 = 1;

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
}

impl PtyChild for WindowsChild {
    fn identity(&self) -> &ProcessIdentity {
        &self.identity
    }

    fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<ChildExit>> {
        if let Some(e) = self.exit.get() {
            return Ok(Some(*e));
        }
        let signalled = wait::chunked(timeout, Instant::now(), Instant::now, MAX_CHUNK_MS, |ms| {
            sys::wait(self.process.as_handle(), ms)
        })?;
        if !signalled {
            return Ok(None);
        }
        let code = sys::exit_code(&self.process)?;
        let e = ChildExit {
            code: Some(code.cast_signed()),
            signal: None,
        };
        Ok(Some(*self.exit.get_or_init(|| e)))
    }

    fn kill_tree(
        &self,
        extra: &[ProcessIdentity],
        lock_by: Instant,
        deadline: Instant,
    ) -> KillReport {
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
        kill::kill_tree(&os, &leader, extra, lock_by, deadline, &self.kill)
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

/// The process calls over the real OS, with no job handle: the owned engine's through
/// [`RealJobOs`], and the orphan loop's (`orphan::kill_recorded`).
pub(crate) struct RealProcessOs;

impl ProcessOs for RealProcessOs {
    type Held = OwnedHandle;

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
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

    fn signalled(&self, h: &OwnedHandle, wait: Duration) -> io::Result<bool> {
        sys::wait_floor(h.as_handle(), wait)
    }

    fn terminate(&self, h: &OwnedHandle) -> io::Result<()> {
        sys::terminate_process(h, KILL_CODE)
    }
}

impl ProcessOs for RealJobOs<'_> {
    type Held = OwnedHandle;

    fn now(&self) -> Instant {
        RealProcessOs.now()
    }

    fn sleep(&self, d: Duration) {
        RealProcessOs.sleep(d);
    }

    fn open_process(&self, pid: u32, access: Access) -> io::Result<OwnedHandle> {
        RealProcessOs.open_process(pid, access)
    }

    fn creation_time(&self, h: &OwnedHandle) -> io::Result<u64> {
        RealProcessOs.creation_time(h)
    }

    fn signalled(&self, h: &OwnedHandle, wait: Duration) -> io::Result<bool> {
        RealProcessOs.signalled(h, wait)
    }

    fn terminate(&self, h: &OwnedHandle) -> io::Result<()> {
        RealProcessOs.terminate(h)
    }
}

impl JobOs for RealJobOs<'_> {
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

    fn in_job(&self, h: &OwnedHandle) -> io::Result<bool> {
        sys::in_job(h, self.job)
    }
}
