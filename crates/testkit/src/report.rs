//! The report stream between a FakeCli process and its test.
//!
//! Wire format: each FakeCli process connects its own TCP stream to the test's
//! [`ReportListener`], writes a hello line, then mode events, one JSON object per line. It
//! ends its side of the stream only as it exits: a killed process by the kernel, and a
//! normal exit by `shutdown(Write)` right before the process ends, after its last line
//! (Windows resets a socket still open at process exit, and the reset discards what the
//! test has not read yet). The test may send `ping` lines; the fixture answers each with
//! `{"event":"pong","inq":n}`, `inq` being the bytes waiting on its stdin when that is a
//! terminal. EOF is death (or an exit already under way), `pong` is life. Other lines go to
//! the mode's command handler, if it registered one (`spawn-tree --emit-output`: `emit`).
//!
//! The test side also has [`Cleanup`], which kills fixtures by verified identity.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::Child;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The process's report stream, once connected, and whether it has ended. It lives as
/// long as the process; [`end`] ends it.
static STREAM: OnceLock<Mutex<(TcpStream, bool)>> = OnceLock::new();

/// The mode's handler of the test's lines other than `ping`.
static COMMAND: OnceLock<fn(&str)> = OnceLock::new();

/// Routes the test's lines other than `ping` to `f`, on the stream's reader thread.
pub(crate) fn on_command(f: fn(&str)) {
    let _ = COMMAND.set(f);
}

/// The fixture side of the report stream.
pub(crate) struct Reporter;

impl Reporter {
    /// Connects to `addr` and starts the thread answering `ping`; with no address (or a
    /// failed connect) events are discarded. A process connects once.
    pub(crate) fn connect(addr: Option<&str>, inq: fn() -> Option<u64>) -> Reporter {
        let Some(stream) = addr.and_then(|a| TcpStream::connect(a).ok()) else {
            return Reporter;
        };
        let read = stream.try_clone();
        let _ = STREAM.set(Mutex::new((stream, false)));
        if let Ok(read) = read {
            std::thread::spawn(move || {
                for line in BufReader::new(read).lines() {
                    let Ok(line) = line else { return };
                    if line.trim() == "ping" {
                        // Through the event writer's lock, so a pong never lands inside an
                        // event line.
                        Reporter.send(&json!({"event": "pong", "inq": inq()}));
                    } else if let Some(f) = COMMAND.get() {
                        f(line.trim());
                    }
                }
            });
        }
        Reporter
    }

    /// Writes one line with one `write_all` under the stream's lock, which `pong` answers
    /// share, so lines never interleave; a process killed mid-write can still leave a partial
    /// last line, which the test's reader discards. Nothing once the stream has ended.
    #[allow(
        clippy::unused_self,
        reason = "a line is sent by the process that connected"
    )]
    pub(crate) fn send(&self, v: &Value) {
        if let Some(s) = STREAM.get() {
            let mut s = s.lock().unwrap_or_else(PoisonError::into_inner);
            if !s.1 {
                let _ = s.0.write_all(format!("{v}\n").as_bytes());
            }
        }
    }
}

/// Ends the report stream right before a normal exit: no line is written after it, and
/// the test reads every line written, then EOF. Without it the socket would still be open
/// at process exit, which Windows resets, discarding the lines the test has not read.
pub(crate) fn end() {
    if let Some(s) = STREAM.get() {
        let mut s = s.lock().unwrap_or_else(PoisonError::into_inner);
        s.1 = true;
        let _ = s.0.shutdown(Shutdown::Write);
    }
}

/// The hello a fixture sends first. Unix-only fields are `None` elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub mode: String,
    pub role: String,
    pub pid: u32,
    pub ppid: Option<u32>,
    pub pgid: Option<u32>,
    pub sid: Option<u32>,
    pub raw: bool,
    /// `ttyname` of fds 0–2.
    pub ttys: Option<Vec<Option<String>>>,
    /// Whether `/dev/tty` opens.
    pub ctty: Option<bool>,
    /// Whether its group is the terminal's foreground group.
    pub fg: Option<bool>,
    /// Its open fds, listed before the report stream connected.
    pub fds: Option<Vec<i32>>,
}

fn u32_of(v: &Value) -> Option<u32> {
    v.as_u64().and_then(|n| u32::try_from(n).ok())
}

impl Hello {
    fn parse(v: &Value) -> Option<Hello> {
        if v["event"] != "hello" {
            return None;
        }
        Some(Hello {
            mode: v["mode"].as_str()?.to_owned(),
            role: v["role"].as_str()?.to_owned(),
            pid: u32_of(&v["pid"])?,
            ppid: u32_of(&v["ppid"]),
            pgid: u32_of(&v["pgid"]),
            sid: u32_of(&v["sid"]),
            raw: v["raw"].as_bool().unwrap_or(false),
            ttys: v["ttys"]
                .as_array()
                .map(|a| a.iter().map(|t| t.as_str().map(str::to_owned)).collect()),
            ctty: v["ctty"].as_bool(),
            fg: v["fg"].as_bool(),
            fds: v["fds"].as_array().map(|a| {
                a.iter()
                    .filter_map(|f| f.as_i64().and_then(|n| i32::try_from(n).ok()))
                    .collect()
            }),
        })
    }
}

/// A `pong`: the fixture was alive when it answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pong {
    pub inq: Option<u64>,
}

/// The test side: a loopback listener every fixture of a test connects to.
pub struct ReportListener {
    listener: TcpListener,
}

impl ReportListener {
    /// # Errors
    ///
    /// The loopback listener cannot be bound.
    pub fn bind() -> io::Result<ReportListener> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        Ok(ReportListener { listener })
    }

    /// The value for `--report`.
    ///
    /// # Panics
    ///
    /// The bound listener has no local address.
    #[must_use]
    pub fn arg(&self) -> String {
        self.listener
            .local_addr()
            .expect("bound listener")
            .to_string()
    }

    /// The next fixture to connect and send its hello.
    ///
    /// # Errors
    ///
    /// `TimedOut` at `deadline`; `InvalidData` when the first line is not a hello.
    pub fn accept(&self, deadline: Instant) -> io::Result<Fixture> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    let mut f = Fixture::new(stream)?;
                    let v = f
                        .read_value(deadline)?
                        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no hello"))?;
                    f.hello = Hello::parse(&v)
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, v.to_string()))?;
                    return Ok(f);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
            if Instant::now() >= deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Exactly the fixtures of `roles`, keyed by role.
    ///
    /// # Errors
    ///
    /// A role outside `roles` or seen twice, or the deadline before all arrived.
    pub fn expect_roles(
        &self,
        roles: &[&str],
        deadline: Instant,
    ) -> Result<BTreeMap<String, Fixture>, String> {
        let mut got = BTreeMap::new();
        while got.len() < roles.len() {
            let f = self
                .accept(deadline)
                .map_err(|e| format!("waiting for {roles:?}, have {:?}: {e}", got.keys()))?;
            let role = f.hello.role.clone();
            if !roles.contains(&role.as_str()) {
                return Err(format!("unexpected role {role}"));
            }
            if got.insert(role.clone(), f).is_some() {
                return Err(format!("role {role} twice"));
            }
        }
        Ok(got)
    }
}

/// One fixture's stream.
pub struct Fixture {
    pub hello: Hello,
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    line: Vec<u8>,
    pending: VecDeque<Value>,
    closed: bool,
}

impl Fixture {
    fn new(stream: TcpStream) -> io::Result<Fixture> {
        Ok(Fixture {
            hello: Hello {
                mode: String::new(),
                role: String::new(),
                pid: 0,
                ppid: None,
                pgid: None,
                sid: None,
                raw: false,
                ttys: None,
                ctty: None,
                fg: None,
                fds: None,
            },
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
            line: Vec::new(),
            pending: VecDeque::new(),
            closed: false,
        })
    }

    /// The next line as JSON; `None` at EOF; `TimedOut` at `deadline`.
    fn read_value(&mut self, deadline: Instant) -> io::Result<Option<Value>> {
        if self.closed {
            return Ok(None);
        }
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            self.reader.get_ref().set_read_timeout(Some(left))?;
            // Bytes read before a timeout stay in `line` for the next call.
            match self.reader.read_until(b'\n', &mut self.line) {
                Ok(0) => {
                    self.closed = true;
                    return Ok(None);
                }
                Ok(_) if self.line.ends_with(b"\n") => {
                    let v = serde_json::from_slice(&self.line)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    self.line.clear();
                    return Ok(Some(v));
                }
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {
                    self.closed = true;
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The next event other than `pong`; `None` once the stream closed.
    ///
    /// # Errors
    ///
    /// `TimedOut` at `deadline`, or a line that is not JSON.
    pub fn next_event(&mut self, deadline: Instant) -> io::Result<Option<Value>> {
        if let Some(v) = self.pending.pop_front() {
            return Ok(Some(v));
        }
        loop {
            match self.read_value(deadline)? {
                Some(v) if v["event"] == "pong" => {}
                other => return Ok(other),
            }
        }
    }

    /// Whether the stream closed (the process exited, or its normal exit is under way) by
    /// `deadline`. Events read meanwhile stay queued for `next_event`.
    pub fn closed_by(&mut self, deadline: Instant) -> bool {
        loop {
            match self.read_value(deadline) {
                Ok(None) => return true,
                Ok(Some(v)) => {
                    if v["event"] != "pong" {
                        self.pending.push_back(v);
                    }
                }
                Err(_) => return false,
            }
        }
    }

    /// Sends one command line to the fixture (`emit` for `spawn-tree --emit-output`).
    ///
    /// # Errors
    ///
    /// The stream is closed or the write failed.
    pub fn send(&mut self, line: &str) -> io::Result<()> {
        if self.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        writeln!(self.writer, "{line}")
    }

    /// Sends `ping` and waits for the `pong`; `None` when the stream closed or the deadline
    /// passed first.
    pub fn ping(&mut self, deadline: Instant) -> Option<Pong> {
        if self.closed || self.writer.write_all(b"ping\n").is_err() {
            return None;
        }
        loop {
            match self.read_value(deadline) {
                Ok(Some(v)) if v["event"] == "pong" => {
                    return Some(Pong {
                        inq: v["inq"].as_u64(),
                    });
                }
                Ok(Some(v)) => self.pending.push_back(v),
                Ok(None) | Err(_) => return None,
            }
        }
    }
}

/// The start stamp of a live process by pid, in the backend's identity format; `None`
/// when no live process has that pid.
pub type Identify = fn(u32) -> Option<String>;

/// Kills a process by pid and recorded start, verifying the start on the handle it kills
/// through (Windows: `fake::kill_verified`); true when it was killed.
pub type KillVerified = fn(u32, &str) -> bool;

/// Kills every registered fixture still alive with its recorded start, and every owned
/// child, on drop (an assertion failure included).
pub struct Cleanup {
    identify: Option<Identify>,
    kill: Option<KillVerified>,
    ids: Vec<(u32, String)>,
    children: Vec<Child>,
}

impl Cleanup {
    /// `identify` verifies identities before a signal; without it only owned children are
    /// killed.
    #[must_use]
    pub fn new(identify: Option<Identify>) -> Cleanup {
        Cleanup {
            identify,
            kill: None,
            ids: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Kills each tracked identity with `kill` on drop, instead of the Unix signal after an
    /// `identify` check.
    #[must_use]
    pub fn kill_with(mut self, kill: KillVerified) -> Cleanup {
        self.kill = Some(kill);
        self
    }

    /// Records `pid` with its current start.
    pub fn track(&mut self, pid: u32) {
        if let Some(start) = self.identify.and_then(|f| f(pid)) {
            self.ids.push((pid, start));
        }
    }

    /// Takes a child the test started.
    pub fn own(&mut self, child: Child) {
        self.children.push(child);
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(kill) = self.kill {
            for (pid, start) in &self.ids {
                let _ = kill(*pid, start);
            }
        }
        #[cfg(unix)]
        for (pid, start) in self.ids.iter().filter(|_| self.kill.is_none()) {
            if self.identify.and_then(|f| f(*pid)).as_ref() == Some(start)
                && let Some(p) = i32::try_from(*pid)
                    .ok()
                    .and_then(rustix::process::Pid::from_raw)
            {
                let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
            }
        }
        for c in &mut self.children {
            let _ = c.kill();
            let end = Instant::now() + Duration::from_secs(10);
            while matches!(c.try_wait(), Ok(None)) && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
