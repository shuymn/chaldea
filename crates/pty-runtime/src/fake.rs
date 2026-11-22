//! `FakePty` (`docs/design/pty-runtime.md` §2.7): a `PtyRuntime` whose child is a script,
//! so the shared drain, writer, VT, and close order run unchanged in tests on every OS.
//! Output travels through `std::io::pipe()`, so the drain reads a real pipe and EOF is
//! real. `WriteHold::wait_entered` and `Probe` are the deterministic signals that a thread
//! is at a given point.

use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::backend::{
    ChildExit, Cleanup, CloseReport, DrainSink, KillReport, NeverStartedKind, ProcessIdentity,
    PtyChild, PtyControl, PtyRuntime, PtySize, SpawnError, SpawnSpec, SpawnedParts, SpawnedPty,
    sealed,
};
use crate::drain::Drain;
use crate::session::lock;

/// How long `spawn` waits for its startup output to be parsed.
const STARTUP_PARSE: Duration = Duration::from_secs(5);
/// The byte `fail_output` writes to wake a blocked read; the reader swallows it.
const WAKE: u8 = 0;

/// What `kill_tree` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KillBehavior {
    /// The child exits at once and the tree is reported dead.
    #[default]
    ExitOnKill,
    /// Nothing dies; `kill_tree` uses its whole deadline and reports survivors.
    Ignore,
    /// The child exits after this long; dead if that is within the deadline.
    ExitAfter(Duration),
}

/// The script of the next spawn.
#[derive(Debug, Default)]
pub struct FakeScript {
    /// `spawn` returns this.
    pub fail: Option<SpawnError>,
    /// Written before `spawn` returns; `spawn` returns after the sink parsed it.
    pub startup_output: Vec<u8>,
    pub kill: KillBehavior,
    /// `control.close` blocks until its deadline and reports `elapsed: None`.
    pub hang_control_close: bool,
}

/// What the fake child observed, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeEvent {
    Spawned,
    WriteEntered,
    Write(usize),
    Resize(PtySize),
    ResizeFailed,
    Kill { extra: Vec<ProcessIdentity> },
    InputDropped,
    ControlClosed,
    OutputClosed,
    Exited,
}

type Reaction = Box<dyn FnMut(&[u8]) -> Vec<u8> + Send>;

struct Hold {
    id: u64,
    entered: bool,
    released: bool,
}

#[derive(Default)]
struct InputState {
    writes: Vec<Vec<u8>>,
    total: u64,
    hold: Option<Hold>,
    next_hold: u64,
    fail_after: Option<(u64, io::ErrorKind)>,
    short: Option<usize>,
    panic_next: bool,
    on_input: Option<Reaction>,
}

/// One fake process and its PTY.
struct Proc {
    identity: ProcessIdentity,
    kill: KillBehavior,
    hang_close: bool,
    log: Mutex<Vec<FakeEvent>>,
    exit: Mutex<Option<ChildExit>>,
    exit_cv: Condvar,
    out: Mutex<Option<PipeWriter>>,
    out_error: Mutex<Option<io::ErrorKind>>,
    input: Mutex<InputState>,
    input_cv: Condvar,
    fail_resize: AtomicBool,
}

impl Proc {
    fn record(&self, e: FakeEvent) {
        lock(&self.log).push(e);
    }

    fn emit(&self, bytes: &[u8]) {
        let mut out = lock(&self.out);
        if let Some(w) = out.as_mut() {
            let _ = w.write_all(bytes);
        }
    }

    fn close_output(&self) {
        if lock(&self.out).take().is_some() {
            self.record(FakeEvent::OutputClosed);
        }
    }

    fn exit(&self, e: ChildExit) {
        let mut x = lock(&self.exit);
        if x.is_none() {
            *x = Some(e);
            self.record(FakeEvent::Exited);
        }
        drop(x);
        self.exit_cv.notify_all();
    }

    fn exited(&self) -> bool {
        lock(&self.exit).is_some()
    }

    fn input_state(&self) -> MutexGuard<'_, InputState> {
        lock(&self.input)
    }
}

/// A `PtyRuntime` whose children are scripts.
#[derive(Clone, Default)]
pub struct FakePty {
    inner: Arc<Runtime>,
}

#[derive(Default)]
struct Runtime {
    next: Mutex<Option<FakeScript>>,
    specs: Mutex<Vec<SpawnSpec>>,
    latest: Mutex<Option<Arc<Proc>>>,
    count: AtomicU32,
}

impl FakePty {
    #[must_use]
    pub fn new() -> FakePty {
        FakePty::default()
    }

    /// Applies to the next spawn.
    pub fn script_next(&self, s: FakeScript) {
        *lock(&self.inner.next) = Some(s);
    }

    /// Every spec, in order.
    #[must_use]
    pub fn spawns(&self) -> Vec<SpawnSpec> {
        lock(&self.inner.specs).clone()
    }

    /// The controller of the latest spawn.
    #[must_use]
    pub fn take_child(&self) -> Option<FakeChild> {
        lock(&self.inner.latest)
            .take()
            .map(|proc| FakeChild { proc })
    }
}

impl sealed::Sealed for FakePty {}

impl PtyRuntime for FakePty {
    #[allow(
        clippy::result_large_err,
        reason = "spawn is rare; the error carries its cleanup evidence by value"
    )]
    fn spawn(&self, spec: &SpawnSpec, output: DrainSink) -> Result<SpawnedPty, SpawnError> {
        lock(&self.inner.specs).push(spec.clone());
        let script = lock(&self.inner.next).take().unwrap_or_default();
        if let Some(e) = script.fail {
            return Err(e);
        }
        let setup = |error: io::Error| SpawnError::NeverStarted {
            kind: NeverStartedKind::Setup,
            error,
            cleanup: Cleanup::Confirmed,
        };
        let (r, w) = io::pipe().map_err(setup)?;
        let n = self.inner.count.fetch_add(1, Ordering::Relaxed) + 1;
        let proc = Arc::new(Proc {
            identity: ProcessIdentity {
                pid: n,
                start_time: format!("fake-{n}"),
                group: format!("fake-{n}"),
            },
            kill: script.kill,
            hang_close: script.hang_control_close,
            log: Mutex::new(vec![FakeEvent::Spawned]),
            exit: Mutex::new(None),
            exit_cv: Condvar::new(),
            out: Mutex::new(Some(w)),
            out_error: Mutex::new(None),
            input: Mutex::new(InputState::default()),
            input_cv: Condvar::new(),
            fail_resize: AtomicBool::new(false),
        });
        let reader = FakeReader {
            pipe: r,
            proc: proc.clone(),
            pending: None,
        };
        let drain = Drain::start(reader, output).map_err(setup)?;
        if !script.startup_output.is_empty() {
            proc.emit(&script.startup_output);
            drain.wait_delivered(
                script.startup_output.len() as u64,
                Instant::now() + STARTUP_PARSE,
            );
        }
        *lock(&self.inner.latest) = Some(proc.clone());
        Ok(SpawnedPty::new(SpawnedParts {
            child: Arc::new(FakeProcess(proc.clone())),
            output: drain,
            input: Box::new(FakeInput(proc.clone())),
            control: Box::new(FakeControl(proc)),
            size: spec.size,
            evidence: vec![("backend", "fake".to_owned())],
        }))
    }
}

/// The output read end: a real pipe, plus an injectable read error.
struct FakeReader {
    pipe: PipeReader,
    proc: Arc<Proc>,
    pending: Option<io::ErrorKind>,
}

impl Read for FakeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(kind) = self.pending.take() {
            return Err(kind.into());
        }
        let n = self.pipe.read(buf)?;
        if n > 0
            && buf[n - 1] == WAKE
            && let Some(kind) = lock(&self.proc.out_error).take()
        {
            if n == 1 {
                return Err(kind.into());
            }
            self.pending = Some(kind);
            return Ok(n - 1);
        }
        Ok(n)
    }
}

/// The input write end.
struct FakeInput(Arc<Proc>);

impl Write for FakeInput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let p = &self.0;
        let mut st = p.input_state();
        p.record(FakeEvent::WriteEntered);
        if st.panic_next {
            st.panic_next = false;
            drop(st);
            panic!("injected write panic");
        }
        if let Some(h) = st.hold.as_mut() {
            h.entered = true;
            p.input_cv.notify_all();
            while st.hold.as_ref().is_some_and(|h| !h.released) {
                st = p
                    .input_cv
                    .wait(st)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            st.hold = None;
        }
        let mut n = buf.len();
        if let Some(m) = st.short {
            n = n.min(m);
        }
        if let Some((limit, kind)) = st.fail_after {
            if st.total >= limit {
                return Err(kind.into());
            }
            n = n.min(usize::try_from(limit - st.total).unwrap_or(usize::MAX));
        }
        st.writes.push(buf[..n].to_vec());
        st.total += n as u64;
        p.record(FakeEvent::Write(n));
        let reaction = st.on_input.as_mut().map(|f| f(&buf[..n]));
        drop(st);
        if let Some(out) = reaction.filter(|o| !o.is_empty()) {
            p.emit(&out);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for FakeInput {
    fn drop(&mut self) {
        self.0.record(FakeEvent::InputDropped);
    }
}

struct FakeControl(Arc<Proc>);

impl PtyControl for FakeControl {
    fn resize(&self, size: PtySize) -> io::Result<()> {
        if self.0.fail_resize.swap(false, Ordering::AcqRel) {
            self.0.record(FakeEvent::ResizeFailed);
            return Err(io::Error::other("injected resize failure"));
        }
        self.0.record(FakeEvent::Resize(size));
        Ok(())
    }

    fn close(self: Box<Self>, deadline: Instant) -> CloseReport {
        let t = Instant::now();
        if self.0.hang_close {
            sleep_until(deadline);
            return CloseReport { elapsed: None };
        }
        self.0.record(FakeEvent::ControlClosed);
        // Releasing the PTY ends its output, as closing a pseudoconsole does.
        self.0.close_output();
        CloseReport {
            elapsed: Some(t.elapsed()),
        }
    }
}

fn sleep_until(deadline: Instant) {
    let left = deadline.saturating_duration_since(Instant::now());
    if !left.is_zero() {
        std::thread::sleep(left);
    }
}

struct FakeProcess(Arc<Proc>);

const KILLED: ChildExit = ChildExit {
    code: None,
    signal: Some(9),
};

impl PtyChild for FakeProcess {
    fn identity(&self) -> &ProcessIdentity {
        &self.0.identity
    }

    fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<ChildExit>> {
        let x = lock(&self.0.exit);
        let (x, _) = self
            .0
            .exit_cv
            .wait_timeout_while(x, timeout, |x| x.is_none())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(*x)
    }

    fn kill_tree(&self, extra: &[ProcessIdentity], deadline: Instant) -> KillReport {
        let p = &self.0;
        p.record(FakeEvent::Kill {
            extra: extra.to_vec(),
        });
        let dead = match p.kill {
            KillBehavior::ExitOnKill => {
                p.exit(KILLED);
                true
            }
            KillBehavior::Ignore => {
                sleep_until(deadline);
                false
            }
            KillBehavior::ExitAfter(d) => {
                let at = Instant::now() + d;
                if at <= deadline {
                    sleep_until(at);
                    p.exit(KILLED);
                    true
                } else {
                    let later = p.clone();
                    std::thread::spawn(move || {
                        sleep_until(at);
                        later.exit(KILLED);
                    });
                    sleep_until(deadline);
                    p.exited()
                }
            }
        };
        KillReport {
            method: "fake".to_owned(),
            tree_dead: dead,
            survivors: if dead {
                Vec::new()
            } else {
                vec![p.identity.clone()]
            },
        }
    }
}

/// The controller of one fake child.
#[derive(Clone)]
pub struct FakeChild {
    proc: Arc<Proc>,
}

impl FakeChild {
    #[must_use]
    pub fn identity(&self) -> ProcessIdentity {
        self.proc.identity.clone()
    }

    /// Child output; blocks only on a full pipe.
    pub fn emit(&self, bytes: &[u8]) {
        self.proc.emit(bytes);
    }

    /// A scripted reaction to every accepted write, e.g. echo or queries; its output is
    /// emitted from the writer thread.
    pub fn on_input(&self, f: impl FnMut(&[u8]) -> Vec<u8> + Send + 'static) {
        self.proc.input_state().on_input = Some(Box::new(f));
    }

    /// EOF.
    pub fn close_output(&self) {
        self.proc.close_output();
    }

    /// Sets the read error, then writes a wake byte the reader swallows.
    pub fn fail_output(&self, kind: io::ErrorKind) {
        *lock(&self.proc.out_error) = Some(kind);
        self.proc.emit(&[WAKE]);
    }

    pub fn exit(&self, e: ChildExit) {
        self.proc.exit(e);
    }

    /// The next `write()` blocks until the hold is released or dropped.
    #[must_use]
    pub fn hold_writes(&self) -> WriteHold {
        let mut st = self.proc.input_state();
        st.next_hold += 1;
        let id = st.next_hold;
        st.hold = Some(Hold {
            id,
            entered: false,
            released: false,
        });
        WriteHold {
            proc: self.proc.clone(),
            id,
        }
    }

    /// Once `bytes` input bytes were accepted, every write fails with `kind`; a write
    /// crossing the limit is accepted up to it.
    pub fn fail_writes_after(&self, bytes: u64, kind: io::ErrorKind) {
        self.proc.input_state().fail_after = Some((bytes, kind));
    }

    /// `write()` accepts at most `max` bytes per call; 0 makes it return `Ok(0)`.
    pub fn short_writes(&self, max: usize) {
        self.proc.input_state().short = Some(max);
    }

    pub fn panic_on_next_write(&self) {
        self.proc.input_state().panic_next = true;
    }

    pub fn fail_next_resize(&self) {
        self.proc.fail_resize.store(true, Ordering::Release);
    }

    /// Every accepted `write()` call's bytes, in order.
    #[must_use]
    pub fn writes(&self) -> Vec<Vec<u8>> {
        self.proc.input_state().writes.clone()
    }

    /// The input stream.
    #[must_use]
    pub fn input(&self) -> Vec<u8> {
        self.proc.input_state().writes.concat()
    }

    #[must_use]
    pub fn log(&self) -> Vec<FakeEvent> {
        lock(&self.proc.log).clone()
    }
}

/// A blocked `write()`, released by `release` or drop.
pub struct WriteHold {
    proc: Arc<Proc>,
    id: u64,
}

impl WriteHold {
    /// Whether the held write has been entered (and is blocked) by `deadline`.
    #[must_use]
    pub fn wait_entered(&self, deadline: Instant) -> bool {
        let mut st = self.proc.input_state();
        loop {
            match st.hold.as_ref() {
                Some(h) if h.id == self.id && h.entered => return true,
                Some(h) if h.id == self.id => {}
                // Released or replaced: it was entered only if it already passed.
                _ => return false,
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self
                .proc
                .input_cv
                .wait_timeout(st, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    pub fn release(self) {}
}

impl Drop for WriteHold {
    fn drop(&mut self) {
        let mut st = self.proc.input_state();
        if let Some(h) = st.hold.as_mut().filter(|h| h.id == self.id) {
            h.released = true;
            if !h.entered {
                st.hold = None;
            }
        }
        drop(st);
        self.proc.input_cv.notify_all();
    }
}

/// A point in the runtime's threads that tests wait for or park at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProbePoint {
    /// A reply entered the reply lane.
    ReplyEnqueued,
    /// The writer entered a unit's pause.
    PauseEntered,
    /// The writer is about to run a recheck (outside the queue lock).
    RecheckEntered,
    /// After a write permission, before the backend call.
    Permitted,
    /// The drain is about to parse a piece, with the VT lock held.
    PieceParsing,
    /// The writer is about to decide a unit's first write permission, with the queue lock
    /// held: every check but the final deadline check has passed.
    Dispatching,
}

impl ProbePoint {
    fn index(self) -> usize {
        match self {
            ProbePoint::ReplyEnqueued => 0,
            ProbePoint::PauseEntered => 1,
            ProbePoint::RecheckEntered => 2,
            ProbePoint::Permitted => 3,
            ProbePoint::PieceParsing => 4,
            ProbePoint::Dispatching => 5,
        }
    }
}

#[derive(Default)]
struct ProbeState {
    counts: [u64; 6],
    armed: [Option<u64>; 6],
    released: Vec<u64>,
    next: u64,
}

#[derive(Default)]
struct ProbeInner {
    st: Mutex<ProbeState>,
    cv: Condvar,
}

/// Test signals of one session.
#[derive(Clone, Default)]
pub struct Probe {
    inner: Arc<ProbeInner>,
}

impl Probe {
    /// Counts an occurrence of `p`; blocks while a park armed for it is held.
    pub(crate) fn hit(&self, p: ProbePoint) {
        let i = p.index();
        let mut st = lock(&self.inner.st);
        st.counts[i] += 1;
        let park = st.armed[i].take();
        self.inner.cv.notify_all();
        if let Some(id) = park {
            while !st.released.contains(&id) {
                st = self
                    .inner
                    .cv
                    .wait(st)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
    }

    /// Whether the `n`-th occurrence of `p` was reached by `deadline`.
    #[must_use]
    pub fn wait(&self, p: ProbePoint, n: u64, deadline: Instant) -> bool {
        let i = p.index();
        let mut st = lock(&self.inner.st);
        loop {
            if st.counts[i] >= n {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self
                .inner
                .cv
                .wait_timeout(st, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// Occurrences of `p` so far.
    #[must_use]
    pub fn count(&self, p: ProbePoint) -> u64 {
        lock(&self.inner.st).counts[p.index()]
    }

    /// The next occurrence of `p` blocks until the returned `Park` is released or dropped.
    #[must_use]
    pub fn park(&self, p: ProbePoint) -> Park {
        let mut st = lock(&self.inner.st);
        st.next += 1;
        let id = st.next;
        st.armed[p.index()] = Some(id);
        Park {
            inner: self.inner.clone(),
            point: p,
            id,
        }
    }
}

/// An armed or holding park.
pub struct Park {
    inner: Arc<ProbeInner>,
    point: ProbePoint,
    id: u64,
}

impl Park {
    pub fn release(self) {}
}

impl Drop for Park {
    fn drop(&mut self) {
        let mut st = lock(&self.inner.st);
        st.released.push(self.id);
        let i = self.point.index();
        if st.armed[i] == Some(self.id) {
            st.armed[i] = None;
        }
        drop(st);
        self.inner.cv.notify_all();
    }
}

/// The identity of a live or zombie process by pid, in the native backend's format (pid,
/// start stamp, process group); `None` when no process has that pid. A test route only:
/// the public lookup arrives with its first consumer (`docs/design/pty-unix.md` §2.1).
///
/// # Errors
///
/// The process exists but its record cannot be read (another user's process).
#[cfg(target_os = "macos")]
pub fn identify(pid: u32) -> io::Result<Option<ProcessIdentity>> {
    crate::unix::identify(pid)
}

/// How many times the macOS writer has entered its full-queue `poll`, process-wide.
#[cfg(target_os = "macos")]
#[must_use]
pub fn unix_writer_polls() -> u64 {
    crate::unix::WRITER_POLLS.load(Ordering::Acquire)
}

#[cfg(windows)]
pub use crate::windows::hooks::{
    FaultReport, Gate, GateControl, TraceEntry, Watch, WindowsFault, gate,
};

/// Injects `f` into the next native spawn on the calling thread.
#[cfg(windows)]
pub fn windows_inject_next(f: WindowsFault) {
    crate::windows::hooks::inject_next(f);
}

/// What the last native spawn on the calling thread did: the call trace, the gate and
/// milestone results, and the suspended child's exit.
#[cfg(windows)]
#[must_use]
pub fn windows_fault_report() -> Option<FaultReport> {
    crate::windows::hooks::report()
}

/// Arms `f` on the calling (spawn) thread; it runs there right after the next teardown's
/// kill.
#[cfg(windows)]
pub fn windows_on_teardown_kill(f: Box<dyn FnOnce() + Send>) {
    crate::windows::hooks::on_teardown_kill(f);
}

/// The identity of a live process by pid (an exited one is `None`), with an empty group.
///
/// # Errors
///
/// The process exists but cannot be opened or read.
#[cfg(windows)]
pub fn identify(pid: u32) -> io::Result<Option<ProcessIdentity>> {
    crate::windows::hooks::identify(pid)
}

/// Terminates `pid` through one handle after checking its creation time on that handle;
/// true when the termination call succeeded.
#[cfg(windows)]
#[must_use]
pub fn kill_verified(pid: u32, start_time: &str) -> bool {
    crate::windows::hooks::kill_verified(pid, start_time)
}

/// One handle on the process `id` names, verified by creation time on that handle.
///
/// # Errors
///
/// The process cannot be opened, or another process has its pid.
#[cfg(windows)]
pub fn windows_watch(id: &ProcessIdentity) -> io::Result<Watch> {
    crate::windows::hooks::watch(id)
}

/// `GetProcessHandleCount` of this process.
#[cfg(windows)]
#[must_use]
pub fn process_handle_count() -> Option<u32> {
    crate::windows::sys_handle_count()
}

/// Waits (bounded by `timeout`) for the threads of close workers whose close returned;
/// true when all have exited.
#[cfg(windows)]
#[must_use]
pub fn windows_close_threads_exited(timeout: Duration) -> bool {
    crate::windows::hooks::close_threads_exited(timeout)
}

/// Whether `pid` is in the job of the live session led by `leader_pid`.
///
/// # Errors
///
/// No live session has that leader, or `pid` cannot be opened.
#[cfg(windows)]
pub fn windows_in_job(leader_pid: u32, pid: u32) -> io::Result<bool> {
    crate::windows::hooks::in_job(leader_pid, pid)
}
