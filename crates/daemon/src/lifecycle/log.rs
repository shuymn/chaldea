//! The log subsystem (design §2.13): a bounded queue drained by the `daemon-log` thread into
//! the current target, the global `tracing` subscriber whose writer redacts each formatted
//! line, rotation of a detached daemon's `daemon.log`, and [`diag`] for every other
//! diagnostic.
//!
//! A stalled stderr or disk blocks only the log thread: every push is a `try_send`, and a
//! full queue drops the line and counts it.

use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use chaldea_pty_runtime::host::{self, Access};

use super::names;
use crate::http::redact_secrets;
use crate::store::{Clock, SystemClock};

/// Lines the queue holds.
pub(crate) const QUEUE_LINES: usize = 1024;
/// A failed rotation is retried this often; lines meanwhile are dropped and counted.
pub(crate) const ROTATION_RETRY: Duration = Duration::from_secs(60);

/// Redacts `chd_`, `chdt_`, and `chdp_` secrets and replaces control characters other than
/// `\n` with spaces: the one function every diagnostic outside `tracing` goes through.
#[must_use]
pub fn diag(s: &str) -> String {
    redact_secrets(s)
        .chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect()
}

/// [`diag`] on one line: newlines become spaces too.
pub(crate) fn diag_line(s: &str) -> String {
    diag(s).replace('\n', " ")
}

/// A captured target for tests.
#[derive(Clone, Default)]
pub(crate) struct TestLog(pub(crate) Arc<Mutex<Vec<String>>>);

impl TestLog {
    #[cfg(test)]
    pub(crate) fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        self.lines().concat()
    }
}

/// Where the log thread writes.
pub(crate) enum LogTarget {
    Stderr,
    /// A detached daemon before step 6: nothing to write to.
    Discard,
    File(LogFile),
    #[cfg_attr(not(test), allow(dead_code, reason = "constructed by tests only"))]
    Test(TestLog),
}

impl LogTarget {
    fn write(&mut self, line: &str) {
        match self {
            LogTarget::Stderr => {
                let _ = io::stderr().lock().write_all(line.as_bytes());
            }
            LogTarget::Discard => {}
            LogTarget::File(f) => f.write(line),
            LogTarget::Test(t) => {
                t.0.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(line.to_owned());
            }
        }
    }

    fn flush(&mut self) {
        match self {
            LogTarget::Stderr => {
                let _ = io::stderr().flush();
            }
            LogTarget::File(f) => {
                let _ = f.file.flush();
            }
            LogTarget::Discard | LogTarget::Test(_) => {}
        }
    }
}

/// `daemon.log` of a detached daemon, rotated at `cap` onto `daemon.log.1`.
pub(crate) struct LogFile {
    dir: PathBuf,
    file: File,
    size: u64,
    cap: u64,
    /// Since when rotation has failed; nothing is written until a retry succeeds.
    broken_since: Option<Instant>,
    dropped: u64,
}

impl LogFile {
    /// Opens `dir/daemon.log` without following a link, for append, created `0600`.
    pub(crate) fn open(dir: PathBuf, cap: u64) -> io::Result<LogFile> {
        let file = host::open_no_follow(&dir.join(names::LOG), Access::AppendCreate)?;
        let size = file.metadata()?.len();
        Ok(LogFile {
            dir,
            file,
            size,
            cap,
            broken_since: None,
            dropped: 0,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        let old = self.dir.join(names::LOG_OLD);
        if !host::is_plain_file_or_absent(&old)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "daemon.log.1 is not a regular file",
            ));
        }
        std::fs::rename(self.dir.join(names::LOG), &old)?;
        self.file = host::open_no_follow(&self.dir.join(names::LOG), Access::AppendCreate)?;
        self.size = 0;
        Ok(())
    }

    pub(crate) fn write(&mut self, line: &str) {
        if let Some(since) = self.broken_since
            && since.elapsed() < ROTATION_RETRY
        {
            self.dropped += 1;
            return;
        }
        if self.size >= self.cap || self.broken_since.is_some() {
            match self.rotate() {
                Ok(()) => self.broken_since = None,
                Err(_) => {
                    self.broken_since = Some(Instant::now());
                    self.dropped += 1;
                    return;
                }
            }
            if self.dropped > 0 {
                let note = stamp(
                    "WARN",
                    &format!("{} log lines dropped while rotation failed", self.dropped),
                );
                self.dropped = 0;
                self.append(&note);
            }
        }
        self.append(line);
    }

    fn append(&mut self, line: &str) {
        if self.file.write_all(line.as_bytes()).is_ok() {
            self.size += line.len() as u64;
        }
    }

    #[cfg(test)]
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }
}

enum Msg {
    Line(String),
    Target(LogTarget, SyncSender<()>),
    Flush(SyncSender<()>),
}

/// A handle on one log queue and its thread. Clones share them; the thread ends when every
/// handle is gone.
#[derive(Clone)]
pub(crate) struct Logger {
    tx: SyncSender<Msg>,
    dropped: Arc<AtomicU64>,
    /// Test-only: the thread never acknowledges a target switch.
    #[cfg(test)]
    withhold_ack: Arc<std::sync::atomic::AtomicBool>,
}

/// One formatted diagnostic line: `<time> <LEVEL> chaldea: <message>\n`, redacted.
pub(crate) fn stamp(level: &str, msg: &str) -> String {
    let ts = SystemClock.now().1;
    diag(&format!("{} {level:>5} chaldea: {msg}\n", ts.0))
}

impl Logger {
    /// Starts a queue and its `daemon-log` thread writing to `target`.
    pub(crate) fn start(target: LogTarget) -> Logger {
        let (tx, rx) = mpsc::sync_channel(QUEUE_LINES);
        #[cfg(test)]
        let withhold_ack = Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(test)]
        let thread_withhold = withhold_ack.clone();
        let spawned = std::thread::Builder::new()
            .name("daemon-log".into())
            .spawn(move || {
                drain(
                    rx,
                    target,
                    #[cfg(test)]
                    &thread_withhold,
                );
            });
        if let Err(e) = spawned {
            let _ = writeln!(io::stderr(), "chaldea: cannot start the log thread: {e}");
        }
        Logger {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            withhold_ack,
        }
    }

    /// Pushes one finished line; never blocks.
    pub(crate) fn push(&self, line: String) {
        match self.tx.try_send(Msg::Line(line)) {
            Ok(()) => {
                let n = self.dropped.swap(0, Ordering::Relaxed);
                if n > 0 {
                    let note = stamp("WARN", &format!("{n} log lines dropped (queue full)"));
                    if self.tx.try_send(Msg::Line(note)).is_err() {
                        self.dropped.fetch_add(n + 1, Ordering::Relaxed);
                    }
                }
            }
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    pub(crate) fn info(&self, msg: &str) {
        self.push(stamp("INFO", msg));
    }

    pub(crate) fn warn(&self, msg: &str) {
        self.push(stamp("WARN", msg));
    }

    pub(crate) fn error(&self, msg: &str) {
        self.push(stamp("ERROR", msg));
    }

    /// Switches the target; true when the thread acknowledged it within `wait`.
    pub(crate) fn switch(&self, target: LogTarget, wait: Duration) -> bool {
        let (ack, rx) = mpsc::sync_channel(1);
        if self.tx.try_send(Msg::Target(target, ack)).is_err() {
            return false;
        }
        rx.recv_timeout(wait).is_ok()
    }

    /// Waits up to `wait` for every line queued before this call to be written.
    pub(crate) fn flush(&self, wait: Duration) -> bool {
        let (ack, rx) = mpsc::sync_channel(1);
        if self.tx.try_send(Msg::Flush(ack)).is_err() {
            return false;
        }
        rx.recv_timeout(wait).is_ok()
    }

    #[cfg(test)]
    pub(crate) fn withhold_switch_ack(&self) {
        self.withhold_ack
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn drain(
    rx: Receiver<Msg>,
    mut target: LogTarget,
    #[cfg(test)] withhold: &std::sync::atomic::AtomicBool,
) {
    for msg in rx {
        match msg {
            Msg::Line(l) => target.write(&l),
            Msg::Target(t, ack) => {
                target.flush();
                target = t;
                #[cfg(test)]
                if withhold.load(std::sync::atomic::Ordering::SeqCst) {
                    // Kept unacknowledged: the sender stays alive, so the caller times out.
                    std::mem::forget(ack);
                    continue;
                }
                let _ = ack.try_send(());
            }
            Msg::Flush(ack) => {
                target.flush();
                let _ = ack.try_send(());
            }
        }
    }
}

// ---- The process logger and the global subscriber ----

static PROCESS: OnceLock<Logger> = OnceLock::new();

/// The process's logger, once `prepare_process` installed it.
pub(crate) fn process_logger() -> Option<Logger> {
    PROCESS.get().cloned()
}

/// Starts the process queue on stderr and installs the global subscriber (once).
pub(crate) fn install_process() {
    if PROCESS.set(Logger::start(LogTarget::Stderr)).is_err() {
        return;
    }
    let subscriber = tracing_subscriber::fmt()
        .with_writer(QueueWriter(None))
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// The subscriber's writer: one [`LineBuf`] per event, into the given logger or the
/// process's.
pub(crate) struct QueueWriter(pub(crate) Option<Logger>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for QueueWriter {
    type Writer = LineBuf;

    fn make_writer(&'a self) -> LineBuf {
        LineBuf {
            buf: Vec::new(),
            to: self.0.clone().or_else(process_logger),
        }
    }
}

/// Collects one event's formatted bytes; on drop, redacts the whole line and pushes it.
pub(crate) struct LineBuf {
    buf: Vec<u8>,
    to: Option<Logger>,
}

impl Write for LineBuf {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LineBuf {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let line = diag(&String::from_utf8_lossy(&self.buf));
        if let Some(l) = &self.to {
            l.push(line);
        }
    }
}
