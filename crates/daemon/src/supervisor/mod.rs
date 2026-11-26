//! The supervisor (`docs/design/supervisor.md`, cited as "design §N"): the launch and process
//! side of the launch commands. A CLI started by `spawn` or `resume` runs in a daemon-owned
//! PTY; each step of its life reaches the store as one explicit event in one job; every
//! launch ends with `launch_exited`, its death confirmed exactly when an owned close or a
//! spawn error proves it; and every process the supervisor creates is counted until it is
//! closed or reaped.
//!
//! The rules (design §2.1):
//!
//! 1. **One job per transition**, each re-reading the rows it decides on.
//! 2. **Launch operations outlive requests**: a command's effect hands its work to a
//!    supervisor task ([`ops`]) and awaits its result.
//! 3. **One op lock per launch** ([`live::LiveLaunch`]).
//! 4. **Every owned process carries a tracker token** ([`tracker`]).
//! 5. **The full list on every kill.**
//! 6. **What a kill shows goes to the record or the log.**
//! 7. **Startup causes for the current, started, unbound launch only.**

mod adapters;
pub(crate) mod commands;
mod components;
mod events;
mod exit;
mod live;
mod ops;
pub(crate) mod prober;
pub(crate) mod record;
mod restart;
mod screen;
pub mod send;
mod tick;
mod tracker;
pub(crate) mod workspace;

#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use chaldea_core::adapter::{HostOs, ReadinessMode};
use chaldea_core::ids::{Agent, LaunchId, ThreadId};
use chaldea_pty_runtime::orphan::OrphanReport;
use chaldea_pty_runtime::{CloseBounds, ProcessIdentity, PtyRuntime, PtySize};
use futures_util::future::BoxFuture;

use crate::hook::HookConfig;
use crate::http::{LifecycleSink, MonotonicClock, RevocationNudge, SystemMonotonic};
use crate::store::Store;

pub use components::SupervisorComponents;
pub(crate) use exit::ExitStep;
pub(crate) use live::LiveLaunch;
pub(crate) use screen::SCREEN_PATH;
pub(crate) use tick::TickStep;

use live::LiveMap;
use tracker::Tracker;
use workspace::GitSlot;

/// How long a kill call waits for a kill-state lock another call holds (design §2.3 Stop
/// step 4).
pub(crate) const KILL_LOCK_WAIT: Duration = Duration::from_millis(250);
/// A stop's lock wait and re-read end this long after its op's start.
pub(crate) const STOP_LOCK: Duration = Duration::from_secs(10);
/// A kill's proof counts only when the op received it before the kill's submission plus
/// this (the task's 11 s and 1 s for the pool to start it).
pub(crate) const PROOF_CUTOFF: Duration = Duration::from_secs(12);
/// `confirmed_within_deadline` is measured against `RequestStart` plus this (mvp P-3).
pub(crate) const STOP_REPORT: Duration = Duration::from_secs(5);
/// A non-manual launch unbound this long after its committed start raises
/// `session_start_timeout`.
pub(crate) const SESSION_START_TIMEOUT: Duration = Duration::from_secs(20);
/// The tick's period.
pub(crate) const TICK: Duration = Duration::from_secs(1);
/// The git probe's bound.
pub(crate) const GIT_PROBE: Duration = Duration::from_secs(5);
/// An exit op's lock wait rounds and its total.
pub(crate) const EXIT_ROUND: Duration = Duration::from_secs(10);
pub(crate) const EXIT_TOTAL: Duration = Duration::from_secs(60);
/// The commands' request budget.
pub(crate) const COMMAND_DEADLINE: Duration = Duration::from_secs(30);
/// The restart prober (`docs/design/supervisor-recovery.md` §2.3): one probe's deadline, the
/// grace its join gets after it, the probes run at once, the candidate read's bound, and the
/// slot wait's.
pub(crate) const PROBE_BUDGET: Duration = Duration::from_secs(5);
pub(crate) const PROBE_JOIN_GRACE: Duration = Duration::from_secs(1);
pub(crate) const PROBE_SLOTS: usize = 4;
pub(crate) const PROBE_READ: Duration = Duration::from_secs(10);
pub(crate) const SLOT_WAIT: Duration = Duration::from_secs(30);

/// A point where a test holds the supervisor (design §6 "Host speed"). The daemon's own
/// configuration has no pauses; only a component built by a test sets one.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pause {
    /// After the reservation committed, before the token and `Session::spawn`.
    AfterReserve,
    /// After taking the tracker token, before `Session::spawn` (on the op).
    BeforeSpawn,
    /// After `Session::spawn` returned, before the reference is sampled.
    AfterSpawn,
    /// After the reference was sampled, before `Tracker::while_accepting`.
    BeforePublish,
    /// Inside the publication, under the tracker's mutex (blocking).
    InPublish,
    /// After the publication, before the start job.
    BeforeStartJob,
    /// Inside the start job (blocking, on the store writer).
    InStartJob,
    /// A stop op at its start, before its lock wait.
    StopStart,
    /// Between a stop's seal and the submission of its `StopRequested` write.
    StopAfterSeal,
    /// Inside the `StopRequested` job (blocking, on the store writer).
    InStopRequested,
    /// A stop's kill submission, after it read the submission instant and before the
    /// kill task is spawned (blocking, in the `StopRequested` write's `on_submit`).
    KillSubmitted,
    /// A stop op after its `StopRequested` write answered and the kill was classified,
    /// before the exit job.
    StopWritten,
    /// A kill task right after it fixed its budget (blocking).
    KillTaskStart,
    /// A kill task after `close_by` returned, before the task returns (blocking).
    KillTaskEnd,
    /// The kill task before it publishes a non-proving report to its slot (blocking).
    KillReportPublish,
    /// A stop whose kill join was not received in time, before it reads the report slot
    /// (async).
    KillReportRead,
    /// Inside the close's drain step: before `close_by` (blocking).
    BeforeCloseBy,
    /// A `close --stop` after its stop, before the closing job.
    BeforeClosingJob,
    /// An exit op after taking the session out of its slot, before its kill task starts.
    ExitTookSession,
    /// An exit op at its start, before it reads its entry.
    ExitStart,
    /// An exit op at the start of each lock round, before it computes the round's end.
    ExitRound,
    /// An exit op before its lock wait.
    ExitBeforeLock,
    /// The participant's close task, before it returns (blocking).
    ShutdownCloseEnd,
    /// A late creation's close, before it returns (blocking).
    LateCloseEnd,
    /// The git probe holding the spawn lock, before `Tracker::admit` (blocking).
    GitBeforeAdmit,
    /// The git probe after its admit, before the OS spawn (blocking).
    GitAfterAdmit,
    /// The git probe right after the child was created (blocking).
    GitAfterCreate,
    /// The shutdown git step's reap, before each `try_reap` (blocking).
    GitShutdownReap,
    /// The shutdown git step after reading the slot (async).
    GitShutdownRead,
    /// The restart prober at its start, before it reads admission (async).
    ProberStart,
    /// A probe's blocking task once started, before it reads admission (blocking).
    ProbeQueued,
    /// A probe's start, under the tracker's mutex, before its start mark (blocking).
    ProbeStarting,
    /// A VT snapshot's task before it copies the screen (blocking; unit tests only).
    #[cfg(test)]
    SnapshotCopy,
    /// The screen route after the thread read check, before the live lookup and the
    /// snapshot's deadline (unit tests only).
    #[cfg(test)]
    ScreenRead,
}

/// The test seam of [`Pause`]: the daemon's own is [`NoPauses`].
#[doc(hidden)]
pub trait Pauses: Send + Sync + 'static {
    /// An async point on a supervisor task.
    fn at(&self, p: Pause) -> BoxFuture<'static, ()> {
        let _ = p;
        Box::pin(async {})
    }
    /// A point on a blocking thread.
    fn blocking(&self, p: Pause) {
        let _ = p;
    }
}

/// No pauses.
#[doc(hidden)]
#[derive(Debug, Default, Clone, Copy)]
pub struct NoPauses;

impl Pauses for NoPauses {}

/// The program and its leading arguments for an agent; `None` when it is not found.
#[doc(hidden)]
pub type ProgramFn = Arc<dyn Fn(Agent) -> Option<Vec<OsString>> + Send + Sync>;

/// The unowned kill of recorded identities: `chaldea_pty_runtime::orphan::kill_recorded`
/// outside tests, its deadline in real time.
#[doc(hidden)]
pub type KillRecordedFn = Arc<dyn Fn(&[ProcessIdentity], Instant) -> OrphanReport + Send + Sync>;

/// The supervisor's settings. [`SupervisorConfig::default`] is the daemon's.
pub struct SupervisorConfig {
    /// The PTY backend.
    pub runtime: Arc<dyn PtyRuntime>,
    /// The clock every supervisor instant is read on; tests pass the server's.
    pub(crate) clock: Arc<dyn MonotonicClock>,
    /// The program of each agent: `None` resolves `claude` or `codex` on the daemon's own
    /// `PATH`; tests replace it with FakeCli.
    #[doc(hidden)]
    pub program: Option<ProgramFn>,
    /// The daemon environment a launch's environment is built from: `None` reads the
    /// process's own.
    pub(crate) env: Option<Vec<(OsString, OsString)>>,
    /// The `git` the workspace probe runs, and its leading arguments.
    pub(crate) git: Vec<OsString>,
    /// The environment the workspace probe's `git` starts from (unit tests), before git's
    /// own variables are removed: `None` inherits the process's own.
    #[cfg(test)]
    pub(crate) git_env: Option<Vec<(OsString, OsString)>>,
    /// The kill phase of a stop's cleanup budget (5 s).
    pub(crate) kill_budget: Duration,
    /// The close's other bounds.
    pub(crate) close_bounds: CloseBounds,
    /// The PTY size of a launch.
    pub(crate) pty_size: PtySize,
    /// The test pause points.
    #[doc(hidden)]
    pub pauses: Arc<dyn Pauses>,
    /// The hook settings (the tick's buffer expiry).
    pub(crate) hook: HookConfig,
    /// 10b's exit and tick steps, added before startup step 9 (design §2.11).
    pub(crate) exit_steps: Vec<Arc<dyn ExitStep>>,
    pub(crate) tick_steps: Vec<Arc<dyn TickStep>>,
    /// Whether the tick task runs (tests drive [`Supervisor::tick`] themselves).
    pub(crate) tick_task: bool,
    /// The hook decoder of the send side (send §2.5): the daemon's decodes nothing until
    /// an adapter PR adds one; tests pass `FakeDecoder`.
    #[doc(hidden)]
    pub decoder: Arc<dyn send::HookDecoder>,
    /// The readiness mode of every row, when a unit test overrides the capability table's.
    pub(crate) readiness_mode: Option<ReadinessMode>,
    /// Whether each launch's send worker and the driver run (unit tests that drive no send
    /// turn them off).
    pub(crate) send_workers: bool,
    /// The restart prober's OS work (`docs/design/supervisor-recovery.md` §2.3).
    #[doc(hidden)]
    pub kill_recorded: KillRecordedFn,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        SupervisorConfig {
            runtime: chaldea_pty_runtime::native(),
            clock: Arc::new(SystemMonotonic),
            program: None,
            env: None,
            git: vec![OsString::from("git")],
            #[cfg(test)]
            git_env: None,
            kill_budget: Duration::from_secs(5),
            close_bounds: CloseBounds::default(),
            pty_size: PtySize::new(120, 40).unwrap_or_else(|_| unreachable_size()),
            pauses: Arc::new(NoPauses),
            hook: HookConfig::default(),
            exit_steps: Vec::new(),
            tick_steps: Vec::new(),
            tick_task: true,
            decoder: Arc::new(send::NoDecoder),
            readiness_mode: None,
            send_workers: true,
            kill_recorded: Arc::new(chaldea_pty_runtime::orphan::kill_recorded),
        }
    }
}

fn unreachable_size() -> PtySize {
    PtySize::new(80, 24).unwrap_or_else(|e| panic!("80x24 is a valid size: {e:?}"))
}

/// The shutdown participant's instants, stored before admission closes (design §2.9).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Window {
    pub(crate) close_end: Instant,
    pub(crate) end: Instant,
}

/// The launch and process side of the daemon.
pub(crate) struct Supervisor {
    pub(crate) host_os: HostOs,
    pub(crate) store: Store,
    pub(crate) sink: Arc<dyn LifecycleSink>,
    pub(crate) port: u16,
    pub(crate) cfg: SupervisorConfig,
    pub(crate) live: LiveMap,
    pub(crate) tracker: Arc<Tracker>,
    pub(crate) git: Arc<GitSlot>,
    pub(crate) nudge: OnceLock<RevocationNudge>,
    pub(crate) window: OnceLock<Window>,
    /// The send and ack side (send §2.1).
    pub(crate) send: Arc<send::SendSide>,
    /// The runtime ops run on (the daemon's).
    pub(crate) rt: tokio::runtime::Handle,
    /// This daemon run's instance UUID (`BuildCtx.instance`): a probe's `probe_run`.
    pub(crate) instance: String,
    /// The restart prober's concurrency slots (`PROBE_SLOTS`).
    pub(crate) probe_slots: Arc<tokio::sync::Semaphore>,
}

impl Supervisor {
    pub(crate) fn clock(&self) -> &dyn MonotonicClock {
        self.cfg.clock.as_ref()
    }

    pub(crate) fn now(&self) -> Instant {
        self.cfg.clock.now()
    }

    pub(crate) fn real(&self, at: Instant) -> Instant {
        self.cfg.clock.real(at)
    }

    pub(crate) fn timer(&self, at: Instant) -> tokio::time::Instant {
        tokio::time::Instant::from_std(self.real(at))
    }

    pub(crate) fn pauses(&self) -> &dyn Pauses {
        self.cfg.pauses.as_ref()
    }

    pub(crate) fn nudge(&self) {
        if let Some(n) = self.nudge.get() {
            n.nudge();
        }
    }

    /// The entry of `launch` when its thread is `thread` (design §2.11 "Lookup by
    /// generation").
    pub(crate) fn live(&self, thread: ThreadId, launch: LaunchId) -> Option<Arc<LiveLaunch>> {
        self.live
            .lock()
            .get(&launch)
            .filter(|l| l.thread_id == thread)
            .cloned()
    }

    pub(crate) fn entry(&self, launch: LaunchId) -> Option<Arc<LiveLaunch>> {
        self.live.lock().get(&launch).cloned()
    }

    pub(crate) fn remove(&self, launch: LaunchId) {
        self.live.lock().remove(&launch);
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
