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
use tempfile::TempDir;

/// Every bounded wait of these tests.
pub const WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(20);

/// A fresh state directory path (absent) under a private temp directory.
pub struct Dir {
    pub tmp: TempDir,
    pub path: PathBuf,
}

/// The parent of every test directory: the OS temp directory on macOS; on Windows
/// `FOLDERID_LocalAppData` (not `%TEMP%`, which can be redirected), asserted private.
#[must_use]
pub fn temp_parent() -> TempDir {
    #[cfg(windows)]
    {
        use chaldea_pty_runtime::host::{self, EntryKind, Privacy};
        let tmp = tempfile::tempdir_in(host::default_state_base().unwrap()).unwrap();
        assert_eq!(
            host::privacy(tmp.path(), EntryKind::Directory).unwrap(),
            Privacy::Private,
            "{}",
            tmp.path().display()
        );
        tmp
    }
    #[cfg(not(windows))]
    tempfile::tempdir().unwrap()
}

#[must_use]
pub fn state() -> Dir {
    let tmp = temp_parent();
    let path = tmp.path().join("state");
    Dir { tmp, path }
}

/// A port nothing listens on right now (bound to `127.0.0.1:0`, then released).
#[must_use]
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
    status: Option<ExitStatus>,
}

fn collect(
    r: impl Read + Send + 'static,
    into: Arc<Mutex<String>>,
    lines: Option<mpsc::Sender<String>>,
) {
    std::thread::spawn(move || {
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

impl Proc {
    pub fn spawn(cmd: &mut Command) -> Proc {
        let mut child = spawn_locked(cmd).unwrap();
        let (tx, rx) = mpsc::channel();
        let stderr = Arc::new(Mutex::new(String::new()));
        let stdout = Arc::new(Mutex::new(String::new()));
        collect(child.stderr.take().unwrap(), stderr.clone(), Some(tx));
        collect(child.stdout.take().unwrap(), stdout.clone(), None);
        Proc {
            child,
            stderr_lines: rx,
            stderr,
            stdout,
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

    /// Waits (bounded) for the exit; kills it and panics past the deadline.
    pub fn wait(&mut self) -> ExitStatus {
        if let Some(s) = self.status {
            return s;
        }
        let end = Instant::now() + WAIT;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                self.status = Some(s);
                // Let the collectors drain the closed pipes.
                let drained = Instant::now() + Duration::from_secs(2);
                while Instant::now() < drained && !self.stderr_lines_closed() {
                    std::thread::sleep(POLL);
                }
                return s;
            }
            if Instant::now() >= end {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("the process did not exit; stderr:\n{}", self.stderr());
            }
            std::thread::sleep(POLL);
        }
    }

    fn stderr_lines_closed(&self) -> bool {
        loop {
            match self.stderr_lines.try_recv() {
                Ok(_) => {}
                Err(mpsc::TryRecvError::Empty) => return false,
                Err(mpsc::TryRecvError::Disconnected) => return true,
            }
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
            let _ = self.child.kill();
            let _ = self.child.wait();
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

/// The saved owner token (without its newline).
#[must_use]
pub fn token_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(names::OWNER_TOKEN))
        .unwrap()
        .trim_end()
        .to_owned()
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

/// Every entry of `dir` with its bytes (directories empty) and modification time.
#[must_use]
pub fn listing(dir: &Path) -> Vec<(String, Vec<u8>, std::time::SystemTime)> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let m = e.metadata().unwrap();
            let bytes = if m.is_file() {
                std::fs::read(e.path()).unwrap()
            } else {
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

/// Opens the daemon's database directly (the daemon must not be running).
#[must_use]
pub fn open_store(dir: &Path) -> chaldea_daemon::store::Store {
    use chaldea_daemon::store::{RandomIds, Store, StoreConfig, SystemClock};
    Store::open(StoreConfig {
        path: dir.join(names::DB),
        readers: 2,
        writer_queue: 16,
        busy_timeout: Duration::from_secs(5),
        clock: Arc::new(SystemClock),
        ids: Arc::new(RandomIds),
    })
    .unwrap()
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
/// command), requires its success within `WAIT`, and returns its first report line.
pub fn run_helper(test: &str, env: &[(&str, &OsStr)], tweak: impl FnOnce(&mut Command)) -> String {
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
    let code = p.code();
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
