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
    ChildExit, Cleanup, CloseReport, DrainSink, KillAttempt, KillReport, NeverStartedKind,
    ProcessIdentity, PtyChild, PtyControl, PtyRuntime, PtySize, SpawnError, SpawnSpec,
    SpawnedParts, SpawnedPty, sealed,
};
use crate::drain::Drain;
use crate::session::lock;

/// How long `spawn` waits for its startup output to be parsed.
const STARTUP_PARSE: Duration = Duration::from_secs(5);
/// The byte `fail_output` writes to wake a blocked read; the reader swallows it.
const WAKE: u8 = 0;

/// The method of a fake report whose call did not get the kill-state lock.
const METHOD_BUSY: &str = "fake_busy";
/// The method of every other fake report but `Unconfirmed`'s.
const METHOD: &str = "fake";
/// The method of `KillBehavior::Unconfirmed`'s report (not busy, as `unix_ownership_lost`).
const METHOD_UNCONFIRMED: &str = "fake_unconfirmed";

/// What `kill_tree` does once it holds the kill-state lock with the child alive and the call
/// not expired. Every behaviour but `Unconfirmed` and `Panic` first performs the fake's
/// signal-equivalent operation (`FakeEvent::Signal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KillBehavior {
    /// The child exits at once and the tree is reported dead.
    #[default]
    ExitOnKill,
    /// Nothing dies; `kill_tree` uses its whole deadline and reports survivors.
    Ignore,
    /// The child exits after this long; dead if that is within the deadline.
    ExitAfter(Duration),
    /// Returns at once with no signal: `NotAttempted`, `tree_dead: false`, a method that is
    /// not busy.
    Unconfirmed,
    /// The calls before the `n`-th signal return at once (`Signalled`, `tree_dead: false`,
    /// not busy); the `n`-th proves the tree dead.
    ProveOnCall(u32),
    /// Panics inside `kill_tree`.
    Panic,
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
    /// The child's `wait_timeout` blocks until [`FakeChild::release_waiter`], so a close
    /// returns with `pty-wait` abandoned.
    pub hold_waiter: bool,
}

/// What the fake child observed, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeEvent {
    Spawned,
    WriteEntered,
    Write(usize),
    Resize(PtySize),
    ResizeFailed,
    /// A `kill_tree` call, at its entry.
    Kill {
        extra: Vec<ProcessIdentity>,
    },
    /// The signal-equivalent operation, made exactly when a native backend would signal.
    Signal,
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
    /// The kill-state lock: whether a `kill_tree` call or a test hold has it.
    kill_lock: Mutex<bool>,
    kill_cv: Condvar,
    /// Signals made so far (`ProveOnCall`).
    signals: AtomicU32,
    /// `wait_timeout` fails (`fail_wait`).
    wait_fails: AtomicBool,
    /// `wait_timeout` blocks while set (`hold_waiter`).
    waiter_held: Mutex<bool>,
    waiter_cv: Condvar,
    /// Every `PtyChild` reference to this process is gone (`wait_dropped`).
    dropped: Mutex<bool>,
    dropped_cv: Condvar,
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

    /// Takes the kill-state lock: at once when free; otherwise waits until `lock_by`, and a
    /// lock freed only at or after `lock_by` counts as not taken.
    fn kill_lock(&self, lock_by: Instant) -> bool {
        let mut held = lock(&self.kill_lock);
        if *held {
            loop {
                let left = lock_by.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return false;
                }
                held = self
                    .kill_cv
                    .wait_timeout(held, left)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
                if !*held {
                    if Instant::now() >= lock_by {
                        return false;
                    }
                    break;
                }
            }
        }
        *held = true;
        true
    }

    fn unlock_kill(&self) {
        *lock(&self.kill_lock) = false;
        self.kill_cv.notify_all();
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
            kill_lock: Mutex::new(false),
            kill_cv: Condvar::new(),
            signals: AtomicU32::new(0),
            wait_fails: AtomicBool::new(false),
            waiter_held: Mutex::new(script.hold_waiter),
            waiter_cv: Condvar::new(),
            dropped: Mutex::new(false),
            dropped_cv: Condvar::new(),
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

/// The last `PtyChild` reference going, as a native child's handles close with it.
impl Drop for FakeProcess {
    fn drop(&mut self) {
        *lock(&self.0.dropped) = true;
        self.0.dropped_cv.notify_all();
    }
}

const KILLED: ChildExit = ChildExit {
    code: None,
    signal: Some(9),
};

impl PtyChild for FakeProcess {
    fn identity(&self) -> &ProcessIdentity {
        &self.0.identity
    }

    fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<ChildExit>> {
        let p = &self.0;
        {
            let mut held = lock(&p.waiter_held);
            while *held {
                held = p
                    .waiter_cv
                    .wait(held)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        let fails = || p.wait_fails.load(Ordering::Acquire);
        let x = lock(&p.exit);
        let (x, _) = p
            .exit_cv
            .wait_timeout_while(x, timeout, |x| x.is_none() && !fails())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if x.is_none() && fails() {
            return Err(io::Error::other("injected wait failure"));
        }
        Ok(*x)
    }

    fn kill_tree(
        &self,
        extra: &[ProcessIdentity],
        lock_by: Instant,
        deadline: Instant,
    ) -> KillReport {
        self.kill_with(extra, lock_by, deadline, || {})
    }
}

impl FakeProcess {
    /// `kill_tree`, with `before_signal` run after the earlier deadline checks, right before
    /// the log's mutex is taken for the signal.
    fn kill_with(
        &self,
        extra: &[ProcessIdentity],
        lock_by: Instant,
        deadline: Instant,
        before_signal: impl FnOnce(),
    ) -> KillReport {
        let p = &self.0;
        p.record(FakeEvent::Kill {
            extra: extra.to_vec(),
        });
        let report = |method: &str, attempt: KillAttempt, proved_at: Option<Instant>| {
            let dead = proved_at.is_some();
            KillReport {
                method: method.to_owned(),
                tree_dead: dead,
                survivors: if dead {
                    Vec::new()
                } else {
                    vec![p.identity.clone()]
                },
                attempt,
                proved_at,
            }
        };
        // An expired call performs no operation, as a native expired call makes no OS call.
        if Instant::now() >= deadline {
            return report(METHOD, KillAttempt::NotAttempted, None);
        }
        if !p.kill_lock(lock_by.min(deadline)) {
            return report(METHOD_BUSY, KillAttempt::NotAttempted, None);
        }
        let _held = KillLockHold {
            proc: Some(p.clone()),
        };
        if p.exited() {
            return report(METHOD, KillAttempt::AlreadyDead, Some(Instant::now()));
        }
        // The deadline is checked again right before the signal-equivalent operation, as a
        // native call checks it before each OS call.
        if Instant::now() >= deadline {
            return report(METHOD, KillAttempt::NotAttempted, None);
        }
        // The signal-equivalent operation: the deadline is checked once more under the log's
        // mutex, right before the signal is recorded and counted. `None`: not performed.
        let signal = || {
            before_signal();
            let mut log = lock(&p.log);
            if Instant::now() >= deadline {
                return None;
            }
            log.push(FakeEvent::Signal);
            Some(p.signals.fetch_add(1, Ordering::AcqRel) + 1)
        };
        let proved = |attempt| report(METHOD, attempt, Some(Instant::now()));
        let not_attempted = || report(METHOD, KillAttempt::NotAttempted, None);
        match p.kill {
            KillBehavior::Panic => panic!("injected kill_tree panic"),
            KillBehavior::Unconfirmed => {
                report(METHOD_UNCONFIRMED, KillAttempt::NotAttempted, None)
            }
            KillBehavior::ExitOnKill => {
                if signal().is_none() {
                    return not_attempted();
                }
                p.exit(KILLED);
                proved(KillAttempt::Signalled)
            }
            KillBehavior::Ignore => {
                if signal().is_none() {
                    return not_attempted();
                }
                sleep_until(deadline);
                report(METHOD, KillAttempt::Signalled, None)
            }
            KillBehavior::ProveOnCall(n) => {
                let Some(count) = signal() else {
                    return not_attempted();
                };
                if count >= n {
                    p.exit(KILLED);
                    proved(KillAttempt::Signalled)
                } else {
                    report(METHOD, KillAttempt::Signalled, None)
                }
            }
            KillBehavior::ExitAfter(d) => {
                if signal().is_none() {
                    return not_attempted();
                }
                let at = Instant::now() + d;
                if at <= deadline {
                    sleep_until(at);
                    p.exit(KILLED);
                    proved(KillAttempt::Signalled)
                } else {
                    let later = p.clone();
                    std::thread::spawn(move || {
                        sleep_until(at);
                        later.exit(KILLED);
                    });
                    sleep_until(deadline);
                    if p.exited() {
                        proved(KillAttempt::Signalled)
                    } else {
                        report(METHOD, KillAttempt::Signalled, None)
                    }
                }
            }
        }
    }
}

/// Holds the fake's kill-state lock until dropped (also while unwinding).
pub struct KillLockHold {
    proc: Option<Arc<Proc>>,
}

impl KillLockHold {
    pub fn release(self) {}
}

impl Drop for KillLockHold {
    fn drop(&mut self) {
        if let Some(p) = self.proc.take() {
            p.unlock_kill();
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

    /// The waiter's next and every later `wait_timeout` fails: the session reports
    /// `WaitFailed` and no exit.
    pub fn fail_wait(&self) {
        self.proc.wait_fails.store(true, Ordering::Release);
        let _x = lock(&self.proc.exit);
        self.proc.exit_cv.notify_all();
    }

    /// Waits until every `PtyChild` reference to the process is gone (the session's and its
    /// waiter's), as a native child's last handle closes, until `deadline`; true when gone.
    #[must_use]
    pub fn wait_dropped(&self, deadline: Instant) -> bool {
        let d = lock(&self.proc.dropped);
        let left = deadline.saturating_duration_since(Instant::now());
        let (d, _) = self
            .proc
            .dropped_cv
            .wait_timeout_while(d, left, |d| !*d)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *d
    }

    /// Ends the block of `FakeScript.hold_waiter`.
    pub fn release_waiter(&self) {
        *lock(&self.proc.waiter_held) = false;
        self.proc.waiter_cv.notify_all();
    }

    /// Takes the kill-state lock (waiting while a kill call holds it) and holds it until the
    /// returned guard is released or dropped: a `kill_tree` call meanwhile waits for it until
    /// its `lock_by`, and one that gets it only at or after `lock_by` reports `fake_busy`.
    #[must_use]
    pub fn hold_kill_lock(&self) -> KillLockHold {
        let mut held = lock(&self.proc.kill_lock);
        while *held {
            held = self
                .proc
                .kill_cv
                .wait(held)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *held = true;
        KillLockHold {
            proc: Some(self.proc.clone()),
        }
    }

    /// Signal-equivalent operations so far.
    #[must_use]
    pub fn signals(&self) -> u32 {
        self.proc.signals.load(Ordering::Acquire)
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
    FaultReport, Gate, GateControl, OutputProbe, TraceEntry, Watch, WindowsFault, gate,
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

/// Starts (`true`) or stops keeping the handles of close workers whose close returned, for
/// [`windows_close_threads_exited`]; ordinary closes drop them.
#[cfg(windows)]
pub fn windows_track_close_threads(on: bool) {
    crate::windows::hooks::track_close_threads(on);
}

/// Waits (bounded by `timeout`) for the tracked threads of close workers whose close
/// returned; true when all of them (the batch this call took) have exited. Use one tracking
/// arm at a time and one sweep at a time.
#[cfg(windows)]
#[must_use]
pub fn windows_close_threads_exited(timeout: Duration) -> bool {
    crate::windows::hooks::close_threads_exited(timeout)
}

/// `ActiveProcesses` of the job of the live session led by `leader_pid`.
///
/// # Errors
///
/// No live session has that leader, or the query failed.
#[cfg(windows)]
pub fn windows_job_active(leader_pid: u32) -> io::Result<u32> {
    crate::windows::hooks::job_active(leader_pid)
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

// ---- `host` test helpers (docs/design/daemon-lifecycle.md §2.2) ----

/// Sets `SIGCHLD` to `SIG_IGN` for this process (a careless parent's state, PR8-CL-17).
///
/// # Errors
///
/// The OS refused the change.
#[cfg(target_os = "macos")]
pub fn ignore_sigchld() -> io::Result<()> {
    crate::unix::sys::ignore_sigchld()
}

/// Blocks `signals` in the calling thread's mask (PR8-CL-17).
///
/// # Errors
///
/// The OS refused the change.
#[cfg(target_os = "macos")]
pub fn block_signals(signals: &[crate::host::UnixSignal]) -> io::Result<()> {
    use crate::host::UnixSignal;
    let raw: Vec<i32> = signals
        .iter()
        .map(|s| match s {
            UnixSignal::Term => libc::SIGTERM,
            UnixSignal::Int => libc::SIGINT,
            UnixSignal::Hup => libc::SIGHUP,
            UnixSignal::Chld => libc::SIGCHLD,
        })
        .collect();
    crate::unix::sys::block_signals(&raw)
}

/// Runs a fixed system tool by absolute path under the spawn lock and requires success.
#[cfg(any(target_os = "macos", windows))]
fn run_tool(program: &str, args: &[&std::ffi::OsStr]) -> io::Result<()> {
    let out = crate::with_spawn_lock(|| {
        std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
    })?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{program} failed: {:?} {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}

/// Adds an extended ACL entry allowing `everyone` to read `path` (`chmod +a`).
///
/// # Errors
///
/// `chmod` failed.
#[cfg(target_os = "macos")]
pub fn grant_everyone_read_acl(path: &std::path::Path) -> io::Result<()> {
    run_tool(
        "/bin/chmod",
        &[
            "+a".as_ref(),
            "everyone allow read".as_ref(),
            path.as_os_str(),
        ],
    )
}

/// Adds an allow ACE for `BUILTIN\Users` (read) to `path` (`icacls`).
///
/// # Errors
///
/// `icacls` failed.
#[cfg(windows)]
pub fn grant_users_read(path: &std::path::Path) -> io::Result<()> {
    let icacls = system32("icacls.exe")?;
    run_tool(
        &icacls,
        &[
            path.as_os_str(),
            "/grant".as_ref(),
            "*S-1-5-32-545:(R)".as_ref(),
        ],
    )
}

#[cfg(windows)]
fn system32(exe: &str) -> io::Result<String> {
    let root = std::env::var("SystemRoot").map_err(io::Error::other)?;
    Ok(format!("{root}\\System32\\{exe}"))
}

/// Makes `link` point at `target`: a symlink on macOS, a directory junction on Windows (no
/// privilege needed; `target` must be a directory there).
///
/// # Errors
///
/// The link could not be made.
pub fn make_link(link: &std::path::Path, target: &std::path::Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        let cmd = system32("cmd.exe")?;
        run_tool(
            &cmd,
            &[
                "/d".as_ref(),
                "/c".as_ref(),
                "mklink".as_ref(),
                "/J".as_ref(),
                link.as_os_str(),
                target.as_os_str(),
            ],
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (link, target);
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

/// The console events `GenerateConsoleCtrlEvent` can send.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtrlEvent {
    CtrlC,
    CtrlBreak,
}

/// `GenerateConsoleCtrlEvent(kind, group)` on this process's console.
///
/// # Errors
///
/// The OS refused it.
#[cfg(windows)]
pub fn send_ctrl_event(kind: CtrlEvent, group: u32) -> io::Result<()> {
    let ev = match kind {
        CtrlEvent::CtrlC => 0,
        CtrlEvent::CtrlBreak => 1,
    };
    crate::windows::sys::generate_ctrl_event(ev, group)
}

/// Makes this process ignore Ctrl-C (an attribute its later children inherit).
///
/// # Errors
///
/// The OS refused it.
#[cfg(windows)]
pub fn ignore_ctrl_c() -> io::Result<()> {
    crate::windows::sys::ignore_ctrl_c()
}

/// The pids attached to this process's console.
///
/// # Errors
///
/// The process has no console.
#[cfg(windows)]
pub fn console_processes() -> io::Result<Vec<u32>> {
    crate::windows::sys::console_processes()
}

/// `FOLDERID_Public`, resolved through `SHGetKnownFolderPath`.
///
/// # Errors
///
/// The lookup failed.
#[cfg(windows)]
pub fn public_folder() -> io::Result<std::path::PathBuf> {
    crate::windows::sys::public_folder()
}

/// Opens the named stop event with `EVENT_MODIFY_STATE` and sets it (what `daemon stop`
/// does on Windows).
///
/// # Errors
///
/// No such event, or access denied.
#[cfg(windows)]
pub fn set_stop_event(name: &str) -> io::Result<()> {
    let h = crate::windows::sys::open_event_modify(name)?;
    crate::windows::sys::set_event(&h)
}

/// The owner SID and the allow-ACE SIDs of `path` (Windows), for read-back assertions.
///
/// # Errors
///
/// The entry cannot be opened or its descriptor read.
#[cfg(windows)]
pub fn descriptor_sids(path: &std::path::Path) -> io::Result<(String, Vec<String>)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsHandle;
    let f = std::fs::OpenOptions::new()
        .access_mode(windows_sys::Win32::Storage::FileSystem::READ_CONTROL)
        .custom_flags(
            windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT
                | windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS,
        )
        .open(path)?;
    let sec = crate::windows::sys::security_of(f.as_handle())?;
    let allows = sec
        .dacl
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.ace_type == crate::windows::sys::ACE_ALLOWED)
        .filter_map(|a| a.sid)
        .collect();
    Ok((sec.owner, allows))
}

/// The owner SID and the allow-ACE SIDs of the named event (Windows), for read-back assertions.
///
/// # Errors
///
/// No such event, or its descriptor cannot be read.
#[cfg(windows)]
pub fn event_sids(name: &str) -> io::Result<(String, Vec<String>)> {
    use std::os::windows::io::AsHandle;
    let h = crate::windows::sys::open_event_read_control(name)?;
    let sec = crate::windows::sys::security_of_object(
        h.as_handle(),
        windows_sys::Win32::Security::Authorization::SE_KERNEL_OBJECT,
    )?;
    let allows = sec
        .dacl
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.ace_type == crate::windows::sys::ACE_ALLOWED)
        .filter_map(|a| a.sid)
        .collect();
    Ok((sec.owner, allows))
}

/// The current user's SID (Windows).
///
/// # Errors
///
/// The token could not be read.
#[cfg(windows)]
pub fn user_sid() -> io::Result<String> {
    crate::windows::sys::user_sid()
}

/// A job object a test owns (Windows, PR8-CL-11, CL-12): limits set at creation, processes
/// assigned by pid. Closing it (or dropping it) closes the test's only handle, so a
/// kill-on-close job then kills what is still in it. A process assigned to two jobs in turn
/// is in the second nested under the first.
#[cfg(windows)]
pub struct TestJob {
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl TestJob {
    /// A new job with these limits.
    ///
    /// # Errors
    ///
    /// The job cannot be created or limited.
    pub fn new(limits: crate::host::JobLimits) -> io::Result<TestJob> {
        use windows_sys::Win32::System::JobObjects::{
            JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
        };
        let job = crate::windows::sys::create_job()?;
        let mut flags = 0;
        if limits.kill_on_close {
            flags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        if limits.breakaway_ok {
            flags |= JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        }
        if limits.silent_breakaway_ok {
            flags |= JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
        }
        crate::windows::sys::set_job_limit(&job, flags)?;
        Ok(TestJob { job })
    }

    /// Assigns the process `pid` to this job.
    ///
    /// # Errors
    ///
    /// The process cannot be opened or assigned.
    pub fn assign(&self, pid: u32) -> io::Result<()> {
        use windows_sys::Win32::System::Threading::{PROCESS_SET_QUOTA, PROCESS_TERMINATE};
        let p = crate::windows::sys::open_process(pid, PROCESS_SET_QUOTA | PROCESS_TERMINATE)?;
        crate::windows::sys::assign(&self.job, &p)
    }

    /// Whether the process `pid` is in this job.
    ///
    /// # Errors
    ///
    /// The process cannot be opened.
    pub fn contains(&self, pid: u32) -> io::Result<bool> {
        let p = crate::windows::sys::open_process(pid, crate::windows::QUERY_SYNC)?;
        crate::windows::sys::in_job(&p, &self.job)
    }

    /// Closes the test's handle to the job.
    pub fn close(self) {
        drop(self.job);
    }
}

#[cfg(windows)]
impl std::fmt::Debug for TestJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TestJob")
    }
}

/// Whether the process `pid` is in any job (`IsProcessInJob(process, NULL)`, Windows).
///
/// # Errors
///
/// The process cannot be opened.
#[cfg(windows)]
pub fn process_in_any_job(pid: u32) -> io::Result<bool> {
    let p = crate::windows::sys::open_process(pid, crate::windows::QUERY_SYNC)?;
    crate::windows::sys::handle_in_any_job(&p)
}

/// Resumes every thread of the process `pid`, one created with `CREATE_SUSPENDED` by a caller
/// holding no thread handle (std's spawn); the number resumed (Windows).
///
/// # Errors
///
/// The thread snapshot, an open, or a resume failed.
#[cfg(windows)]
pub fn resume_process(pid: u32) -> io::Result<usize> {
    crate::windows::sys::resume_process_threads(pid)
}

/// Sets `HANDLE_FLAG_INHERIT` on `h`, so a child created with handle inheritance and no handle
/// list (std's spawn) inherits it (Windows).
///
/// # Errors
///
/// `SetHandleInformation` failed.
#[cfg(windows)]
pub fn make_inheritable(h: &impl std::os::windows::io::AsRawHandle) -> io::Result<()> {
    crate::windows::sys::make_inheritable(h)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::*;

    fn spawn(script: FakeScript) -> (Arc<dyn PtyChild>, FakeChild, SpawnedParts) {
        let fake = FakePty::new();
        fake.script_next(script);
        let spec = SpawnSpec {
            argv: vec![OsString::from("/work/bin/agent")],
            cwd: PathBuf::from("/work/repo"),
            env: Vec::new(),
            size: PtySize::new(80, 24).unwrap(),
        };
        let parts = fake
            .spawn(&spec, DrainSink::test(|_| {}))
            .unwrap()
            .into_parts();
        (parts.child.clone(), fake.take_child().unwrap(), parts)
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    fn signals(c: &FakeChild) -> usize {
        c.log().iter().filter(|e| **e == FakeEvent::Signal).count()
    }

    /// Releases `hold` from another thread once `Instant::now()` reached `at` (a sleep alone
    /// can end slightly before it on this clock). The thread takes the fake lock's inner
    /// mutex 50 ms before `at` and keeps it until the release, so a waiter whose wait ends
    /// meanwhile sees the release only after it: the lock is first seen free at `at` or later.
    fn release_at(hold: KillLockHold, at: Instant) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut hold = hold;
            let p = hold.proc.take().expect("a held lock");
            sleep_until(at.checked_sub(Duration::from_millis(50)).unwrap_or(at));
            let mut held = lock(&p.kill_lock);
            while Instant::now() < at {
                sleep_until(at);
            }
            *held = false;
            p.kill_cv.notify_all();
        })
    }

    #[test]
    fn busy_at_lock_by() {
        // Freed before `lock_by`: the kill runs.
        let (child, c, _parts) = spawn(FakeScript::default());
        let lock_by = Instant::now() + Duration::from_millis(500);
        let t = release_at(
            c.hold_kill_lock(),
            Instant::now() + Duration::from_millis(50),
        );
        let r = child.kill_tree(&[], lock_by, soon());
        t.join().unwrap();
        assert!(r.tree_dead && !r.is_busy(), "{r:?}");
        assert_eq!(signals(&c), 1);
        // Freed at `lock_by` and just after it, or never: busy, no signal.
        for after in [Some(0), Some(50), None] {
            let (child, c, _parts) = spawn(FakeScript::default());
            let lock_by = Instant::now() + Duration::from_millis(100);
            let hold = c.hold_kill_lock();
            let (t, kept) = match after {
                Some(ms) => (
                    Some(release_at(hold, lock_by + Duration::from_millis(ms))),
                    None,
                ),
                None => (None, Some(hold)),
            };
            let r = child.kill_tree(&[], lock_by, soon());
            drop(kept);
            assert!(Instant::now() >= lock_by, "returned before lock_by");
            assert_eq!(
                (r.method.as_str(), r.tree_dead, r.attempt, r.proved_at),
                (METHOD_BUSY, false, KillAttempt::NotAttempted, None),
                "{after:?}"
            );
            assert_eq!(signals(&c), 0, "{after:?}");
            assert!(!c.proc.exited());
            if let Some(t) = t {
                t.join().unwrap();
            }
        }
    }

    #[test]
    fn expired_and_dead_calls_signal_nothing() {
        let (child, c, _parts) = spawn(FakeScript::default());
        let past = Instant::now();
        let r = child.kill_tree(&[], past, past);
        assert_eq!(
            (r.tree_dead, r.attempt, r.proved_at),
            (false, KillAttempt::NotAttempted, None)
        );
        assert_eq!(signals(&c), 0);
        assert_eq!(
            c.log().last(),
            Some(&FakeEvent::Kill { extra: Vec::new() }),
            "every call is recorded at its entry"
        );
        c.exit(ChildExit {
            code: Some(0),
            signal: None,
        });
        let before = Instant::now();
        let r = child.kill_tree(&[], soon(), soon());
        assert_eq!((r.tree_dead, r.attempt), (true, KillAttempt::AlreadyDead));
        assert!(r.proved_at.unwrap() >= before);
        assert_eq!(signals(&c), 0);
    }

    /// A call that holds the kill-state lock but reaches its signal only after its deadline
    /// (the exit state's lock held across it) sends nothing.
    #[test]
    fn deadline_rechecked_before_the_signal() {
        let (child, c, _parts) = spawn(FakeScript::default());
        let exit_state = c.proc.exit.lock().unwrap();
        let deadline = Instant::now() + Duration::from_millis(100);
        let call = std::thread::spawn(move || child.kill_tree(&[], deadline, deadline));
        // The call recorded its entry and took the kill-state lock: it is past its entry check
        // and waits on the exit state.
        let entered = soon();
        while !(*c.proc.kill_lock.lock().unwrap()) {
            assert!(
                Instant::now() < entered,
                "the call takes the kill-state lock"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        while Instant::now() <= deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(exit_state);
        let r = call.join().unwrap();
        assert_eq!(
            (r.tree_dead, r.attempt, r.proved_at),
            (false, KillAttempt::NotAttempted, None)
        );
        assert_eq!(signals(&c), 0);
    }

    /// The deadline checked again under the log's mutex: a call held after its earlier
    /// checks and before that mutex until past its deadline records no signal.
    #[test]
    fn deadline_rechecked_under_the_log() {
        let (_child, c, _parts) = spawn(FakeScript::default());
        let (reached_tx, reached) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let process = FakeProcess(c.proc.clone());
        let deadline = Instant::now() + Duration::from_millis(500);
        let call = std::thread::spawn(move || {
            process.kill_with(&[], deadline, deadline, move || {
                reached_tx.send(()).unwrap();
                let _ = wait.recv_timeout(Duration::from_secs(20));
            })
        });
        // The call passed its earlier checks and is about to take the log's mutex.
        reached.recv_timeout(Duration::from_secs(20)).unwrap();
        while Instant::now() <= deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        release.send(()).unwrap();
        let r = call.join().unwrap();
        assert_eq!(
            (r.tree_dead, r.attempt, r.proved_at),
            (false, KillAttempt::NotAttempted, None)
        );
        assert_eq!(signals(&c), 0);
    }

    #[test]
    fn behaviours() {
        // Unconfirmed: at once, no signal, not busy.
        let (child, c, _parts) = spawn(FakeScript {
            kill: KillBehavior::Unconfirmed,
            ..FakeScript::default()
        });
        let r = child.kill_tree(&[], soon(), soon());
        assert_eq!(
            (r.method.as_str(), r.tree_dead, r.attempt),
            (METHOD_UNCONFIRMED, false, KillAttempt::NotAttempted)
        );
        assert!(!r.is_busy());
        assert_eq!(signals(&c), 0);
        // ProveOnCall(2): the first call signals and returns at once unproven, the second
        // proves.
        let (child, c, _parts) = spawn(FakeScript {
            kill: KillBehavior::ProveOnCall(2),
            ..FakeScript::default()
        });
        let r = child.kill_tree(&[], soon(), soon());
        assert_eq!((r.tree_dead, r.attempt), (false, KillAttempt::Signalled));
        assert!(!r.is_busy());
        let r = child.kill_tree(&[], soon(), soon());
        assert_eq!((r.tree_dead, r.attempt), (true, KillAttempt::Signalled));
        assert!(r.proved_at.is_some());
        assert_eq!(signals(&c), 2);
        // Panic: inside `kill_tree`, and the kill-state lock is released by the unwind.
        let (child, c, _parts) = spawn(FakeScript {
            kill: KillBehavior::Panic,
            ..FakeScript::default()
        });
        let k = child.clone();
        let panicked = std::thread::spawn(move || k.kill_tree(&[], soon(), soon())).join();
        assert!(panicked.is_err());
        drop(c.hold_kill_lock());
        // Ignore: signals, uses its deadline, unproven.
        let (child, c, _parts) = spawn(FakeScript {
            kill: KillBehavior::Ignore,
            ..FakeScript::default()
        });
        let r = child.kill_tree(&[], soon(), Instant::now() + Duration::from_millis(20));
        assert_eq!((r.tree_dead, r.attempt), (false, KillAttempt::Signalled));
        assert_eq!(signals(&c), 1);
    }

    #[test]
    fn waiter_controls() {
        let (child, c, _parts) = spawn(FakeScript::default());
        c.fail_wait();
        assert!(child.wait_timeout(Duration::from_secs(5)).is_err());
        let (child, c, _parts) = spawn(FakeScript {
            hold_waiter: true,
            ..FakeScript::default()
        });
        c.exit(KILLED);
        let k = child.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            let _ = tx.send(k.wait_timeout(Duration::from_millis(10)));
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "held past its timeout"
        );
        c.release_waiter();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
            Some(KILLED)
        );
        t.join().unwrap();
    }
}
