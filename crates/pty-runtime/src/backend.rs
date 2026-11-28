//! The backend seam (`docs/design/pty-runtime.md` §2.1): what each OS backend supplies
//! and what a spawn hands to the session.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::drain::{Drain, DrainEvent};
use crate::session::SharedSink;

pub(crate) mod sealed {
    pub trait Sealed {}
}

/// Starts a child in a new PTY. Sealed: implemented by the native backends and, behind the
/// `fake` feature, `FakePty`. Object safe; callers hold `Arc<dyn PtyRuntime>` and call only
/// [`crate::Session::spawn`], since they cannot build a [`DrainSink`].
pub trait PtyRuntime: Send + Sync + sealed::Sealed {
    /// Opens a PTY at `spec.size`, starts a [`Drain`] over its output read end feeding
    /// `output`, and starts the child. On failure everything created is released, or the
    /// error says what could not be confirmed released.
    #[allow(
        clippy::result_large_err,
        reason = "spawn is rare; the error carries its cleanup evidence by value"
    )]
    fn spawn(&self, spec: &SpawnSpec, output: DrainSink) -> Result<SpawnedPty, SpawnError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    /// `argv[0]` is an absolute path to a native executable.
    pub argv: Vec<OsString>,
    pub cwd: PathBuf,
    /// The child's complete environment; nothing is inherited.
    pub env: Vec<(OsString, OsString)>,
    pub size: PtySize,
}

/// Validated PTY size: 2 ≤ cols ≤ 4095 (a wide pair fits), 1 ≤ rows ≤ 4095, so the pixel
/// sizes the responder reports (8×16 px cells) fit in `u16`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtySize {
    cols: u16,
    rows: u16,
}

/// The largest column or row count.
pub const MAX_DIMENSION: u16 = 4095;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeError {
    pub cols: u16,
    pub rows: u16,
}

impl fmt::Display for SizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid PTY size {}x{} (cols 2..={MAX_DIMENSION}, rows 1..={MAX_DIMENSION})",
            self.cols, self.rows
        )
    }
}

impl std::error::Error for SizeError {}

impl PtySize {
    pub fn new(cols: u16, rows: u16) -> Result<PtySize, SizeError> {
        if (2..=MAX_DIMENSION).contains(&cols) && (1..=MAX_DIMENSION).contains(&rows) {
            Ok(PtySize { cols, rows })
        } else {
            Err(SizeError { cols, rows })
        }
    }

    #[must_use]
    pub fn cols(self) -> u16 {
        self.cols
    }

    #[must_use]
    pub fn rows(self) -> u16 {
        self.rows
    }
}

/// The session's output sink (VT, responder, subscribers); opaque outside the crate.
pub struct DrainSink {
    pub(crate) inner: SinkInner,
}

pub(crate) enum SinkInner {
    Session(SharedSink),
    #[cfg(test)]
    Test(Box<dyn FnMut(DrainEvent<'_>) + Send>),
}

impl DrainSink {
    pub(crate) fn deliver(&mut self, event: DrainEvent<'_>) {
        match &mut self.inner {
            SinkInner::Session(s) => s.deliver(event),
            #[cfg(test)]
            SinkInner::Test(f) => f(event),
        }
    }

    #[cfg(test)]
    pub(crate) fn test(f: impl FnMut(DrainEvent<'_>) + Send + 'static) -> DrainSink {
        DrainSink {
            inner: SinkInner::Test(Box::new(f)),
        }
    }
}

impl fmt::Debug for DrainSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DrainSink")
    }
}

/// Methods take `&self` and may be called concurrently from different threads (the waiter
/// polls `wait_timeout` while `kill_tree` runs).
pub trait PtyChild: Send + Sync {
    fn identity(&self) -> &ProcessIdentity;
    /// Never blocks past `timeout` and holds no lock `kill_tree` needs.
    fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<ChildExit>>;
    /// Kills the whole tree plus the recorded descendants in `extra`; idempotent; returns
    /// by `deadline`.
    ///
    /// A kill-state lock another call holds is waited for only until `lock_by` (capped at
    /// `deadline`): a lock held at the first try and obtained only at or after `lock_by`
    /// counts as not taken, and the call returns a busy report with no signal. A call made at
    /// or after `deadline` makes no OS call.
    ///
    /// The caller passes its full list of recorded descendants on every call: the first
    /// stop, a retry after a busy report or `tree_dead: false`, and a close after an earlier
    /// kill. A report's survivors are diagnostic (one entry per pid, possibly naming the
    /// leader instead of an extra) and are never the list to resubmit.
    fn kill_tree(
        &self,
        extra: &[ProcessIdentity],
        lock_by: Instant,
        deadline: Instant,
    ) -> KillReport;
}

pub trait PtyControl: Send {
    /// Resizes the PTY. Not bounded by the crate: `ConPTY`'s resize can wait on conhost.
    fn resize(&self, size: PtySize) -> io::Result<()>;
    /// Releases the PTY itself (Unix master, Windows pseudoconsole); returns by `deadline`.
    fn close(self: Box<Self>, deadline: Instant) -> CloseReport;
}

/// Maps to `launch_started { pid, start_time, pgid_or_job }`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: String,
    pub group: String,
}

/// Maps to `launch_exited { code, signal }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// `method` maps to `launch_death_confirmed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillReport {
    pub method: String,
    pub tree_dead: bool,
    pub survivors: Vec<ProcessIdentity>,
    /// What this call did; `tree_dead: true` comes only with `Signalled` or `AlreadyDead`.
    pub attempt: KillAttempt,
    /// The backend's clock read right after the read that proved the tree dead; `None`
    /// unless `tree_dead`. A proof from the last observation started before the deadline
    /// counts, so this instant can fall slightly after the deadline.
    pub proved_at: Option<Instant>,
}

/// The methods of a report whose call did not get the kill-state lock.
const BUSY_METHODS: [&str; 3] = ["unix_busy", "windows_busy", "fake_busy"];

impl KillReport {
    /// Whether the call did not get the kill-state lock (`unix_busy`, `windows_busy`,
    /// `fake_busy`): it sent nothing, and a retry with the same list may succeed.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        BUSY_METHODS.contains(&self.method.as_str())
    }
}

/// What one `kill_tree` call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillAttempt {
    /// The call made at least one signal (Unix) or termination (Windows) call.
    Signalled,
    /// The tree was proven dead before any signal.
    AlreadyDead,
    /// No signal was sent: the call was busy, expired, or (macOS) found ownership lost
    /// before any signal.
    NotAttempted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseReport {
    /// `None`: did not return by the deadline.
    pub elapsed: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeverStartedKind {
    NotFound,
    PermissionDenied,
    Setup,
}

#[derive(Debug)]
pub enum SpawnError {
    /// The program provably never executed; any process created for it has been reaped:
    /// the confirmed `spawn_failed`. Resources created before it (a pseudoconsole and its
    /// drain) may still be unreleased.
    NeverStarted {
        kind: NeverStartedKind,
        error: io::Error,
        cleanup: Cleanup,
    },
    /// A process existed and setup then failed. A Codex positional prompt may have run.
    StartedThenFailed {
        stage: &'static str,
        error: io::Error,
        identity: Option<ProcessIdentity>,
        cleanup: Cleanup,
    },
}

impl SpawnError {
    #[must_use]
    pub fn cleanup(&self) -> &Cleanup {
        match self {
            SpawnError::NeverStarted { cleanup, .. }
            | SpawnError::StartedThenFailed { cleanup, .. } => cleanup,
        }
    }
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpawnError::NeverStarted { kind, error, .. } => {
                write!(f, "child never started ({kind:?}): {error}")
            }
            SpawnError::StartedThenFailed { stage, error, .. } => {
                write!(
                    f,
                    "setup failed after the child started, at {stage}: {error}"
                )
            }
        }
    }
}

impl std::error::Error for SpawnError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cleanup {
    Confirmed,
    Unresolved {
        kill: Option<KillReport>,
        close: Option<CloseReport>,
        drain_abandoned: bool,
    },
}

/// Bounds of the failure teardown of an unconsumed [`SpawnedPty`].
pub(crate) const TEARDOWN_KILL: Duration = Duration::from_secs(5);
pub(crate) const TEARDOWN_CLOSE: Duration = Duration::from_secs(2);
pub(crate) const TEARDOWN_DRAIN: Duration = Duration::from_secs(2);

/// Internal owning guard for everything a spawn created; `Session::spawn` consumes it.
/// Dropped unconsumed it runs the failure teardown: `kill_tree(&[], 5 s, 5 s)` → drop input →
/// `control.close(2 s)` → drain finish (2 s).
pub struct SpawnedPty {
    parts: Option<SpawnedParts>,
}

pub(crate) struct SpawnedParts {
    pub(crate) child: Arc<dyn PtyChild>,
    pub(crate) output: Drain,
    pub(crate) input: Box<dyn Write + Send>,
    pub(crate) control: Box<dyn PtyControl>,
    pub(crate) size: PtySize,
    pub(crate) evidence: Vec<(&'static str, String)>,
}

impl SpawnedPty {
    pub(crate) fn new(parts: SpawnedParts) -> SpawnedPty {
        SpawnedPty { parts: Some(parts) }
    }

    pub(crate) fn into_parts(mut self) -> SpawnedParts {
        self.parts.take().expect("SpawnedPty is consumed only once")
    }
}

impl SpawnedParts {
    /// The failure teardown, in order: kill, drop input, close the PTY, finish the drain.
    pub(crate) fn teardown(self) -> Cleanup {
        let SpawnedParts {
            child,
            output,
            input,
            control,
            ..
        } = self;
        let deadline = Instant::now() + TEARDOWN_KILL;
        let kill = child.kill_tree(&[], deadline, deadline);
        #[cfg(all(windows, any(test, feature = "fake")))]
        crate::windows::hooks::after_teardown_kill();
        drop(input);
        #[cfg(all(windows, any(test, feature = "fake")))]
        crate::windows::hooks::before_teardown_close();
        let close = control.close(Instant::now() + TEARDOWN_CLOSE);
        #[cfg(all(test, target_os = "macos"))]
        crate::unix::trace::record("drain_finish");
        #[cfg(all(windows, any(test, feature = "fake")))]
        crate::windows::hooks::trace("drain_finish", true, 0, Ok(0));
        let end = output.finish(TEARDOWN_DRAIN);
        let drain_abandoned = end.is_none();
        if kill.tree_dead && close.elapsed.is_some() && !drain_abandoned {
            Cleanup::Confirmed
        } else {
            Cleanup::Unresolved {
                kill: Some(kill),
                close: Some(close),
                drain_abandoned,
            }
        }
    }
}

/// The failure teardown of an unconsumed guard. A panic inside it (a backend's kill or close)
/// is caught and the parts not yet torn down drop as values: the drop may run during an
/// unwind, where an escaping panic aborts the process.
impl Drop for SpawnedPty {
    fn drop(&mut self) {
        if let Some(p) = self.parts.take() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.teardown()));
        }
    }
}

impl fmt::Debug for SpawnedPty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpawnedPty")
            .field("consumed", &self.parts.is_none())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{KillAttempt, KillReport};

    /// Busy is exactly the three lock-not-taken methods; every other method a backend or
    /// FakePty reports is not busy.
    #[test]
    fn busy_methods() {
        let methods = [
            ("unix_busy", true),
            ("windows_busy", true),
            ("fake_busy", true),
            ("unix_killpg_session_scan", false),
            ("unix_ownership_lost", false),
            ("windows_job", false),
            ("windows_job_incomplete", false),
            ("fake", false),
            ("fake_unconfirmed", false),
        ];
        for (method, busy) in methods {
            let r = KillReport {
                method: method.to_owned(),
                tree_dead: false,
                survivors: Vec::new(),
                attempt: KillAttempt::NotAttempted,
                proved_at: None,
            };
            assert_eq!(r.is_busy(), busy, "{method}");
        }
    }

    /// An unconsumed spawn dropped during an unwind, its teardown's kill panicking: the
    /// panic stays inside the drop, so the unwind ends where it is caught, not in an abort.
    #[cfg(feature = "fake")]
    #[test]
    fn unconsumed_spawn_survives_an_unwind() {
        use crate::fake::{FakeEvent, FakePty, FakeScript, KillBehavior};
        use crate::{PtyRuntime, PtySize, SpawnSpec};
        let fake = FakePty::new();
        fake.script_next(FakeScript {
            kill: KillBehavior::Panic,
            ..FakeScript::default()
        });
        let spec = SpawnSpec {
            argv: vec!["/work/bin/agent".into()],
            cwd: "/work/repo".into(),
            env: Vec::new(),
            size: PtySize::new(80, 24).unwrap(),
        };
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _spawned = fake.spawn(&spec, super::DrainSink::test(|_| {})).unwrap();
            crate::fake::inject_panic("injected fault while the spawn is unconsumed");
        }));
        assert!(r.is_err());
        let child = fake.take_child().unwrap();
        assert!(child.log().contains(&FakeEvent::Kill { extra: Vec::new() }));
        child.close_output();
    }
}
