//! The shutdown gate (design §2.8): the first request fixes the cause and the cutoff
//! together; later fatal outcomes and panics only raise the exit reason's severity; a Windows
//! console end is recorded separately and makes the coordinator exit at once.

use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::FailReason;
use super::log::Logger;
use crate::http::{LifecycleSink, StoreFatal};

/// A stop signal or console event that requests a graceful shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalName {
    Term,
    Int,
    Hup,
    CtrlC,
    CtrlBreak,
}

impl SignalName {
    fn as_str(self) -> &'static str {
        match self {
            SignalName::Term => "SIGTERM",
            SignalName::Int => "SIGINT",
            SignalName::Hup => "SIGHUP",
            SignalName::CtrlC => "Ctrl-C",
            SignalName::CtrlBreak => "Ctrl-Break",
        }
    }
}

/// Why the daemon stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopCause {
    Signal(SignalName),
    /// The Windows stop event (on Unix `daemon stop` arrives as `Signal(Term)`).
    StopRequest,
    StoreFatal(StoreFatal),
    Panic,
    StartupFailed(FailReason),
}

impl StopCause {
    fn severity(&self) -> u8 {
        match self {
            StopCause::StoreFatal(_) => 1,
            StopCause::Panic => 2,
            _ => 0,
        }
    }
}

impl fmt::Display for StopCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StopCause::Signal(s) => write!(f, "signal {}", s.as_str()),
            StopCause::StopRequest => f.write_str("stop request"),
            StopCause::StoreFatal(s) => write!(f, "store fatal: {}", fatal_label(s)),
            StopCause::Panic => f.write_str("panic"),
            StopCause::StartupFailed(r) => write!(f, "startup failed: {r}"),
        }
    }
}

fn fatal_label(f: &StoreFatal) -> String {
    match f {
        StoreFatal::CommitUnknown => "commit_unknown".to_owned(),
        StoreFatal::WriterGone => "writer_gone".to_owned(),
        StoreFatal::Failed => "failed".to_owned(),
        StoreFatal::ReceiptUncompleted { command_id } => {
            format!("receipt_uncompleted {command_id}")
        }
    }
}

/// A Windows console end (design §2.8): a crash exit, not a shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleEvent {
    Close,
    Logoff,
    Shutdown,
}

/// The cause and the cutoff, published together once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stopping {
    pub cause: StopCause,
    pub cutoff: Instant,
}

/// The one deciding point of the shutdown (design §2.15).
pub struct ShutdownGate {
    first: OnceLock<Stopping>,
    /// The most severe later outcome: 0 none, 1 store fatal, 2 panic.
    worst: AtomicU8,
    crash: OnceLock<ConsoleEvent>,
    wake: Notify,
    total: Duration,
    log: Logger,
}

impl fmt::Debug for ShutdownGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShutdownGate")
            .field("first", &self.first.get())
            .field("worst", &self.worst.load(Ordering::SeqCst))
            .field("crash", &self.crash.get())
            .finish_non_exhaustive()
    }
}

impl ShutdownGate {
    pub(crate) fn new(total: Duration, log: Logger) -> ShutdownGate {
        ShutdownGate {
            first: OnceLock::new(),
            worst: AtomicU8::new(0),
            crash: OnceLock::new(),
            wake: Notify::new(),
            total,
            log,
        }
    }

    /// Non-blocking and callable from any thread, a panic hook, or a `LifecycleSink`.
    /// Publishes first, then queues its diagnostic; true when this call was the first.
    pub fn request(&self, cause: StopCause) -> bool {
        let severity = cause.severity();
        let lost = self
            .first
            .set(Stopping {
                cause,
                cutoff: Instant::now() + self.total,
            })
            .err();
        if severity > 0 {
            self.worst.fetch_max(severity, Ordering::SeqCst);
        }
        self.wake.notify_one();
        let was_first = lost.is_none();
        match (lost, self.first.get()) {
            (None, Some(s)) => self.log.info(&format!("stopping: {}", s.cause)),
            (Some(l), Some(s)) => self
                .log
                .info(&format!("{}: already stopping ({})", l.cause, s.cause)),
            _ => {}
        }
        was_first
    }

    /// A Windows console end: records it and wakes the coordinator, which exits at once.
    pub fn crash_exit(&self, event: ConsoleEvent) {
        let _ = self.crash.set(event);
        self.wake.notify_one();
        self.log.warn(&format!(
            "console end ({event:?}): exiting without a shutdown"
        ));
    }

    /// Whether a shutdown was requested.
    #[must_use]
    pub fn fired(&self) -> bool {
        self.first.get().is_some()
    }

    #[must_use]
    pub fn stopping(&self) -> Option<&Stopping> {
        self.first.get()
    }

    pub(crate) fn crashed(&self) -> Option<ConsoleEvent> {
        self.crash.get().copied()
    }

    pub(crate) fn worst(&self) -> u8 {
        self.worst.load(Ordering::SeqCst)
    }

    /// Waits for the next wake (a request or a crash).
    pub(crate) async fn woken(&self) {
        self.wake.notified().await;
    }
}

/// The gate as later components hold it (design §2.14).
#[derive(Clone)]
pub struct GateHandle(pub(crate) Arc<ShutdownGate>);

impl GateHandle {
    /// [`ShutdownGate::request`].
    pub fn request(&self, cause: StopCause) -> bool {
        self.0.request(cause)
    }

    #[must_use]
    pub fn fired(&self) -> bool {
        self.0.fired()
    }
}

impl fmt::Debug for GateHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The `LifecycleSink` the HTTP server gets: every fatal store outcome stops the daemon.
pub(crate) struct GateSink(pub(crate) Arc<ShutdownGate>);

impl LifecycleSink for GateSink {
    fn store_fatal(&self, f: StoreFatal) {
        let label = fatal_label(&f);
        self.0.request(StopCause::StoreFatal(f));
        self.0.log.error(&format!("store fatal: {label}"));
    }
}

// ---- The active gate the panic hook reads (design §2.3) ----

static ACTIVE_GATE: Mutex<Option<Weak<ShutdownGate>>> = Mutex::new(None);

/// Registers `gate` as the process's active gate.
pub(crate) fn register(gate: &Arc<ShutdownGate>) {
    *ACTIVE_GATE.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::downgrade(gate));
}

/// The active gate, if a run registered one and it is alive. Holds the mutex only to upgrade.
pub(crate) fn active() -> Option<Arc<ShutdownGate>> {
    let slot = ACTIVE_GATE.lock().unwrap_or_else(PoisonError::into_inner);
    slot.as_ref().and_then(Weak::upgrade)
}

/// Whether any gate is registered (tests of `run_isolated`).
#[cfg(test)]
pub(crate) fn any_registered() -> bool {
    ACTIVE_GATE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some()
}
