//! Test routes of the Windows backend (`docs/design/pty-windows.md` §2.6), compiled only
//! under `cfg(any(test, feature = "fake"))`: the next-spawn fault, gates, the per-thread call
//! trace, the output-marker reader, the teardown hook, and the registries of close threads
//! and live jobs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Read};
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::sys;
use crate::backend::ProcessIdentity;

/// How long a gate's `open`, the after-spawn milestone, and the drain hold wait at most.
pub(crate) const HOOK_WAIT: Duration = Duration::from_secs(10);

/// A fault injected into the next native spawn on the calling thread.
#[derive(Debug)]
pub enum WindowsFault {
    /// Fail the first pipe.
    FirstPipe,
    /// Fail the second pipe; the first exists.
    SecondPipe,
    /// Fail `CloseWorker::start`: no pseudoconsole is created.
    CloseWorker,
    PseudoConsole,
    DrainStart,
    JobCreate,
    /// The job handle exists; setting its limit fails.
    JobLimits,
    /// Shared by both attempts: the spawn fails after one attempt, with no fallback.
    AttrInit,
    /// Shared by both attempts: the spawn fails after one attempt, with no fallback.
    PseudoConsoleAttr,
    /// `set_job_list` fails (the list is initialised): the fallback runs for real.
    JobListAttr,
    /// Attempt 5.1 fails with `ERROR_NOT_SUPPORTED` without calling `CreateProcessW`; with a
    /// gate, the suspended create then stops at it as `AfterCreate` does.
    RejectJobList(Option<Gate>),
    /// Both attempts fail without calling `CreateProcessW`: 5.1 `ERROR_NOT_SUPPORTED`, 5.2
    /// `ERROR_NOT_ENOUGH_MEMORY`.
    CreateProcess,
    /// Implies `RejectJobList(None)`; the assignment fails (not called); termination is real.
    Assign,
    /// As `Assign`, and `TerminateProcess` goes through a handle without `PROCESS_TERMINATE`.
    AssignTerminateDenied,
    /// Implies `RejectJobList(None)`; `ResumeThread` is not called and fails; termination is
    /// real.
    ResumeFails,
    /// As `ResumeFails`, and `TerminateJobObject` goes through a handle without
    /// `JOB_OBJECT_TERMINATE`.
    ResumeTerminateDenied,
    /// Fail the creation-time read right after `CreateProcessW`.
    Identity,
    /// After a successful `CreateProcessW` (lock released): signal `reached`, wait `open`.
    AfterCreate(Gate),
    /// After stage 6: wait for the probe's marker, hold the drain, signal `reached`, wait
    /// for `open`, then fail.
    AfterSpawn {
        gate: Gate,
        probe: OutputProbe,
    },
    /// The teardown's kill runs with an expired deadline.
    KillExpired,
    /// Implies `RejectJobList(None)`; the real `ResumeThread` runs, and its previous suspend
    /// count is classified as this value instead.
    ResumeCount(u32),
    /// No failure: once the probe's marker is seen, the drain's next read waits until the
    /// control's close begins (or `HOOK_WAIT`).
    HoldOutput(OutputProbe),
}

impl WindowsFault {
    /// Whether this fault forces the job-list attempt to fail.
    pub(crate) fn rejects_job_list(&self) -> bool {
        matches!(
            self,
            WindowsFault::RejectJobList(_)
                | WindowsFault::Assign
                | WindowsFault::AssignTerminateDenied
                | WindowsFault::ResumeFails
                | WindowsFault::ResumeTerminateDenied
                | WindowsFault::ResumeCount(_)
        )
    }
}

/// The backend's end of a gate.
#[derive(Debug)]
pub struct Gate {
    reached: Sender<()>,
    open: Receiver<()>,
}

/// The test's end of a gate.
#[derive(Debug)]
pub struct GateControl {
    reached: Receiver<()>,
    open: Sender<()>,
}

/// A new gate: the backend gets [`Gate`], the test keeps [`GateControl`].
#[must_use]
pub fn gate() -> (Gate, GateControl) {
    let (rt, rr) = mpsc::channel();
    let (ot, or) = mpsc::channel();
    (
        Gate {
            reached: rt,
            open: or,
        },
        GateControl {
            reached: rr,
            open: ot,
        },
    )
}

impl GateControl {
    /// Waits until the backend reached the gate; false at `timeout`.
    #[must_use]
    pub fn wait_reached(&self, timeout: Duration) -> bool {
        self.reached.recv_timeout(timeout).is_ok()
    }

    /// Lets the backend continue.
    pub fn open(self) {
        let _ = self.open.send(());
    }
}

impl Gate {
    /// Signals `reached` and waits for `open` (at most `HOOK_WAIT`); a timeout or a dropped
    /// sender records `gate_abandoned` and continues.
    pub(crate) fn pass(&self) {
        let _ = self.reached.send(());
        if self.open.recv_timeout(HOOK_WAIT).is_err() {
            with_report(|r| r.gate_abandoned = true);
        }
    }
}

/// One call the backend made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceEntry {
    pub name: &'static str,
    /// False when the call was synthesized by a fault instead of made.
    pub native: bool,
    /// Creation flags of a `create_process` entry; 0 otherwise.
    pub flags: u32,
    /// A value (a pid, a suspend count) or a Win32 error code.
    pub result: Result<u64, i32>,
    /// Whether the tracing thread held the spawn lock.
    pub lock_held: bool,
}

/// What the last native spawn on this thread did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FaultReport {
    pub trace: Vec<TraceEntry>,
    /// A gate's `open` timed out or its sender was dropped.
    pub gate_abandoned: bool,
    /// Whether the suspended child's exit was confirmed (stage 5.4).
    pub child_exited: Option<bool>,
    /// Whether the marker was seen while the child was running (`AfterSpawn`).
    pub marker_seen_running: Option<bool>,
    /// Per created process, read right after its native create returned and before any
    /// assignment: whether it was already in the session's job (`None`: the read failed).
    pub member_at_create: Vec<Option<bool>>,
    /// Each native `CreatePseudoConsole` / `CreateProcessW` call, with whether the calling
    /// thread held the spawn lock as the call was made.
    pub native_lock: Vec<(&'static str, bool)>,
    /// Whether the drain's reader was parked by an `AfterSpawn` hold when the teardown's
    /// kill returned.
    pub held_at_teardown_kill: Option<bool>,
}

impl FaultReport {
    /// The names of the trace, in order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.trace.iter().map(|e| e.name).collect()
    }

    /// The entries named `name`.
    #[must_use]
    pub fn entries(&self, name: &str) -> Vec<&TraceEntry> {
        self.trace.iter().filter(|e| e.name == name).collect()
    }
}

type TeardownHook = Box<dyn FnOnce() + Send>;

thread_local! {
    static NEXT: RefCell<Option<WindowsFault>> = const { RefCell::new(None) };
    static REPORT: RefCell<Option<FaultReport>> = const { RefCell::new(None) };
    static TEARDOWN: RefCell<Option<TeardownHook>> = const { RefCell::new(None) };
    static HOLD: RefCell<Option<Arc<Marker>>> = const { RefCell::new(None) };
}

pub(crate) fn inject_next(f: WindowsFault) {
    NEXT.with(|n| *n.borrow_mut() = Some(f));
}

pub(crate) fn take_next() -> Option<WindowsFault> {
    NEXT.with(|n| n.borrow_mut().take())
}

/// Starts a fresh report for a spawn on this thread.
pub(crate) fn start_report() {
    REPORT.with(|r| *r.borrow_mut() = Some(FaultReport::default()));
    HOLD.with(|h| *h.borrow_mut() = None);
}

pub(crate) fn report() -> Option<FaultReport> {
    REPORT.with(|r| r.borrow().clone())
}

pub(crate) fn with_report(f: impl FnOnce(&mut FaultReport)) {
    REPORT.with(|r| {
        if let Some(rep) = r.borrow_mut().as_mut() {
            f(rep);
        }
    });
}

/// Records, at a native create's call boundary, whether this thread holds the spawn lock.
pub(crate) fn native_boundary(name: &'static str) {
    let held = crate::spawn::spawn_lock_held_here();
    with_report(|r| r.native_lock.push((name, held)));
}

/// Records one call on this thread's report, if a spawn started one.
pub(crate) fn trace(name: &'static str, native: bool, flags: u32, result: Result<u64, i32>) {
    let lock_held = crate::spawn::spawn_lock_held_here();
    with_report(|r| {
        r.trace.push(TraceEntry {
            name,
            native,
            flags,
            result,
            lock_held,
        });
    });
}

pub(crate) fn on_teardown_kill(f: TeardownHook) {
    TEARDOWN.with(|t| *t.borrow_mut() = Some(f));
}

/// The teardown point right after the kill.
pub(crate) fn after_teardown_kill() {
    trace("teardown_kill", true, 0, Ok(0));
    if let Some(parked) = HOLD.with(|h| h.borrow().as_ref().map(|m| m.parked())) {
        with_report(|r| r.held_at_teardown_kill = Some(parked));
    }
    if let Some(f) = TEARDOWN.with(|t| t.borrow_mut().take()) {
        f();
        trace("teardown_hook_ran", true, 0, Ok(0));
    }
}

/// The teardown point right before the close: releases a held drain.
pub(crate) fn before_teardown_close() {
    trace("teardown_close", true, 0, Ok(0));
    if let Some(m) = HOLD.with(|h| h.borrow_mut().take()) {
        m.release();
    }
}

/// Registers the marker whose hold the next teardown on this thread releases.
pub(crate) fn hold_until_teardown(m: Arc<Marker>) {
    HOLD.with(|h| *h.borrow_mut() = Some(m));
}

// ---- Output marker ----

#[derive(Default)]
struct MarkerState {
    carry: Vec<u8>,
    seen: bool,
    hold: bool,
    held_since: Option<Instant>,
    released: bool,
    /// The reader is blocked in the hold right now.
    parked: bool,
    /// Whether the reader was parked when the close began (`HoldOutput`).
    parked_at_close: Option<bool>,
    /// Every byte read, in order (tests: CL-13).
    #[cfg(test)]
    bytes: Vec<u8>,
    /// Set by the test right before the control's close (tests: CL-13).
    #[cfg(test)]
    closing: bool,
    /// A read returned 0 before `closing` was set (tests: CL-13).
    #[cfg(test)]
    eof_before_closing: bool,
}

/// What the output-marker reader has seen; shared with the spawn, the control, and the
/// teardown.
pub(crate) struct Marker {
    marker: &'static [u8],
    st: Mutex<MarkerState>,
    cv: Condvar,
}

impl Marker {
    /// `hold`: once the marker is seen, the next read blocks until released (or
    /// `HOOK_WAIT`).
    pub(crate) fn new(marker: &'static [u8], hold: bool) -> Arc<Marker> {
        Arc::new(Marker {
            marker,
            st: Mutex::new(MarkerState {
                hold,
                ..MarkerState::default()
            }),
            cv: Condvar::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MarkerState> {
        self.st.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits until `done` holds for the state; false at `deadline`.
    fn wait_for(&self, deadline: Instant, done: impl Fn(&MarkerState) -> bool) -> bool {
        let mut st = self.lock();
        while !done(&st) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self
                .cv
                .wait_timeout(st, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Waits until the marker was seen; false at `deadline`.
    pub(crate) fn wait_seen(&self, deadline: Instant) -> bool {
        self.wait_for(deadline, |st| st.seen)
    }

    /// Waits until the reader is parked in the hold; false at `deadline`.
    pub(crate) fn wait_parked(&self, deadline: Instant) -> bool {
        self.wait_for(deadline, |st| st.parked)
    }

    pub(crate) fn parked(&self) -> bool {
        self.lock().parked
    }

    /// Every byte read so far.
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> Vec<u8> {
        self.lock().bytes.clone()
    }

    /// Marks the start of the control's close: a later EOF is expected.
    #[cfg(test)]
    pub(crate) fn closing(&self) {
        self.lock().closing = true;
    }

    /// Whether a read returned 0 before `closing`.
    #[cfg(test)]
    pub(crate) fn eof_before_closing(&self) -> bool {
        self.lock().eof_before_closing
    }

    /// Releases the hold (one-shot).
    pub(crate) fn release(&self) {
        self.lock().released = true;
        self.cv.notify_all();
    }

    /// Records whether the reader is parked, then releases it (the control's close).
    pub(crate) fn release_at_close(&self) {
        let mut st = self.lock();
        st.parked_at_close = Some(st.parked);
        st.released = true;
        drop(st);
        self.cv.notify_all();
    }

    pub(crate) fn parked_at_close(&self) -> Option<bool> {
        self.lock().parked_at_close
    }

    /// Blocks while held: from the read after the marker until released or `HOOK_WAIT`.
    fn wait_hold(&self) {
        let mut st = self.lock();
        loop {
            if !(st.hold && st.seen) || st.released {
                break;
            }
            let since = *st.held_since.get_or_insert_with(Instant::now);
            let left = (since + HOOK_WAIT).saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            if !st.parked {
                st.parked = true;
                self.cv.notify_all();
            }
            st = self
                .cv
                .wait_timeout(st, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        st.parked = false;
    }

    fn scan(&self, bytes: &[u8]) {
        let mut st = self.lock();
        #[cfg(test)]
        {
            if bytes.is_empty() && !st.closing {
                st.eof_before_closing = true;
            }
            st.bytes.extend_from_slice(bytes);
        }
        if st.seen || self.marker.is_empty() {
            return;
        }
        let mut window = std::mem::take(&mut st.carry);
        window.extend_from_slice(bytes);
        if window.windows(self.marker.len()).any(|w| w == self.marker) {
            st.seen = true;
            drop(st);
            self.cv.notify_all();
            return;
        }
        let keep = self.marker.len() - 1;
        st.carry = window[window.len().saturating_sub(keep)..].to_vec();
    }
}

/// The test's end of one spawn's holding output-marker reader (`AfterSpawn`,
/// `HoldOutput`). The test keeps it through its assertions, so what the reader recorded
/// outlives the spawn's own references, and no other spawn can replace it. One probe serves
/// one spawn.
#[derive(Clone)]
pub struct OutputProbe(Arc<Marker>);

impl OutputProbe {
    /// A probe whose reader holds once it has seen `marker`.
    #[must_use]
    pub fn new(marker: &'static [u8]) -> OutputProbe {
        OutputProbe(Marker::new(marker, true))
    }

    /// Whether the reader is parked in its hold by `deadline` (no read outstanding).
    #[must_use]
    pub fn parked(&self, deadline: Instant) -> bool {
        self.0.wait_parked(deadline)
    }

    /// Whether the reader was parked when the control's close began; `None` before that
    /// close (`HoldOutput`).
    #[must_use]
    pub fn parked_at_close(&self) -> Option<bool> {
        self.0.parked_at_close()
    }

    pub(crate) fn marker(&self) -> Arc<Marker> {
        self.0.clone()
    }
}

impl std::fmt::Debug for OutputProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OutputProbe")
            .field(&String::from_utf8_lossy(self.0.marker))
            .finish()
    }
}

/// The drain's reader in test and `fake` builds: the output read end, watched for a marker.
pub(crate) struct MarkerReader<R> {
    inner: R,
    marker: Option<Arc<Marker>>,
}

impl<R: Read> MarkerReader<R> {
    pub(crate) fn new(inner: R, marker: Option<Arc<Marker>>) -> MarkerReader<R> {
        MarkerReader { inner, marker }
    }
}

impl<R: Read> Read for MarkerReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(m) = &self.marker {
            m.wait_hold();
        }
        let n = self.inner.read(buf)?;
        if let Some(m) = &self.marker {
            m.scan(&buf[..n]);
        }
        Ok(n)
    }
}

// ---- Registries ----

/// Close workers' handles kept for `close_threads_exited` (a handle-count probe), and
/// whether they are kept: one lock, so a registration and a disarm are ordered.
struct CloseThreads {
    track: bool,
    kept: Vec<JoinHandle<()>>,
}

static CLOSE_THREADS: Mutex<CloseThreads> = Mutex::new(CloseThreads {
    track: false,
    kept: Vec::new(),
});

fn close_threads() -> std::sync::MutexGuard<'static, CloseThreads> {
    CLOSE_THREADS.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
type AfterTake = Box<dyn FnOnce()>;

#[cfg(test)]
thread_local! {
    /// Runs in `close_threads_exited` after it took the batch, before its waits (tests).
    static AFTER_TAKE: RefCell<Option<AfterTake>> = const { RefCell::new(None) };
}

/// Runs `f` in the next `close_threads_exited` on this thread, after it took the batch.
#[cfg(test)]
pub(crate) fn after_take(f: AfterTake) {
    AFTER_TAKE.with(|h| *h.borrow_mut() = Some(f));
}

/// Whether the registry's lock is free right now (tests).
#[cfg(test)]
pub(crate) fn close_threads_unlocked() -> bool {
    CLOSE_THREADS.try_lock().is_ok()
}

/// Starts or stops keeping close workers' handles; stopping drops those kept.
pub(crate) fn track_close_threads(on: bool) {
    let mut c = close_threads();
    c.track = on;
    let dropped = if on {
        Vec::new()
    } else {
        std::mem::take(&mut c.kept)
    };
    drop(c);
    drop(dropped);
}

/// A close worker whose close returned: kept only while a probe tracks them, else dropped.
pub(crate) fn register_close_thread(j: JoinHandle<()>) {
    let mut c = close_threads();
    if c.track {
        c.kept.push(j);
    }
}

/// How many close workers' handles are kept.
#[cfg(test)]
pub(crate) fn kept_close_threads() -> usize {
    close_threads().kept.len()
}

/// Takes the kept close threads out of the registry, waits (bounded, without the
/// registry's lock) on their handles, joins those signalled, and returns whether all of
/// this batch were; the others go back while tracking is on. The result covers only the
/// batch it took: the probe is meant for one arm at a time, used serially (one sweep at a
/// time, no disarm and rearm during a sweep).
pub(crate) fn close_threads_exited(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let batch = std::mem::take(&mut close_threads().kept);
    #[cfg(test)]
    if let Some(f) = AFTER_TAKE.with(|h| h.borrow_mut().take()) {
        f();
    }
    let mut left = Vec::new();
    for j in batch {
        let budget = deadline.saturating_duration_since(Instant::now());
        if sys::wait_floor(j.as_handle(), budget).unwrap_or(false) {
            let _ = j.join();
        } else {
            left.push(j);
        }
    }
    let all = left.is_empty();
    let mut c = close_threads();
    if c.track {
        c.kept.extend(left);
    }
    all
}

static JOBS: Mutex<Option<HashMap<u32, Weak<OwnedHandle>>>> = Mutex::new(None);

pub(crate) fn register_job(leader: u32, job: &Arc<OwnedHandle>) {
    JOBS.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(leader, Arc::downgrade(job));
}

pub(crate) fn unregister_job(leader: u32) {
    if let Some(m) = JOBS.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
        m.remove(&leader);
    }
}

fn job_of(leader: u32) -> io::Result<Arc<OwnedHandle>> {
    JOBS.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|m| m.get(&leader))
        .and_then(Weak::upgrade)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no live session job"))
}

/// `IsProcessInJob(pid, the job of the session led by `leader`)`.
pub(crate) fn in_job(leader: u32, pid: u32) -> io::Result<bool> {
    let job = job_of(leader)?;
    let p = sys::open_process(pid, super::QUERY_SYNC)?;
    sys::in_job(&p, &job)
}

/// `ActiveProcesses` of the job of the session led by `leader`.
pub(crate) fn job_active(leader: u32) -> io::Result<u32> {
    sys::active_processes(&*job_of(leader)?)
}

// ---- Process helpers ----

/// A live process's identity (an exited one is `None`); group empty.
pub(crate) fn identify(pid: u32) -> io::Result<Option<ProcessIdentity>> {
    let h = match sys::open_process(pid, super::QUERY_SYNC) {
        Ok(h) => h,
        Err(e) if e.raw_os_error() == Some(super::kill::ERROR_INVALID_PARAMETER) => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    if sys::wait(h.as_handle(), 0)? {
        return Ok(None);
    }
    let start = sys::creation_time(h.as_handle())?;
    Ok(Some(ProcessIdentity {
        pid,
        start_time: start.to_string(),
        group: String::new(),
    }))
}

/// Terminates `pid` through one handle after checking its creation time on that handle.
pub(crate) fn kill_verified(pid: u32, start_time: &str) -> bool {
    let Ok(start) = start_time.parse::<u64>() else {
        return false;
    };
    let Ok(h) = sys::open_process(pid, super::KILL_SYNC) else {
        return false;
    };
    sys::creation_time(h.as_handle()).is_ok_and(|t| t == start)
        && sys::terminate_process(&h, super::child::KILL_CODE).is_ok()
}

/// One handle on a verified process, kept for every later check.
#[derive(Debug)]
pub struct Watch {
    h: OwnedHandle,
}

impl Watch {
    /// Whether the process has exited by `timeout`.
    ///
    /// # Errors
    ///
    /// The wait failed.
    pub fn signalled(&self, timeout: Duration) -> io::Result<bool> {
        sys::wait_floor(self.h.as_handle(), timeout)
    }
}

pub(crate) fn watch(id: &ProcessIdentity) -> io::Result<Watch> {
    let h = sys::open_process(id.pid, super::QUERY_SYNC)?;
    let start = sys::creation_time(h.as_handle())?;
    if start.to_string() != id.start_time {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("pid {} has start {start}, not {}", id.pid, id.start_time),
        ));
    }
    Ok(Watch { h })
}
