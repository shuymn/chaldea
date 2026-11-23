//! The daemon process (`docs/design/daemon-lifecycle.md`, cited as "design §N"): the state
//! directory and its files, the single-instance lock, discovery, the port, the owner token
//! file, the startup order, the one shutdown decision and its budget, the process-wide signal
//! state, logging, and the panic hook.
//!
//! The rules (design §2.1):
//!
//! 1. **The OS decides exclusivity and liveness**: one daemon per state directory is an OS
//!    file lock released by the OS at exit; a stop is an OS signal (Unix) or an event in the
//!    account's global namespace (Windows).
//! 2. **One coordinator decides the shutdown**: every trigger calls [`ShutdownGate::request`];
//!    the first call fixes the cause and the cutoff together.
//! 3. **Order is code**: startup is one task whose steps run in a fixed order (the listener
//!    does not exist before reconciliation committed); shutdown is one function.
//! 4. **Nothing is adopted, removed, or overwritten without proof.**
//! 5. **Lifecycle resources outlive every unwind**: the lock is released in-process only by
//!    the coordinator; the runtime is never dropped while work may remain.
//! 6. **Every wait has a deadline**, and every ending ends the process.

mod files;
mod gate;
mod log;
mod panic;
mod process;
mod ready;
mod shutdown;
mod signals;
mod startup;
mod state_dir;

#[cfg(test)]
pub(crate) mod faults;
#[cfg(test)]
mod tests;

use std::fmt;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;

use crate::http::{HttpServer, LifecycleSink, Mounts};
use crate::store::{JobError, Store, WriteTx};

pub use files::{
    Discovery, DiscoveryRecord, InstanceLock, LockError, LockProbe, RecordState, discover, probe,
    read_record,
};
pub use gate::{ConsoleEvent, GateHandle, ShutdownGate, SignalName, StopCause, Stopping};
pub use log::diag;
pub use process::prepare_process;
pub use ready::{ReadyOpener, ReadyWriter};
pub use state_dir::{STATE_DIR_ENV, resolve as resolve_state_dir};

pub(crate) use panic::TaskKindGuard;

/// The files of the state directory (design §2.4).
pub mod names {
    pub const MARKER: &str = "CHALDEA_STATE";
    pub const LOCK: &str = "daemon.lock";
    pub const RECORD: &str = "daemon.json";
    pub const PORT: &str = "port";
    pub const OWNER_TOKEN: &str = "owner.token";
    pub const LOG: &str = "daemon.log";
    pub const LOG_OLD: &str = "daemon.log.1";
    pub const DB: &str = "chaldea.db";
    /// The database and its sidecars, which SQLite opens by path.
    pub const DB_FILES: [&str; 4] = [
        "chaldea.db",
        "chaldea.db-wal",
        "chaldea.db-shm",
        "chaldea.db-journal",
    ];
}

/// The first port a first start tries, and the last (design §2.6).
pub const PORT_RANGE: std::ops::RangeInclusive<u16> = 7373..=7472;
/// How long [`InstanceLock::acquire`] retries on contention (design §2.5).
pub const LOCK_RETRY: Duration = Duration::from_secs(2);
/// A detached daemon's log rotates at this size (design §2.13).
pub const LOG_CAP: u64 = 8 * 1024 * 1024;

/// How the daemon was started.
pub enum RunMode {
    /// `chaldea daemon run` in a terminal: logs to stderr.
    Foreground,
    /// The child `daemon start` launched (8b): its readiness verdict goes to `ready`, its log
    /// to `daemon.log` from step 6.
    DetachedChild { ready: ReadyOpener, via: DetachVia },
}

impl fmt::Debug for RunMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunMode::Foreground => f.write_str("Foreground"),
            RunMode::DetachedChild { via, .. } => write!(f, "DetachedChild({via:?})"),
        }
    }
}

/// Which detach path launched a detached daemon (recorded as `detached` in `daemon.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachVia {
    Launchd,
    Setsid,
    Windows,
}

impl DetachVia {
    fn as_str(self) -> &'static str {
        match self {
            DetachVia::Launchd => "launchd",
            DetachVia::Setsid => "setsid",
            DetachVia::Windows => "windows",
        }
    }
}

/// The one shutdown budget and the ends measured back from its cutoff `C` (design §2.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    /// `C` = first request + `total`.
    pub total: Duration,
    /// The startup task has until `C − startup_join_before` to reach a step boundary.
    pub startup_join_before: Duration,
    /// HTTP drains until `C − http_before`.
    pub http_before: Duration,
    /// Participants run until `C − store_before`; the store has the rest.
    pub store_before: Duration,
    /// How long past `C` the coordinator observes the store's report.
    pub report_grace: Duration,
    /// The log target switch and the final flush.
    pub log_flush: Duration,
}

impl Budgets {
    pub const DEFAULT: Budgets = Budgets {
        total: Duration::from_secs(10),
        startup_join_before: Duration::from_secs(6),
        http_before: Duration::from_secs(5),
        store_before: Duration::from_secs(3),
        report_grace: Duration::from_millis(250),
        log_flush: Duration::from_millis(100),
    };
}

impl Default for Budgets {
    fn default() -> Self {
        Budgets::DEFAULT
    }
}

/// What [`run`] runs.
#[derive(Debug)]
pub struct DaemonConfig {
    /// Absolute; checked and canonicalized by `run` (design §2.4).
    pub state_dir: PathBuf,
    /// `--port`; `None`: the saved port, else the first free port of [`PORT_RANGE`].
    pub port: Option<u16>,
    pub mode: RunMode,
    pub budgets: Budgets,
}

/// One restart reconciliation: a write job run at step 9, before the listener exists.
pub struct Reconciler {
    pub name: &'static str,
    #[allow(clippy::type_complexity, reason = "the job shape of Store::submit")]
    pub job: Box<dyn FnOnce(&mut WriteTx<'_>) -> Result<u64, JobError> + Send>,
}

/// What the components get at step 11.
pub struct BuildCtx {
    pub store: Store,
    pub gate: GateHandle,
    pub sink: Arc<dyn LifecycleSink>,
    pub port: u16,
    pub state_dir: PathBuf,
}

/// What the components built: the HTTP mounts and the shutdown participants (7c adds the
/// `CommandRegistry`, design §2.14).
pub struct Built {
    pub mounts: Mounts,
    pub participants: Vec<Box<dyn ShutdownParticipant>>,
}

/// Something stopped after HTTP and before the store, sharing the time between `C − 5 s` and
/// `C − 3 s` (design §2.14).
pub trait ShutdownParticipant: Send + Sync {
    fn name(&self) -> &'static str;
    /// Stops by `end`; the coordinator stops waiting at `end` either way.
    fn shutdown(&self, end: Instant) -> BoxFuture<'_, ()>;
}

/// What later PRs plug in, called at fixed steps (design §2.14).
pub trait Components: Send + 'static {
    /// Step 9, before the listener exists; run in order, each its own write job.
    fn reconcilers(&mut self, store: &Store) -> Vec<Reconciler>;
    /// Step 11, after bind.
    fn build(&mut self, ctx: &BuildCtx) -> Built;
    /// Step 12, after `HttpServer::new`, before `serve`.
    fn attach(&mut self, server: &HttpServer) {
        let _ = server;
    }
}

/// PR 8's components: the receipts reconciler and [`Mounts::mvp`].
#[derive(Debug, Default, Clone, Copy)]
pub struct MvpComponents;

/// The receipts reconciler (mvp §2.4 "Restart"): every `in_progress` receipt of a crashed
/// run becomes `unknown_after_crash`.
#[must_use]
pub fn receipts_reconciler() -> Reconciler {
    Reconciler {
        name: "receipts_mark_unknown_after_crash",
        job: Box::new(|tx| Ok(tx.receipts_mark_unknown_after_crash()?)),
    }
}

impl Components for MvpComponents {
    fn reconcilers(&mut self, _store: &Store) -> Vec<Reconciler> {
        vec![receipts_reconciler()]
    }

    fn build(&mut self, _ctx: &BuildCtx) -> Built {
        Built {
            mounts: Mounts::mvp(),
            participants: Vec::new(),
        }
    }
}

/// The CLI's exit codes (design §2.8, D14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Ok = 0,
    Failed = 1,
    Usage = 2,
    AlreadyRunning = 3,
    Timeout = 4,
}

impl ExitCode {
    #[must_use]
    pub fn code(self) -> i32 {
        self as i32
    }
}

/// Why a daemon ended other than by a clean stop (design §2.8); exactly these tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    StateDir,
    LockIo,
    AlreadyRunning,
    ParentJob,
    TokenFile,
    PortFile,
    PortUnavailable,
    StoreOpen,
    Internal,
    StoreFatal,
    Panic,
    LockHeldAtExit,
    ConsoleEnd,
    StartupUnjoined,
}

impl FailReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FailReason::StateDir => "state_dir",
            FailReason::LockIo => "lock_io",
            FailReason::AlreadyRunning => "already_running",
            FailReason::ParentJob => "parent_job",
            FailReason::TokenFile => "token_file",
            FailReason::PortFile => "port_file",
            FailReason::PortUnavailable => "port_unavailable",
            FailReason::StoreOpen => "store_open",
            FailReason::Internal => "internal",
            FailReason::StoreFatal => "store_fatal",
            FailReason::Panic => "panic",
            FailReason::LockHeldAtExit => "lock_held_at_exit",
            FailReason::ConsoleEnd => "console_end",
            FailReason::StartupUnjoined => "startup_unjoined",
        }
    }
}

impl fmt::Display for FailReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What became of the single-instance lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockDisposition {
    /// Not acquired by this run (stopped before step 5, refused, or held by another daemon).
    NeverTaken,
    /// Released in-process under design §2.8 step 6's conditions.
    Released,
    /// Leaked: the OS releases it when the process exits.
    HeldUntilExit,
}

/// How [`run`] ended; `main` passes `code` to `std::process::exit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitReport {
    pub code: ExitCode,
    pub reason: Option<FailReason>,
    pub lock: LockDisposition,
}

impl ExitReport {
    pub(crate) fn failed(reason: FailReason, lock: LockDisposition) -> ExitReport {
        ExitReport {
            code: ExitCode::Failed,
            reason: Some(reason),
            lock,
        }
    }
}

/// Where a run sends its diagnostics and what it registers process-wide.
pub(crate) struct RunEnv {
    /// `run`: registers the active gate and starts the signal listeners; `run_isolated`:
    /// neither.
    pub(crate) process: bool,
    pub(crate) log: log::Logger,
    #[cfg(test)]
    pub(crate) faults: Arc<faults::Faults>,
    /// `run_isolated`'s target, restored when the coordinator leaves `daemon.log`.
    #[cfg(test)]
    pub(crate) test_log: Option<log::TestLog>,
}

/// The process entry (design §2.2): registers the active gate and the process log target,
/// runs one daemon to its end, and returns what `main` passes to `std::process::exit`.
/// Assumes [`prepare_process`] ran; called once per process.
#[must_use]
pub fn run(cfg: DaemonConfig, components: Box<dyn Components>) -> ExitReport {
    let Some(log) = log::process_logger() else {
        // `prepare_process` did not run: nothing could be logged or caught.
        let _ = io::Write::write_all(
            &mut io::stderr(),
            b"chaldea: internal error: the process was not prepared\n",
        );
        return ExitReport::failed(FailReason::Internal, LockDisposition::NeverTaken);
    };
    run_env(
        cfg,
        components,
        RunEnv {
            process: true,
            log,
            #[cfg(test)]
            faults: Arc::default(),
            #[cfg(test)]
            test_log: None,
        },
    )
}

/// The same startup and coordinator as [`run`] with nothing registered process-wide: no
/// active gate, no signal listeners, diagnostics into `log`.
#[cfg(test)]
pub(crate) fn run_isolated(
    cfg: DaemonConfig,
    components: Box<dyn Components>,
    log: log::TestLog,
    faults: Arc<faults::Faults>,
) -> ExitReport {
    let logger = log::Logger::start(log::LogTarget::Test(log.clone()));
    if faults
        .withhold_log_ack
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        logger.withhold_switch_ack();
    }
    let report = run_env(
        cfg,
        components,
        RunEnv {
            process: false,
            log: logger.clone(),
            faults,
            test_log: Some(log),
        },
    );
    logger.flush(Duration::from_secs(1));
    report
}

/// [`run`] with test fault points, for helper processes (which ran `prepare_process`).
#[cfg(test)]
pub(crate) fn run_process_with(
    cfg: DaemonConfig,
    components: Box<dyn Components>,
    faults: Arc<faults::Faults>,
) -> ExitReport {
    let log = log::process_logger().expect("prepare_process ran");
    if faults
        .withhold_log_ack
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        log.withhold_switch_ack();
    }
    run_env(
        cfg,
        components,
        RunEnv {
            process: true,
            log,
            faults,
            test_log: None,
        },
    )
}

fn run_env(cfg: DaemonConfig, components: Box<dyn Components>, env: RunEnv) -> ExitReport {
    let log = env.log.clone();
    match catch_unwind(AssertUnwindSafe(move || {
        shutdown::run_daemon(cfg, components, env)
    })) {
        Ok(report) => report,
        Err(_) => {
            // A panic before the runtime existed or one escaping it (design §2.9).
            log.error("exiting: code 1 (panic)");
            log.flush(Budgets::DEFAULT.log_flush);
            ExitReport::failed(FailReason::Panic, LockDisposition::HeldUntilExit)
        }
    }
}

/// The one place the daemon crate creates a child process (design §2.11): under the
/// process-wide spawn lock shared with the PTY backends.
///
/// # Errors
///
/// The spawn error.
#[allow(
    clippy::disallowed_methods,
    reason = "the one spawn site; every child is created under the spawn lock"
)]
pub fn spawn_locked(cmd: &mut Command) -> io::Result<Child> {
    chaldea_pty_runtime::with_spawn_lock(|| cmd.spawn())
}

/// Flushes the process log queue for at most `wait` (for `main` before `std::process::exit`).
pub fn flush_process_log(wait: Duration) {
    if let Some(l) = log::process_logger() {
        l.flush(wait);
    }
}
