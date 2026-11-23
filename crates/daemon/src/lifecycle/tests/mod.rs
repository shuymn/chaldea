//! Unit tests of the lifecycle (design §7): `run_isolated` and the lifecycle's parts in
//! process with fault points, and helper processes (this test binary re-executed) for
//! everything that touches process-wide state, leaks a lock, or leaves a task running.

#![allow(clippy::missing_panics_doc, reason = "tests")]

mod files;
mod gate;
mod helpers;
mod log;
mod shutdown;
mod startup;
mod state_dir;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use tempfile::TempDir;

use super::faults::Faults;
use super::gate::{ShutdownGate, SignalName, StopCause};
use super::log::TestLog;
use super::{
    Budgets, BuildCtx, Built, Components, DaemonConfig, DetachVia, ExitReport, LockProbe,
    ReadyWriter, Reconciler, RunMode, ShutdownParticipant, probe, receipts_reconciler,
    run_isolated, spawn_locked,
};
use crate::http::{HttpServer, Mounts};
use crate::store::Store;

/// Every bounded wait of these tests.
pub(super) const WAIT: Duration = Duration::from_secs(30);

/// Waits for a write's reply for at most [`WAIT`] (the writer may be parked by a fault).
pub(super) fn settle<R: Send + 'static>(
    ticket: crate::store::WriteTicket<R>,
) -> Result<R, crate::store::WriteError> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(ticket.blocking_wait());
    });
    rx.recv_timeout(WAIT)
        .expect("the write's reply did not arrive in time")
}

/// A fresh state directory path (absent) under a private temp directory.
pub(super) struct Dir {
    _tmp: TempDir,
    pub(super) path: PathBuf,
}

pub(super) fn state() -> Dir {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state");
    Dir { _tmp: tmp, path }
}

/// A port nothing listens on right now (bound to `127.0.0.1:0`, then released).
pub(super) fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Test components: the receipts reconciler unless told otherwise, extra reconcilers, test
/// mounts, participants, and a panicking `build`.
pub(super) type OnBuild = Box<dyn FnOnce(&BuildCtx) + Send>;

#[derive(Default)]
pub(super) struct TestComponents {
    pub(super) no_receipts: bool,
    pub(super) extra: Vec<Reconciler>,
    pub(super) mounts: Option<Mounts>,
    pub(super) participants: Vec<Box<dyn ShutdownParticipant>>,
    pub(super) panic_in_build: bool,
    pub(super) on_build: Option<OnBuild>,
    /// The command handlers; [`crate::http::CommandRegistry::mvp`] when absent.
    pub(super) commands: Option<crate::http::CommandRegistry>,
}

impl Components for TestComponents {
    fn reconcilers(&mut self, _store: &Store) -> Vec<Reconciler> {
        let mut v = Vec::new();
        if !self.no_receipts {
            v.push(receipts_reconciler());
        }
        v.append(&mut self.extra);
        v
    }

    fn build(&mut self, ctx: &BuildCtx) -> Built {
        assert!(!self.panic_in_build, "a component's build panicked");
        if let Some(f) = self.on_build.take() {
            f(ctx);
        }
        Built {
            mounts: self.mounts.take().unwrap_or_else(Mounts::mvp),
            commands: self
                .commands
                .take()
                .unwrap_or_else(crate::http::CommandRegistry::mvp),
            participants: std::mem::take(&mut self.participants),
        }
    }

    fn attach(&mut self, _server: &HttpServer) {}
}

pub(super) fn foreground(dir: &Path, port: Option<u16>) -> DaemonConfig {
    DaemonConfig {
        state_dir: dir.to_path_buf(),
        port,
        mode: RunMode::Foreground,
        budgets: Budgets::DEFAULT,
    }
}

/// One verdict line, with what `probe` saw on the lock and whether the record existed when
/// it was written.
#[derive(Debug, Clone)]
pub(super) struct Verdict {
    pub(super) line: String,
    pub(super) lock: Option<LockProbe>,
    pub(super) record_present: bool,
}

/// A readiness writer that records each line with the lock and record state at that moment.
#[derive(Clone)]
pub(super) struct RecordingReady {
    pub(super) dir: PathBuf,
    pub(super) lines: Arc<Mutex<Vec<Verdict>>>,
    pub(super) closed: Arc<Mutex<bool>>,
}

impl RecordingReady {
    pub(super) fn new(dir: &Path) -> RecordingReady {
        RecordingReady {
            dir: dir.to_path_buf(),
            lines: Arc::default(),
            closed: Arc::default(),
        }
    }

    pub(super) fn lines(&self) -> Vec<Verdict> {
        self.lines.lock().unwrap().clone()
    }
}

impl ReadyWriter for RecordingReady {
    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let lock = probe(&self.dir).ok();
        let record_present = self.dir.join(super::names::RECORD).exists();
        self.lines.lock().unwrap().push(Verdict {
            line: line.to_owned(),
            lock,
            record_present,
        });
        Ok(())
    }

    fn close(self: Box<Self>) {
        *self.closed.lock().unwrap() = true;
    }
}

pub(super) fn detached(dir: &Path, port: Option<u16>, ready: &RecordingReady) -> DaemonConfig {
    let r = ready.clone();
    DaemonConfig {
        state_dir: dir.to_path_buf(),
        port,
        mode: RunMode::DetachedChild {
            ready: Box::new(move || Ok(Box::new(r) as Box<dyn ReadyWriter>)),
            via: DetachVia::Setsid,
        },
        budgets: Budgets::DEFAULT,
    }
}

/// One `run_isolated` on its own thread.
pub(super) struct Running {
    handle: Option<JoinHandle<ExitReport>>,
    pub(super) faults: Arc<Faults>,
    pub(super) log: TestLog,
}

pub(super) fn start(cfg: DaemonConfig, comps: TestComponents, faults: Arc<Faults>) -> Running {
    let log = TestLog::default();
    let (l, f) = (log.clone(), faults.clone());
    let handle = std::thread::spawn(move || run_isolated(cfg, Box::new(comps), l, f));
    Running {
        handle: Some(handle),
        faults,
        log,
    }
}

/// Polls `f` every 10 ms until it holds or `within` passes.
pub(super) fn wait_for(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + within;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl Running {
    pub(super) fn gate(&self) -> Arc<ShutdownGate> {
        assert!(
            wait_for(WAIT, || self.faults.gate.get().is_some()),
            "no gate published"
        );
        self.faults.gate.get().unwrap().clone()
    }

    /// Waits until the run logged that it serves; returns its port.
    pub(super) fn serving(&self) -> u16 {
        assert!(
            wait_for(WAIT, || self.log.text().contains("listening on")
                || self.is_finished()),
            "not serving: {}",
            self.log.text()
        );
        assert!(!self.is_finished(), "ended: {}", self.log.text());
        *self.faults.port.get().unwrap()
    }

    pub(super) fn store(&self) -> Store {
        assert!(wait_for(WAIT, || self.faults.store.get().is_some()));
        self.faults.store.get().unwrap().clone()
    }

    pub(super) fn stop(&self) -> bool {
        self.gate().request(StopCause::Signal(SignalName::Term))
    }

    pub(super) fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Joins the run, failing if it has not returned within `within`.
    pub(super) fn join_within(mut self, within: Duration) -> ExitReport {
        assert!(
            wait_for(within, || self.is_finished()),
            "the run did not return within {within:?}: {}",
            self.log.text()
        );
        self.handle.take().unwrap().join().unwrap()
    }

    pub(super) fn join(self) -> ExitReport {
        self.join_within(WAIT)
    }
}

/// Each participant's name, start, and end.
pub(super) type Seen = Arc<Mutex<Vec<(&'static str, Instant, Instant)>>>;

/// A recording participant.
pub(super) struct Recorder {
    pub(super) name: &'static str,
    pub(super) seen: Seen,
}

impl ShutdownParticipant for Recorder {
    fn name(&self) -> &'static str {
        self.name
    }

    fn shutdown(&self, end: Instant) -> BoxFuture<'_, ()> {
        self.seen
            .lock()
            .unwrap()
            .push((self.name, Instant::now(), end));
        Box::pin(async {})
    }
}

/// One HTTP/1.1 request on a new connection; the status code and the whole reply.
pub(super) fn request(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> std::io::Result<(u16, String)> {
    let mut s = TcpStream::connect_timeout(
        &SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into(),
        Duration::from_secs(5),
    )?;
    s.set_read_timeout(Some(Duration::from_secs(20)))?;
    let auth = token.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
    let body_head = body.map_or_else(
        || "Content-Length: 0\r\n".to_owned(),
        |b| {
            format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                b.len()
            )
        },
    );
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}{body_head}Connection: close\r\n\r\n{}",
        body.unwrap_or("")
    )?;
    let mut buf = String::new();
    s.read_to_string(&mut buf)?;
    let code = buf
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("no status in {buf:?}")))?;
    Ok((code, buf))
}

pub(super) fn get_status(port: u16, path: &str, token: Option<&str>) -> std::io::Result<u16> {
    request(port, "GET", path, token, None).map(|(c, _)| c)
}

pub(super) fn token_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(super::names::OWNER_TOKEN))
        .unwrap()
        .trim_end()
        .to_owned()
}

// ---- Helper processes ----

/// Selects the helper body in a re-executed test binary.
pub(super) const HELPER_ENV: &str = "CHALDEA_LIFECYCLE_HELPER";
/// The state directory a helper uses.
pub(super) const HELPER_DIR: &str = "CHALDEA_LIFECYCLE_HELPER_DIR";
/// The prefix of a helper's report lines.
pub(super) const MARK: &str = "@@ ";

pub(super) fn in_helper() -> bool {
    std::env::var_os(HELPER_ENV).is_some()
}

pub(super) fn helper_dir() -> PathBuf {
    PathBuf::from(std::env::var_os(HELPER_DIR).expect("helper dir"))
}

/// Prints one report line (stdout, flushed).
pub(super) fn report(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{MARK}{line}");
    let _ = out.flush();
}

/// The helper's exit path: like the CLI's `dispatch`, flush and `std::process::exit`.
pub(super) fn helper_exit(r: &ExitReport, pause: Duration) -> ! {
    report(&format!(
        "returned code={} reason={} lock={:?}",
        r.code.code(),
        r.reason.map_or("none", super::FailReason::as_str),
        r.lock
    ));
    std::thread::sleep(pause);
    super::flush_process_log(Duration::from_millis(100));
    std::process::exit(r.code.code());
}

/// Kills `child` and reaps it within 10 s (a process that cannot be reaped is reported, not
/// waited for).
pub(super) fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("pid {} was not reaped within 10 s of its kill", child.id());
}

/// A helper process: this test binary re-executed with `--exact <test>`.
pub(super) struct Helper {
    child: Child,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    /// The stderr collector reached the end of the stream.
    stderr_done: Arc<std::sync::atomic::AtomicBool>,
    pub(super) seen: Vec<String>,
}

impl Helper {
    pub(super) fn spawn(test: &str, dir: &Path, env: &[(&str, &str)]) -> Helper {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env(HELPER_ENV, "1")
            .env(HELPER_DIR, dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = spawn_locked(&mut cmd).unwrap();
        let (tx, rx) = mpsc::channel();
        let out = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                // libtest prints `test <name> ... ` without a newline before the test runs.
                if let Some(i) = line.find(MARK)
                    && tx.send(line[i + MARK.len()..].to_owned()).is_err()
                {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let err = child.stderr.take().unwrap();
        let sink = stderr.clone();
        let stderr_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = stderr_done.clone();
        std::thread::spawn(move || {
            let mut buf = String::new();
            let mut r = BufReader::new(err);
            while r.read_line(&mut buf).unwrap_or(0) > 0 {
                sink.lock().unwrap().push_str(&buf);
                buf.clear();
            }
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        Helper {
            child,
            lines: rx,
            stderr,
            stderr_done,
            seen: Vec::new(),
        }
    }

    pub(super) fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// The next report line starting with `prefix`, within `within`; the helper is killed on
    /// timeout.
    pub(super) fn expect(&mut self, prefix: &str, within: Duration) -> String {
        let end = Instant::now() + within;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(l) => {
                    self.seen.push(l.clone());
                    if l.starts_with(prefix) {
                        return l;
                    }
                }
                Err(e) => {
                    kill_and_reap(&mut self.child);
                    panic!(
                        "no `{prefix}` from the helper ({e}); seen {:?}; stderr:\n{}",
                        self.seen,
                        self.stderr()
                    );
                }
            }
        }
    }

    /// Waits for the helper's exit within `within` (killed otherwise), then for its stderr to
    /// be collected (10 s); its exit code.
    pub(super) fn wait(&mut self, within: Duration) -> Option<i32> {
        let end = Instant::now() + within;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                let collected = wait_for(Duration::from_secs(10), || {
                    self.stderr_done.load(std::sync::atomic::Ordering::SeqCst)
                });
                assert!(collected, "the helper's stderr was not collected");
                return s.code();
            }
            if Instant::now() >= end {
                kill_and_reap(&mut self.child);
                panic!("the helper did not exit; stderr:\n{}", self.stderr());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            kill_and_reap(&mut self.child);
        }
    }
}

/// The value of `key=` in a report line.
pub(super) fn field<'a>(line: &'a str, key: &str) -> &'a str {
    line.split(' ')
        .find_map(|w| w.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .unwrap_or_else(|| panic!("no {key} in {line:?}"))
}
