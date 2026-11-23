//! Shared helpers of the `chaldea` binary tests: the binary started through `spawn_locked`,
//! its stderr captured, every wait bounded, and a stop by signal (Unix) or by the instance's
//! stop event (Windows).

#![allow(
    dead_code,
    clippy::missing_panics_doc,
    reason = "each test file uses part of these helpers"
)]

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chaldea_daemon::lifecycle::{
    Discovery, DiscoveryRecord, LockProbe, STATE_DIR_ENV, discover, names, probe, spawn_locked,
};

pub mod faults;
#[path = "../../../daemon/tests/support/lifecycle.rs"]
mod lifecycle;
pub mod runner;
#[allow(unused_imports, reason = "each test file uses part of these helpers")]
pub use lifecycle::{kill_and_reap, open_store, token_of};

/// Every bounded wait of these tests.
pub const WAIT: Duration = Duration::from_secs(30);

/// Waits for a write's reply for at most [`WAIT`] (the writer may be parked by a fault) and
/// returns the job's value.
pub fn settle<R: Send + 'static>(ticket: chaldea_daemon::store::WriteTicket<R>) -> R {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(ticket.blocking_wait().map_err(|e| e.to_string()));
    });
    rx.recv_timeout(WAIT)
        .expect("the write's reply did not arrive in time")
        .expect("the write failed")
}
const POLL: Duration = Duration::from_millis(20);

/// A fresh state directory path (absent) under a private temp directory.
pub struct Dir {
    pub tmp: TempParent,
    pub path: PathBuf,
}

/// A test's private directory, removed with its contents when dropped.
pub struct TempParent(PathBuf);

impl TempParent {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempParent {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The parent of every test directory: on macOS under the OS temp directory; on Windows under
/// `FOLDERID_LocalAppData` (not `%TEMP%`, which can be redirected). Created with
/// `host::create_private_dir`, as the daemon creates its state directory, so its owner is this
/// account whatever owner the process's token gives new objects (an elevated administrator's
/// token gives them `Administrators`); asserted private.
#[must_use]
pub fn temp_parent() -> TempParent {
    use chaldea_pty_runtime::host::{self, EntryKind, Privacy};
    #[cfg(windows)]
    let base = host::default_state_base().unwrap();
    #[cfg(not(windows))]
    let base = std::env::temp_dir();
    let path = base.join(format!(".chaldea-test-{}", uuid::Uuid::new_v4().simple()));
    host::create_private_dir(&path).unwrap();
    let tmp = TempParent(path);
    assert_eq!(
        host::privacy(tmp.path(), EntryKind::Directory).unwrap(),
        Privacy::Private,
        "{}",
        tmp.path().display()
    );
    tmp
}

#[must_use]
pub fn state() -> Dir {
    let tmp = temp_parent();
    let path = tmp.path().join("state");
    Dir { tmp, path }
}

/// `chaldea <args>` with `CHALDEA_STATE_DIR` removed unless `env` sets it.
#[must_use]
pub fn chaldea(args: &[&str], env: &[(&str, &OsStr)]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_chaldea"));
    c.args(args)
        .env_remove(STATE_DIR_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    c
}

/// `chaldea --state-dir <dir> daemon run [--port <p>]`.
#[must_use]
pub fn run_cmd(dir: &Path, port: Option<u16>) -> Command {
    let mut c = chaldea(&[], &[]);
    c.arg("--state-dir").arg(dir).args(["daemon", "run"]);
    if let Some(p) = port {
        c.args(["--port", &p.to_string()]);
    }
    c
}

/// A started `chaldea` process with its output collected by threads.
pub struct Proc {
    pub child: Child,
    stderr_lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    stdout: Arc<Mutex<String>>,
    /// Collector threads that saw their pipe close.
    collected: Arc<std::sync::atomic::AtomicUsize>,
    status: Option<ExitStatus>,
}

fn collect(
    r: impl Read + Send + 'static,
    into: Arc<Mutex<String>>,
    lines: Option<mpsc::Sender<String>>,
    done: Arc<std::sync::atomic::AtomicUsize>,
) {
    std::thread::spawn(move || {
        let _done = Done(done);
        for line in BufReader::new(r).lines() {
            let Ok(line) = line else { break };
            {
                let mut s = into.lock().unwrap();
                s.push_str(&line);
                s.push('\n');
            }
            if let Some(tx) = &lines {
                let _ = tx.send(line);
            }
        }
    });
}

/// Counts a collector as finished when its thread ends (also by a panic).
struct Done(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for Done {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Proc {
    pub fn spawn(cmd: &mut Command) -> Proc {
        let mut child = spawn_locked(cmd).unwrap();
        let (tx, rx) = mpsc::channel();
        let stderr = Arc::new(Mutex::new(String::new()));
        let stdout = Arc::new(Mutex::new(String::new()));
        let collected = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        collect(
            child.stderr.take().unwrap(),
            stderr.clone(),
            Some(tx),
            collected.clone(),
        );
        collect(
            child.stdout.take().unwrap(),
            stdout.clone(),
            None,
            collected.clone(),
        );
        Proc {
            child,
            stderr_lines: rx,
            stderr,
            stdout,
            collected,
            status: None,
        }
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    #[must_use]
    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    #[must_use]
    pub fn stdout(&self) -> String {
        self.stdout.lock().unwrap().clone()
    }

    /// Waits (bounded) for the `listening on http://127.0.0.1:<port>` line and returns the
    /// port.
    pub fn serving(&self) -> u16 {
        const PREFIX: &str = "listening on http://127.0.0.1:";
        let end = Instant::now() + WAIT;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.stderr_lines.recv_timeout(left) {
                Ok(line) => {
                    if let Some(i) = line.find(PREFIX) {
                        return line[i + PREFIX.len()..].trim().parse().unwrap();
                    }
                }
                Err(e) => panic!("no listening line ({e}); stderr:\n{}", self.stderr()),
            }
        }
    }

    /// Waits (bounded by `WAIT`) for the exit; kills it and panics past the deadline.
    pub fn wait(&mut self) -> ExitStatus {
        self.wait_within(WAIT)
    }

    /// Waits for the exit within `within`, then for both output collectors to finish (10 s);
    /// kills it and panics past the deadline, with what it printed so far.
    pub fn wait_within(&mut self, within: Duration) -> ExitStatus {
        if let Some(s) = self.status {
            return s;
        }
        let end = Instant::now() + within;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                self.status = Some(s);
                let drained = Instant::now() + Duration::from_secs(10);
                while Instant::now() < drained
                    && self.collected.load(std::sync::atomic::Ordering::SeqCst) < 2
                {
                    std::thread::sleep(POLL);
                }
                return s;
            }
            if Instant::now() >= end {
                kill_and_reap(&mut self.child);
                panic!(
                    "pid {} did not exit within {within:?}; stdout:\n{}\nstderr:\n{}",
                    self.pid(),
                    self.stdout(),
                    self.stderr()
                );
            }
            std::thread::sleep(POLL);
        }
    }

    /// The exit code.
    pub fn code(&mut self) -> i32 {
        self.wait().code().expect("exited by a signal")
    }

    /// A graceful stop: `SIGTERM` (Unix) or the instance's stop event (Windows).
    pub fn stop(&mut self, dir: &Path) {
        #[cfg(unix)]
        {
            let _ = dir;
            term(self.pid(), rustix::process::Signal::TERM);
        }
        #[cfg(windows)]
        {
            let r = serving_record(dir);
            let name = chaldea_pty_runtime::host::stop_event_name(&r.instance);
            chaldea_pty_runtime::fake::set_stop_event(&name).unwrap();
        }
    }

    /// Stops and returns the exit code.
    pub fn stop_and_wait(&mut self, dir: &Path) -> i32 {
        self.stop(dir);
        self.code()
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if self.status.is_none() && matches!(self.child.try_wait(), Ok(None)) {
            kill_and_reap(&mut self.child);
        }
    }
}

/// Sends `sig` to `pid` (a process this test started).
#[cfg(unix)]
pub fn term(pid: u32, sig: rustix::process::Signal) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    rustix::process::kill_process(pid, sig).unwrap();
}

/// Runs `cmd` to its (bounded) exit and returns the code, stdout, and stderr.
pub fn run_to_exit(cmd: &mut Command) -> (i32, String, String) {
    let mut p = Proc::spawn(cmd);
    let code = p.code();
    (code, p.stdout(), p.stderr())
}

/// The `serving` record (the lock held, the identity alive).
#[must_use]
pub fn serving_record(dir: &Path) -> DiscoveryRecord {
    match discover(dir) {
        Discovery::Serving(r) => r,
        other => panic!("not serving: {other:?}"),
    }
}

/// Polls `cond` until it holds or `WAIT` passes.
pub fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + WAIT;
    while Instant::now() < end {
        if cond() {
            return true;
        }
        std::thread::sleep(POLL);
    }
    cond()
}

/// The lock probe of `dir`.
#[must_use]
pub fn lock_state(dir: &Path) -> LockProbe {
    probe(dir).unwrap()
}

/// `GET <path>` on `127.0.0.1:<port>` with `token` (or none); the status code.
#[must_use]
pub fn get_status(port: u16, path: &str, token: Option<&str>) -> u16 {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    let auth = token.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut head = [0u8; 12];
    s.read_exact(&mut head).unwrap();
    std::str::from_utf8(&head[9..12]).unwrap().parse().unwrap()
}

/// One request on a new connection, read through EOF (the server closes it): the status and
/// the body, whose length matches its `Content-Length`. A body is sent as JSON.
pub fn request_complete(
    port: u16,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&str>,
) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    let content = body.map_or_else(String::new, |b| {
        format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        )
    });
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n{content}Connection: close\r\n\r\n{}",
        body.unwrap_or("")
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8(raw).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").expect("a complete header");
    let status = head[9..12].parse().unwrap();
    let length = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap())
        })
        .expect("a Content-Length");
    assert_eq!(body.len(), length, "{text}");
    (status, body.to_owned())
}

/// Every entry of `dir` with its bytes and modification time. Directories and `daemon.lock`
/// carry no bytes: the lock file is data-less, and Windows refuses reading a byte range
/// another process locked.
#[must_use]
pub fn listing(dir: &Path) -> Vec<(String, Vec<u8>, std::time::SystemTime)> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let m = e.metadata().unwrap();
            let bytes = if m.is_file() && e.file_name() != names::LOCK {
                std::fs::read(e.path()).unwrap()
            } else {
                assert_eq!(m.len(), 0, "{:?} has content", e.file_name());
                Vec::new()
            };
            (
                e.file_name().to_string_lossy().into_owned(),
                bytes,
                m.modified().unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

/// A `chd_` secret of the token form that no store issued.
#[must_use]
pub fn unknown_token() -> String {
    format!("chd_{}", "A".repeat(43))
}

/// The marker of a Chaldea state directory (design §2.4), restated.
pub const MARKER: &[u8] = b"{\"chaldea_state_dir\":1}\n";

/// Creates `dir` private with only a complete marker (no database yet).
pub fn marked_dir(dir: &Path) {
    use chaldea_pty_runtime::host;
    host::create_private_dir(dir).unwrap();
    write_private(&dir.join(names::MARKER), MARKER);
}

/// Creates `path` owner-only with `bytes`.
pub fn write_private(path: &Path, bytes: &[u8]) {
    let mut f = chaldea_pty_runtime::host::create_private_file(path).unwrap();
    f.write_all(bytes).unwrap();
    f.sync_all().unwrap();
}

/// Shuts the store down and asserts it closed.
pub fn close_store(store: &chaldea_daemon::store::Store) {
    let r = store.shutdown(Instant::now() + WAIT);
    assert!(r.writer_exited && r.readers_returned, "{r:?}");
}

/// The prefix of a helper's report lines (found anywhere in a line: libtest prints
/// `test <name> ... ` first).
const MARK: &str = "@@ ";

/// Prints one report line from a helper.
pub fn report(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{MARK}{line}");
    let _ = out.flush();
}

/// Runs `test` of this test binary as a helper process with `env` (and `tweak` applied to the
/// command), requires its success within `within`, and returns its first report line.
pub fn run_helper(
    test: &str,
    env: &[(&str, &OsStr)],
    within: Duration,
    tweak: impl FnOnce(&mut Command),
) -> String {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([test, "--exact", "--nocapture", "--test-threads=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    tweak(&mut cmd);
    let mut p = Proc::spawn(&mut cmd);
    let code = p.wait_within(within).code().expect("exited by a signal");
    let out = p.stdout();
    assert_eq!(
        code,
        0,
        "helper {test} failed\nstdout:\n{out}\nstderr:\n{}",
        p.stderr()
    );
    out.lines()
        .find_map(|l| l.find(MARK).map(|i| l[i + MARK.len()..].to_owned()))
        .unwrap_or_else(|| panic!("no report from helper {test}\nstdout:\n{out}"))
}
