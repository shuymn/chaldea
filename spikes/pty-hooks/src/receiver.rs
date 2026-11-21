//! Hook receiver (§4.3): std `TcpListener` + `httparse`, one request per
//! connection, 5 s absolute request deadline carried through read and
//! dispatch. The head is read byte by byte up to its boundary, so no body
//! byte is consumed before method/path, Host, Bearer, and framing pass. The
//! response gets its own 500 ms write budget after the decision (a 408 sent
//! because the deadline passed included); after a rejection the receiver
//! half-closes and drains the unread request bytes (bounded) before closing,
//! so the client can read the status (on Windows, closing with unread data
//! resets the connection). Every step is clipped to a 7 s connection bound.
//! Fixtures are stored byte-for-byte under a per-launch directory with a
//! report-wide sequence; launch binding and witnesses live here too.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::util;

pub const MAX_HEAD: usize = 8 * 1024;
pub const MAX_BODY: usize = 4 * 1024 * 1024;
pub const MAX_HEADERS: usize = 32;
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(5);
/// Write budget for the response, from the decision.
pub const RESPONSE_BUDGET: Duration = Duration::from_millis(500);
/// Drain budget after a rejection response.
pub const DRAIN_TIME: Duration = Duration::from_secs(1);
/// Bytes drained beyond the declared length (or 4 MiB without one).
pub const DRAIN_SLACK: usize = 64 * 1024;
/// Absolute bound per connection, from accept: request + response + drain.
pub const CONNECTION_BOUND: Duration = Duration::from_secs(7);
pub const MAX_CONNS: usize = 16;

/// Report-wide fixture sequence (one report per process), shared by every receiver.
static SEQ: AtomicU64 = AtomicU64::new(0);
/// Report-wide connection sequence (`connections` log).
static CONN_SEQ: AtomicU64 = AtomicU64::new(0);

/// One connection as the receiver handled it (F-3c): what it decided, what
/// it wrote, and how the drain of unread request bytes ended.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ConnRecord {
    pub seq: u64,
    pub route: String,
    /// The status decided for the request (None: the peer closed before a head).
    pub decided: Option<u16>,
    pub bytes_written: usize,
    pub write_ok: Option<bool>,
    pub write_error: Option<String>,
    pub drained: usize,
    /// `peer_closed`, `deadline`, `byte_cap`, or `error: <kind>`.
    pub drain_stop: Option<String>,
    pub decided_ms: u64,
    pub total_ms: u64,
}

/// Union of the event names observed in Claude Code 2.1.322 and Codex 0.170.0 (§3).
pub const KNOWN_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "Stop",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Notification",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "Interrupt",
];

#[derive(Debug, Clone)]
pub enum BindRule {
    /// Claude: the predetermined `--session-id` (plus the launch's thread header).
    SessionId(String),
    /// Codex / FakeCli: thread id header + nearest CLI ancestor is the launched pid.
    Ancestor,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub token: String,
    pub thread_id: String,
    pub agent: String,
    pub cli_version: String,
    /// Unique per launch: fixtures go to `<fixtures_dir>/<agent>/<ver>/<launch_id>/`.
    pub launch_id: String,
    pub rule: BindRule,
    pub fixtures_dir: PathBuf,
    pub run_id: String,
    /// Normalized process names that count as "a CLI" in the emitter ancestry.
    pub cli_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventRecord {
    pub seq: u64,
    pub event: String,
    pub fixture: String,
    pub class: String,
    pub reason: Option<String>,
    pub session_id: Option<String>,
    pub received_ms: u64,
    pub thread_header: Option<String>,
    pub agent_header: Option<String>,
    pub ancestry: Vec<(u32, String)>,
    pub nearest_cli: Option<u32>,
    pub body_len: usize,
    #[serde(skip)]
    pub body: Vec<u8>,
}

impl EventRecord {
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
    pub fn is_bound(&self) -> bool {
        self.class == "bound"
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WitnessRecord {
    pub pid: u32,
    pub start_time: u64,
    pub role: String,
    pub session: bool,
    pub report: Value,
    pub received_ms: u64,
}

/// C-4 / C-5 probe routes, present only while a probe runs. Every handler
/// gets the request's absolute deadline.
pub trait ProbeHandler: Send + Sync {
    fn credential(&self) -> String;
    fn screen(&self, deadline: Instant) -> Vec<u8>;
    fn input(&self, body: &[u8], deadline: Instant) -> u16;
    fn key(&self, body: &[u8], deadline: Instant) -> u16;
    fn confirm(&self, body: &[u8], deadline: Instant) -> u16;
}

#[derive(Default)]
struct Inner {
    events: Vec<EventRecord>,
    witnesses: Vec<WitnessRecord>,
    rejections: BTreeMap<u16, u32>,
    bound_session: Option<String>,
    bound_at: Option<u64>,
    ambiguous: Vec<String>,
    connections: Vec<ConnRecord>,
}

struct State {
    cfg: Config,
    port: u16,
    t0: Instant,
    cli_pid: Mutex<Option<u32>>,
    cli_cv: Condvar,
    inner: Mutex<Inner>,
    cv: Condvar,
    active: AtomicUsize,
    probe: Mutex<Option<Arc<dyn ProbeHandler>>>,
    /// Largest number of bytes read past the head boundary before a rejection.
    overread: AtomicUsize,
    /// Called once when binding first becomes ambiguous (revokes the writer gate).
    on_ambiguous: Mutex<Option<AmbiguityHook>>,
}

pub struct Receiver {
    state: Arc<State>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

/// Read-only view on a receiver's witnesses (for probe handlers).
#[derive(Clone)]
pub struct WitnessView(Arc<State>);

impl WitnessView {
    pub fn witnesses(&self) -> Vec<WitnessRecord> {
        self.0
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .witnesses
            .clone()
    }
}

impl Receiver {
    pub fn start(cfg: Config) -> io::Result<Receiver> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let state = Arc::new(State {
            cfg,
            port,
            t0: Instant::now(),
            cli_pid: Mutex::new(None),
            cli_cv: Condvar::new(),
            inner: Mutex::new(Inner::default()),
            cv: Condvar::new(),
            active: AtomicUsize::new(0),
            probe: Mutex::new(None),
            overread: AtomicUsize::new(0),
            on_ambiguous: Mutex::new(None),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (st, sp) = (state.clone(), stop.clone());
        let accept = std::thread::Builder::new()
            .name("receiver-accept".into())
            .spawn(move || {
                while !sp.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            // Accepted sockets inherit O_NONBLOCK from the listener on BSDs;
                            // the per-read deadlines below need blocking reads with timeouts.
                            if stream.set_nonblocking(false).is_err() {
                                continue;
                            }
                            if st.active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
                                st.active.fetch_sub(1, Ordering::SeqCst);
                                st.reject(503);
                                let mut s = stream;
                                let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
                                let _ = respond(&mut s, 503, b"");
                                continue;
                            }
                            let st2 = st.clone();
                            let spawned = std::thread::Builder::new()
                                .name("receiver-conn".into())
                                .spawn(move || {
                                    st2.handle(stream);
                                    st2.active.fetch_sub(1, Ordering::SeqCst);
                                });
                            if spawned.is_err() {
                                st.active.fetch_sub(1, Ordering::SeqCst);
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(20)),
                    }
                }
            })?;
        Ok(Receiver {
            state,
            stop,
            accept: Some(accept),
        })
    }

    pub fn port(&self) -> u16 {
        self.state.port
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/hook", self.state.port)
    }

    pub fn config(&self) -> &Config {
        &self.state.cfg
    }

    pub fn witness_view(&self) -> WitnessView {
        WitnessView(self.state.clone())
    }

    pub fn set_cli_pid(&self, pid: u32) {
        *self.state.cli_pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(pid);
        self.state.cli_cv.notify_all();
    }

    /// Registers the hook run when binding becomes ambiguous; runs it at once
    /// if that already happened.
    pub fn on_ambiguous(&self, hook: AmbiguityHook) {
        let already = !self.lock().ambiguous.is_empty();
        if already {
            hook();
        }
        *self
            .state
            .on_ambiguous
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    pub fn set_probe(&self, p: Option<Arc<dyn ProbeHandler>>) {
        *self.state.probe.lock().unwrap_or_else(|e| e.into_inner()) = p;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.state.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait_until<T>(
        &self,
        deadline: Instant,
        mut f: impl FnMut(&Inner) -> Option<T>,
    ) -> Option<T> {
        let mut g = self.lock();
        loop {
            if let Some(v) = f(&g) {
                return Some(v);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            g = self
                .state
                .cv
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Readiness: the bound session id, unless a conflicting eligible
    /// SessionStart made the binding ambiguous (then never ready).
    fn ready(i: &Inner) -> Option<String> {
        if i.ambiguous.is_empty() {
            i.bound_session.clone()
        } else {
            None
        }
    }

    /// Waits for the readiness event (a bound, unambiguous SessionStart).
    pub fn wait_bound(&self, deadline: Instant) -> Option<String> {
        self.wait_until(deadline, |i| {
            if !i.ambiguous.is_empty() {
                // Ambiguity is final for this launch: stop waiting.
                return Some(None);
            }
            i.bound_session.clone().map(Some)
        })
        .flatten()
    }

    pub fn wait_witness(
        &self,
        deadline: Instant,
        pred: impl Fn(&WitnessRecord) -> bool,
    ) -> Option<WitnessRecord> {
        self.wait_until(deadline, |i| i.witnesses.iter().find(|w| pred(w)).cloned())
    }

    pub fn wait_witnesses(
        &self,
        deadline: Instant,
        pred: impl Fn(&[WitnessRecord]) -> bool,
    ) -> bool {
        self.wait_until(deadline, |i| pred(&i.witnesses).then_some(()))
            .is_some()
    }

    pub fn events(&self) -> Vec<EventRecord> {
        self.lock().events.clone()
    }

    /// The `n`-th (1-based) bound event named `event`, selected under the
    /// lock so only that record is cloned.
    pub fn nth_bound(&self, event: &str, n: usize) -> Option<EventRecord> {
        let g = self.lock();
        g.events
            .iter()
            .filter(|e| e.event == event && e.is_bound())
            .nth(n.checked_sub(1)?)
            .cloned()
    }

    pub fn witnesses(&self) -> Vec<WitnessRecord> {
        self.lock().witnesses.clone()
    }

    pub fn rejections(&self) -> BTreeMap<u16, u32> {
        self.lock().rejections.clone()
    }

    pub fn connections(&self) -> Vec<ConnRecord> {
        self.lock().connections.clone()
    }

    pub fn bound(&self) -> Option<String> {
        Self::ready(&self.lock())
    }

    /// Milliseconds since the receiver started (the clock of `received_ms`).
    pub fn now_ms(&self) -> u64 {
        self.state.t0.elapsed().as_millis() as u64
    }

    pub fn ambiguous(&self) -> Vec<String> {
        self.lock().ambiguous.clone()
    }

    /// Bytes consumed past a head boundary before any rejection (must stay 0).
    pub fn max_overread(&self) -> usize {
        self.state.overread.load(Ordering::SeqCst)
    }

    pub fn summary(&self) -> Value {
        let g = self.lock();
        json!({
            "port": self.state.port,
            "launch_id": self.state.cfg.launch_id,
            "bound_session": g.bound_session,
            "bound_at_ms": g.bound_at,
            "ambiguous": g.ambiguous,
            "events": g.events,
            "witnesses": g.witnesses,
            "rejections": g.rejections,
            "connections": g.connections,
        })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.accept.take() {
            let _ = j.join();
        }
        // Connection threads end by their own 7 s bound; wait for them, bounded.
        let end = Instant::now() + CONNECTION_BOUND + Duration::from_secs(1);
        while self.state.active.load(Ordering::SeqCst) > 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        421 => "Misdirected Request",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

pub type AmbiguityHook = Box<dyn Fn() + Send + Sync>;

fn respond(s: &mut TcpStream, code: u16, body: &[u8]) -> io::Result<()> {
    respond_by(s, code, body, Instant::now() + Duration::from_millis(200))
}

/// Writes the response, recomputing the write timeout from `deadline` before
/// every write.
fn respond_by(s: &mut TcpStream, code: u16, body: &[u8], deadline: Instant) -> io::Result<()> {
    respond_counted(s, code, body, deadline).1
}

/// Like `respond_by`, also returning how many bytes reached the socket.
fn respond_counted(
    s: &mut TcpStream,
    code: u16,
    body: &[u8],
    deadline: Instant,
) -> (usize, io::Result<()>) {
    let mut msg = format!(
        "HTTP/1.1 {code} {}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        reason(code),
        body.len()
    )
    .into_bytes();
    msg.extend_from_slice(body);
    let mut n = 0;
    while n < msg.len() {
        let r = remaining(deadline)
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "write budget spent"))
            .and_then(|left| s.set_write_timeout(Some(left)))
            .and_then(|_| s.write(&msg[n..(n + 16 * 1024).min(msg.len())]));
        match r {
            Ok(0) => return (n, Err(io::ErrorKind::WriteZero.into())),
            Ok(k) => n += k,
            Err(e) => return (n, Err(e)),
        }
    }
    (n, s.flush())
}

/// Reads and discards unread request bytes until the peer closes, `deadline`
/// passes, or `cap` bytes were read. Nothing read here is parsed or stored.
/// The read timeout is set once to a short tick (macOS may refuse a later
/// `setsockopt` on a half-closed socket) and the deadline is checked per read.
fn drain(s: &mut TcpStream, cap: usize, deadline: Instant) -> (usize, String) {
    const TICK: Duration = Duration::from_millis(50);
    let mut n = 0;
    let mut buf = vec![0u8; 16 * 1024];
    let Some(left) = remaining(deadline) else {
        return (n, "deadline".into());
    };
    if let Err(e) = s.set_read_timeout(Some(left.min(TICK))) {
        return (n, format!("error: {}", e.kind()));
    }
    loop {
        if n >= cap {
            return (n, "byte_cap".into());
        }
        if remaining(deadline).is_none() {
            return (n, "deadline".into());
        }
        let want = buf.len().min(cap - n);
        match s.read(&mut buf[..want]) {
            Ok(0) => return (n, "peer_closed".into()),
            Ok(k) => n += k,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return (n, format!("error: {}", e.kind())),
        }
    }
}

enum Outcome {
    Reply(u16, Vec<u8>),
    /// Peer closed before a full head; nothing to answer.
    Closed,
}

/// What `process` learned about the request, for the drain and the log.
#[derive(Default)]
struct ConnInfo {
    route: &'static str,
    /// A valid `Content-Length` (bounds the drain).
    declared: Option<usize>,
}

struct Head {
    method: String,
    path: String,
    hosts: Vec<String>,
    auths: Vec<Vec<u8>>,
    te: bool,
    cls: Vec<String>,
    thread: Option<String>,
    agent: Option<String>,
    emitter_version: Option<String>,
    ancestry: Option<String>,
}

fn remaining(deadline: Instant) -> Option<Duration> {
    let now = Instant::now();
    (now < deadline).then(|| deadline - now)
}

impl State {
    fn reject(&self, code: u16) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g.rejections.entry(code).or_insert(0) += 1;
    }

    fn handle(&self, mut s: TcpStream) {
        let accepted = Instant::now();
        let bound = accepted + CONNECTION_BOUND;
        let deadline = accepted + REQUEST_DEADLINE;
        let mut info = ConnInfo {
            route: "unrouted",
            declared: None,
        };
        let outcome = self.process(&mut s, deadline, &mut info);
        let ms = || accepted.elapsed().as_millis() as u64;
        let mut rec = ConnRecord {
            seq: CONN_SEQ.fetch_add(1, Ordering::SeqCst) + 1,
            route: info.route.into(),
            decided_ms: ms(),
            ..Default::default()
        };
        if let Outcome::Reply(code, body) = outcome {
            rec.decided = Some(code);
            if code != 200 {
                self.reject(code);
            }
            // Its own write budget from the decision, so a 408 decided at the
            // request deadline still reaches the client.
            let (n, r) = respond_counted(
                &mut s,
                code,
                &body,
                (Instant::now() + RESPONSE_BUDGET).min(bound),
            );
            rec.bytes_written = n;
            rec.write_ok = Some(r.is_ok());
            rec.write_error = r.err().map(|e| e.to_string());
            if code != 200 {
                // Half-close, then discard what the client still sends, so
                // closing never resets a connection the response is still on.
                let _ = s.shutdown(Shutdown::Write);
                let cap = info.declared.map_or(MAX_BODY, |n| n.min(MAX_BODY)) + DRAIN_SLACK;
                let (d, why) = drain(&mut s, cap, (Instant::now() + DRAIN_TIME).min(bound));
                rec.drained = d;
                rec.drain_stop = Some(why);
            }
        } else {
            rec.route = "closed".into();
        }
        let _ = s.shutdown(Shutdown::Both);
        rec.total_ms = ms();
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .connections
            .push(rec);
    }

    fn read_some(s: &mut TcpStream, buf: &mut [u8], deadline: Instant) -> Result<usize, u16> {
        let left = remaining(deadline).ok_or(408u16)?;
        s.set_read_timeout(Some(left)).map_err(|_| 400u16)?;
        match s.read(buf) {
            Ok(n) => Ok(n),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Err(408)
            }
            Err(_) => Err(400),
        }
    }

    /// Reads exactly through `\r\n\r\n` (one byte per read), never past it.
    fn read_head(s: &mut TcpStream, deadline: Instant) -> Result<Option<Vec<u8>>, u16> {
        let mut head = Vec::with_capacity(512);
        let mut b = [0u8; 1];
        loop {
            if head.ends_with(b"\r\n\r\n") {
                return Ok(Some(head));
            }
            if head.len() == MAX_HEAD {
                return Err(431);
            }
            match Self::read_some(s, &mut b, deadline)? {
                0 => return Ok(None),
                _ => head.push(b[0]),
            }
        }
    }

    fn process(&self, s: &mut TcpStream, deadline: Instant, info: &mut ConnInfo) -> Outcome {
        let raw = match Self::read_head(s, deadline) {
            Ok(Some(h)) => h,
            Ok(None) => return Outcome::Closed,
            Err(code) => return Outcome::Reply(code, Vec::new()),
        };
        // `raw` ends at the boundary by construction; nothing past it has been read.
        let consumed_past_head = raw.len() - (find_head_end(&raw).unwrap_or(raw.len()));
        self.overread
            .fetch_max(consumed_past_head, Ordering::SeqCst);
        let head = match parse_head(&raw) {
            Ok(h) => h,
            Err(code) => return Outcome::Reply(code, Vec::new()),
        };
        // The drain bound uses the declared length whatever check rejects.
        info.declared = match head.cls.as_slice() {
            [c] if !c.is_empty() && c.len() <= 12 && c.bytes().all(|b| b.is_ascii_digit()) => {
                c.parse().ok()
            }
            _ => None,
        };
        let probe = self.probe.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // 1. method / path
        let route = match (head.path.as_str(), head.method.as_str()) {
            ("/hook", "POST") => "hook",
            ("/witness", "POST") => "witness",
            ("/hook", _) | ("/witness", _) => return Outcome::Reply(405, Vec::new()),
            ("/probe/screen", "GET") if probe.is_some() => "screen",
            ("/probe/input", "POST") if probe.is_some() => "input",
            ("/probe/key", "POST") if probe.is_some() => "key",
            ("/probe/confirm", "POST") if probe.is_some() => "confirm",
            ("/probe/screen" | "/probe/input" | "/probe/key" | "/probe/confirm", _)
                if probe.is_some() =>
            {
                return Outcome::Reply(405, Vec::new());
            }
            _ => return Outcome::Reply(404, Vec::new()),
        };
        info.route = route;
        // 2. Host
        let ok_hosts = [
            format!("127.0.0.1:{}", self.port),
            format!("localhost:{}", self.port),
        ];
        if head.hosts.len() != 1 || !ok_hosts.contains(&head.hosts[0]) {
            return Outcome::Reply(421, Vec::new());
        }
        // 3. Authorization (probe routes use the separate per-run credential)
        let is_probe = matches!(route, "screen" | "input" | "key" | "confirm");
        let secret = match (&probe, is_probe) {
            (Some(p), true) => p.credential(),
            _ => self.cfg.token.clone(),
        };
        let authed = head.auths.len() == 1
            && head.auths[0].starts_with(b"Bearer ")
            && bool::from(head.auths[0][7..].ct_eq(secret.as_bytes()));
        if !authed {
            return Outcome::Reply(401, Vec::new());
        }
        // 4. framing
        if head.te {
            return Outcome::Reply(400, Vec::new());
        }
        let body_len = if route == "screen" {
            if head.cls.iter().any(|c| c != "0") || head.cls.len() > 1 {
                return Outcome::Reply(400, Vec::new());
            }
            0
        } else {
            if head.cls.len() != 1 {
                return Outcome::Reply(411, Vec::new());
            }
            let c = &head.cls[0];
            if c.is_empty() || c.len() > 12 || !c.bytes().all(|b| b.is_ascii_digit()) {
                return Outcome::Reply(411, Vec::new());
            }
            let n: usize = c.parse().unwrap_or(usize::MAX);
            if n > MAX_BODY {
                return Outcome::Reply(413, Vec::new());
            }
            n
        };
        // 5. body, bounded by Content-Length and the deadline
        let mut body = Vec::with_capacity(body_len.min(64 * 1024));
        let mut chunk = vec![0u8; 64 * 1024];
        while body.len() < body_len {
            let want = (body_len - body.len()).min(chunk.len());
            match Self::read_some(s, &mut chunk[..want], deadline) {
                Ok(0) => return Outcome::Reply(400, Vec::new()),
                Ok(n) => body.extend_from_slice(&chunk[..n]),
                Err(code) => return Outcome::Reply(code, Vec::new()),
            }
        }
        match (route, probe) {
            ("hook", _) => self.store_hook(&head, body, deadline),
            ("witness", _) => self.store_witness(&body, deadline),
            ("screen", Some(p)) => Outcome::Reply(200, p.screen(deadline)),
            ("input", Some(p)) => Outcome::Reply(p.input(&body, deadline), Vec::new()),
            ("key", Some(p)) => Outcome::Reply(p.key(&body, deadline), Vec::new()),
            ("confirm", Some(p)) => Outcome::Reply(p.confirm(&body, deadline), Vec::new()),
            _ => Outcome::Reply(404, Vec::new()),
        }
    }

    fn nearest_cli(&self, chain: &[(u32, String)]) -> Option<u32> {
        chain
            .iter()
            .skip(1)
            .find(|(_, n)| self.cfg.cli_names.contains(&util::normalize_proc_name(n)))
            .map(|(p, _)| *p)
    }

    /// The launched CLI's pid, waiting at most 1 s and never past `deadline`.
    fn cli_pid(&self, deadline: Instant) -> Option<u32> {
        let wait = remaining(deadline)?.min(Duration::from_secs(1));
        let g = self.cli_pid.lock().unwrap_or_else(|e| e.into_inner());
        let (g, _) = self
            .cli_cv
            .wait_timeout_while(g, wait, |p| p.is_none())
            .unwrap_or_else(|e| e.into_inner());
        *g
    }

    fn store_hook(&self, head: &Head, body: Vec<u8>, deadline: Instant) -> Outcome {
        let parsed: Option<Value> = serde_json::from_slice(&body).ok();
        let event_field = parsed
            .as_ref()
            .and_then(|v| v.get("hook_event_name"))
            .and_then(Value::as_str);
        let event = match event_field {
            Some(e) if KNOWN_EVENTS.contains(&e) => e.to_string(),
            _ => "unknown".to_string(),
        };
        let session_id = parsed
            .as_ref()
            .and_then(|v| v.get("session_id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let chain = head
            .ancestry
            .as_deref()
            .map(util::decode_ancestry)
            .unwrap_or_default();
        let nearest = self.nearest_cli(&chain);
        let cli_pid = match self.cfg.rule {
            BindRule::Ancestor => self.cli_pid(deadline),
            _ => None,
        };
        let thread_ok = head.thread.as_deref() == Some(self.cfg.thread_id.as_str());
        let ancestor_ok = nearest.is_some() && nearest == cli_pid;

        let dir = self
            .cfg
            .fixtures_dir
            .join(util::sanitize_component(&self.cfg.agent))
            .join(util::sanitize_component(&self.cfg.cli_version))
            .join(util::sanitize_component(&self.cfg.launch_id));
        // Nothing is published (fixture or readiness) once the deadline passed.
        if remaining(deadline).is_none() {
            return Outcome::Reply(408, Vec::new());
        }
        if std::fs::create_dir_all(&dir).is_err() {
            return Outcome::Reply(500, Vec::new());
        }
        let received_ms = self.t0.elapsed().as_millis() as u64;
        let seq = SEQ.fetch_add(1, Ordering::SeqCst) + 1;
        let name = format!("{event}-{seq}");
        let path = dir.join(format!("{name}.json"));
        if std::fs::write(&path, &body).is_err() {
            return Outcome::Reply(500, Vec::new());
        }
        let meta = json!({
            "received_unix_ms": util::now_ms(),
            "run_id": self.cfg.run_id,
            "launch_id": self.cfg.launch_id,
            "thread_id": head.thread,
            "agent": head.agent,
            "emitter_version": head.emitter_version,
            "emitter_ancestry": chain,
        });
        let _ = std::fs::write(
            dir.join(format!("{name}.meta.json")),
            serde_json::to_vec_pretty(&meta).unwrap_or_default(),
        );

        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if remaining(deadline).is_none() {
            drop(g);
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(dir.join(format!("{name}.meta.json")));
            return Outcome::Reply(408, Vec::new());
        }
        let was_ambiguous = !g.ambiguous.is_empty();
        let foreign = ("raw_unknown", Some("foreign_session".to_string()));
        let (class, why) = if parsed.is_none() {
            ("raw_unknown", Some("unparseable".to_string()))
        } else {
            match (&self.cfg.rule, event.as_str()) {
                (BindRule::SessionId(want), _) => {
                    if session_id.as_deref() == Some(want.as_str()) && thread_ok {
                        if event == "SessionStart" && g.bound_session.is_none() {
                            g.bound_session = Some(want.clone());
                            g.bound_at = Some(received_ms);
                        }
                        ("bound", None)
                    } else {
                        foreign
                    }
                }
                (BindRule::Ancestor, "SessionStart") => {
                    match (thread_ok && ancestor_ok, &g.bound_session, &session_id) {
                        (true, None, Some(sid)) => {
                            g.bound_session = Some(sid.clone());
                            g.bound_at = Some(received_ms);
                            ("bound", None)
                        }
                        (true, Some(b), Some(sid)) if b == sid => ("bound", None),
                        (true, Some(_), Some(sid)) => {
                            // A second eligible candidate: readiness is invalidated.
                            g.ambiguous.push(sid.clone());
                            ("raw_unknown", Some("ambiguous_candidate".into()))
                        }
                        _ => foreign,
                    }
                }
                (BindRule::Ancestor, _) => {
                    // Evidence events re-check the launch identity, not just the id.
                    if g.bound_session.is_some()
                        && g.bound_session == session_id
                        && thread_ok
                        && ancestor_ok
                    {
                        ("bound", None)
                    } else {
                        foreign
                    }
                }
            }
        };
        let rel = path
            .strip_prefix(self.cfg.fixtures_dir.parent().unwrap_or(Path::new("")))
            .unwrap_or(&path);
        g.events.push(EventRecord {
            seq,
            event,
            fixture: rel.display().to_string(),
            class: class.into(),
            reason: why,
            session_id,
            received_ms,
            thread_header: head.thread.clone(),
            agent_header: head.agent.clone(),
            ancestry: chain,
            nearest_cli: nearest,
            body_len: body.len(),
            body,
        });
        let became_ambiguous = !was_ambiguous && !g.ambiguous.is_empty();
        drop(g);
        if became_ambiguous
            && let Some(h) = self
                .on_ambiguous
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
        {
            h();
        }
        self.cv.notify_all();
        Outcome::Reply(200, Vec::new())
    }

    /// Like `store_hook`, nothing is published once the deadline passed,
    /// checked again under the state lock.
    fn store_witness(&self, body: &[u8], deadline: Instant) -> Outcome {
        let Ok(v) = serde_json::from_slice::<Value>(body) else {
            return Outcome::Reply(400, Vec::new());
        };
        let (Some(pid), Some(start_time), Some(role)) = (
            v.get("pid").and_then(Value::as_u64),
            v.get("start_time").and_then(Value::as_u64),
            v.get("role").and_then(Value::as_str),
        ) else {
            return Outcome::Reply(400, Vec::new());
        };
        let rec = WitnessRecord {
            pid: pid as u32,
            start_time,
            role: role.to_string(),
            session: v.get("session").and_then(Value::as_bool).unwrap_or(true),
            report: v.get("report").cloned().unwrap_or(Value::Null),
            received_ms: self.t0.elapsed().as_millis() as u64,
        };
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if remaining(deadline).is_none() {
            return Outcome::Reply(408, Vec::new());
        }
        g.witnesses.push(rec);
        drop(g);
        self.cv.notify_all();
        Outcome::Reply(200, Vec::new())
    }
}

fn find_head_end(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn parse_head(raw: &[u8]) -> Result<Head, u16> {
    let mut hdrs = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut req = httparse::Request::new(&mut hdrs);
    match req.parse(raw) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err(400),
        Err(httparse::Error::TooManyHeaders) => return Err(431),
        Err(_) => return Err(400),
    }
    let mut h = Head {
        method: req.method.unwrap_or("").to_string(),
        path: req.path.unwrap_or("").to_string(),
        hosts: Vec::new(),
        auths: Vec::new(),
        te: false,
        cls: Vec::new(),
        thread: None,
        agent: None,
        emitter_version: None,
        ancestry: None,
    };
    for hd in req.headers.iter() {
        let v = String::from_utf8_lossy(hd.value).into_owned();
        match hd.name.to_ascii_lowercase().as_str() {
            "host" => h.hosts.push(v),
            "authorization" => h.auths.push(hd.value.to_vec()),
            "transfer-encoding" => h.te = true,
            "content-length" => h.cls.push(v),
            "x-chaldea-thread-id" => h.thread = Some(v),
            "x-chaldea-agent" => h.agent = Some(v),
            "x-chaldea-emitter-version" => h.emitter_version = Some(v),
            "x-chaldea-emitter-ancestry" => h.ancestry = Some(v),
            _ => {}
        }
    }
    Ok(h)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn cfg(dir: &Path, launch: &str, rule: BindRule) -> Config {
        Config {
            token: "t0k".into(),
            thread_id: "thread-1".into(),
            agent: "fake".into(),
            cli_version: "v1".into(),
            launch_id: launch.into(),
            rule,
            fixtures_dir: dir.join("fixtures"),
            run_id: "r".into(),
            cli_names: vec!["chs-fake-cli".into()],
        }
    }

    pub fn test_receiver(dir: &Path) -> Receiver {
        Receiver::start(cfg(dir, "l1", BindRule::SessionId("sid-1".into()))).unwrap()
    }

    pub fn raw(port: u16, req: &[u8]) -> u16 {
        let mut s = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port))).unwrap();
        let _ = s.write_all(req);
        crate::http_client::read_response(&mut s, Instant::now() + Duration::from_secs(8))
            .map(|r| r.0)
            .unwrap_or(0)
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("chs-recv-test-{}", util::run_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fixtures(dir: &Path) -> usize {
        fn walk(p: &Path) -> usize {
            std::fs::read_dir(p)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| {
                            if e.path().is_dir() {
                                walk(&e.path())
                            } else {
                                1
                            }
                        })
                        .sum()
                })
                .unwrap_or(0)
        }
        walk(&dir.join("fixtures"))
    }

    const BODY: &str = r#"{"hook_event_name":"SessionStart","session_id":"sid-1"}"#;
    const THREAD: &str = "X-Chaldea-Thread-Id: thread-1\r\n";

    fn req(host: &str, auth: &str, extra: &str, body: &str) -> Vec<u8> {
        format!(
            "POST /hook HTTP/1.1\r\nHost: {host}\r\n{auth}{extra}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn rejects_bad_head_auth_and_framing_without_fixtures() {
        let d = tmp();
        let mut r = test_receiver(&d);
        let p = r.port();
        let host = format!("127.0.0.1:{p}");
        let good_auth = "Authorization: Bearer t0k\r\n";
        let cases: Vec<(&str, Vec<u8>, u16)> =
            vec![
            ("bad host", req("evil:1", good_auth, THREAD, BODY), 421),
            (
                "dup host",
                req(&host, good_auth, &format!("Host: {host}\r\n{THREAD}"), BODY),
                421,
            ),
            ("no token", req(&host, "", THREAD, BODY), 401),
            (
                "wrong token",
                req(&host, "Authorization: Bearer t0x\r\n", THREAD, BODY),
                401,
            ),
            ("dup token", req(&host, good_auth, &format!("{good_auth}{THREAD}"), BODY), 401),
            ("te", req(&host, good_auth, "Transfer-Encoding: chunked\r\n", BODY), 400),
            ("dup cl", req(&host, good_auth, "Content-Length: 1\r\n", BODY), 411),
            (
                "no cl",
                format!("POST /hook HTTP/1.1\r\nHost: {host}\r\n{good_auth}\r\n").into_bytes(),
                411,
            ),
            (
                "big body",
                format!(
                    "POST /hook HTTP/1.1\r\nHost: {host}\r\n{good_auth}Content-Length: {}\r\n\r\n",
                    MAX_BODY + 1
                )
                .into_bytes(),
                413,
            ),
            (
                "method",
                format!("GET /hook HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes(),
                405,
            ),
            (
                "path",
                format!("POST /x HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes(),
                404,
            ),
            (
                "huge head",
                format!(
                    "POST /hook HTTP/1.1\r\nHost: {host}\r\nX-Pad: {}\r\n\r\n",
                    "a".repeat(MAX_HEAD)
                )
                .into_bytes(),
                431,
            ),
        ];
        for (name, bytes, want) in cases {
            assert_eq!(raw(p, &bytes), want, "{name}");
        }
        assert_eq!(fixtures(&d), 0);
        assert!(r.bound().is_none());
        assert_eq!(raw(p, &req(&host, good_auth, THREAD, BODY)), 200);
        assert_eq!(fixtures(&d), 2);
        assert_eq!(r.bound().as_deref(), Some("sid-1"));
        r.stop();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// M7: a rejected head coalesced with its body must not consume body bytes.
    #[test]
    fn rejection_reads_nothing_past_head() {
        let d = tmp();
        let r = test_receiver(&d);
        let p = r.port();
        let big = "x".repeat(4000);
        let bytes = req("evil:1", "Authorization: Bearer t0k\r\n", THREAD, &big);
        assert_eq!(raw(p, &bytes), 421);
        assert_eq!(r.max_overread(), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unknown_payload_kept_verbatim() {
        let d = tmp();
        let r = test_receiver(&d);
        let p = r.port();
        let body = "  {\"hook_event_name\" : \"Future\",\n \"x\":[1,2]}  ";
        let bytes = req(
            &format!("localhost:{p}"),
            "Authorization: Bearer t0k\r\n",
            "",
            body,
        );
        assert_eq!(raw(p, &bytes), 200);
        let ev = r.events();
        assert_eq!(ev[0].event, "unknown");
        assert_eq!(ev[0].class, "raw_unknown");
        let stored = std::fs::read(d.join(&ev[0].fixture)).unwrap();
        assert_eq!(stored, body.as_bytes());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// H7: two launches with the same agent/version keep separate raw evidence.
    #[test]
    fn launches_do_not_overwrite_each_others_fixtures() {
        let d = tmp();
        let a = Receiver::start(cfg(&d, "launch-a", BindRule::SessionId("sid-1".into()))).unwrap();
        let b = Receiver::start(cfg(&d, "launch-b", BindRule::SessionId("sid-1".into()))).unwrap();
        let auth = "Authorization: Bearer t0k\r\n";
        let body_a = r#"{"hook_event_name":"SessionStart","session_id":"sid-1","n":"a"}"#;
        let body_b = r#"{"hook_event_name":"SessionStart","session_id":"sid-1","n":"b"}"#;
        assert_eq!(
            raw(
                a.port(),
                &req(&format!("127.0.0.1:{}", a.port()), auth, THREAD, body_a)
            ),
            200
        );
        assert_eq!(
            raw(
                b.port(),
                &req(&format!("127.0.0.1:{}", b.port()), auth, THREAD, body_b)
            ),
            200
        );
        let (ea, eb) = (&a.events()[0], &b.events()[0]);
        assert_ne!(ea.fixture, eb.fixture);
        assert_ne!(ea.seq, eb.seq);
        assert_eq!(
            std::fs::read(d.join(&ea.fixture)).unwrap(),
            body_a.as_bytes()
        );
        assert_eq!(
            std::fs::read(d.join(&eb.fixture)).unwrap(),
            body_b.as_bytes()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// M3: a second eligible SessionStart invalidates readiness, and evidence
    /// events re-check the thread header and nearest CLI ancestor.
    #[test]
    fn ambiguity_invalidates_readiness_and_followups_recheck_identity() {
        let d = tmp();
        let r = Receiver::start(cfg(&d, "l", BindRule::Ancestor)).unwrap();
        r.set_cli_pid(4242);
        let host = format!("127.0.0.1:{}", r.port());
        let auth = "Authorization: Bearer t0k\r\n";
        let ok_anc =
            format!("{THREAD}X-Chaldea-Emitter-Ancestry: 1:chs-hook-emit/4242:chs-fake-cli\r\n");
        let ss =
            |sid: &str| format!(r#"{{"hook_event_name":"SessionStart","session_id":"{sid}"}}"#);
        assert_eq!(raw(r.port(), &req(&host, auth, &ok_anc, &ss("s1"))), 200);
        assert_eq!(r.bound().as_deref(), Some("s1"));
        // Follow-up with the bound id but a foreign thread header is not evidence.
        let ups = r#"{"hook_event_name":"UserPromptSubmit","session_id":"s1","prompt":"x"}"#;
        let bad_thread =
            "X-Chaldea-Thread-Id: other\r\nX-Chaldea-Emitter-Ancestry: 1:e/4242:chs-fake-cli\r\n";
        assert_eq!(raw(r.port(), &req(&host, auth, bad_thread, ups)), 200);
        // Same thread, but the nearest CLI is a nested one.
        let nested = format!(
            "{THREAD}X-Chaldea-Emitter-Ancestry: 1:e/77:chs-fake-cli/4242:chs-fake-cli\r\n"
        );
        assert_eq!(raw(r.port(), &req(&host, auth, &nested, ups)), 200);
        assert!(
            r.events()[1..].iter().all(|e| !e.is_bound()),
            "{:?}",
            r.events()
        );
        assert_eq!(raw(r.port(), &req(&host, auth, &ok_anc, ups)), 200);
        assert!(r.events()[3].is_bound());
        // A conflicting eligible SessionStart: never ready again, and the
        // writer gate is revoked (a queued paste admitted earlier is dropped).
        let gate = Arc::new(crate::pty::GateState::default());
        gate.set(true);
        let admitted = gate.admit().unwrap();
        let g2 = gate.clone();
        r.on_ambiguous(Box::new(move || g2.revoke()));
        assert!(gate.admit().is_some());
        assert_eq!(raw(r.port(), &req(&host, auth, &ok_anc, &ss("s2"))), 200);
        assert!(gate.admit().is_none(), "ambiguity did not revoke the gate");
        assert_ne!(gate.epoch.load(Ordering::SeqCst), admitted);
        assert!(r.bound().is_none());
        assert_eq!(r.ambiguous(), vec!["s2".to_string()]);
        assert!(
            r.wait_bound(Instant::now() + Duration::from_millis(50))
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Connection records, waiting (bounded) until `n` exist.
    fn wait_conns(r: &Receiver, n: usize) -> Vec<ConnRecord> {
        let end = Instant::now() + Duration::from_secs(10);
        while r.connections().len() < n && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        r.connections()
    }

    fn connect(port: u16) -> TcpStream {
        TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port))).unwrap()
    }

    fn read_status(s: &mut TcpStream) -> u16 {
        crate::http_client::read_response(s, Instant::now() + Duration::from_secs(9))
            .map(|r| r.0)
            .unwrap_or(0)
    }

    /// F-3c: both deadline cases and `Content-Length: +5` deliver their
    /// status to the client, and the receiver's decided and written status
    /// match what the client read.
    #[test]
    fn deadline_and_bad_length_statuses_reach_the_client() {
        let d = tmp();
        let r = test_receiver(&d);
        let p = r.port();
        let host = format!("127.0.0.1:{p}");
        let slow_head = format!("POST /hook HTTP/1.1\r\nHost: {host}\r\n");
        let slow_body = format!(
            "POST /hook HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer t0k\r\n{THREAD}Content-Length: {}\r\n\r\n{}",
            BODY.len(),
            &BODY[..5]
        );
        let plus5 = format!(
            "POST /hook HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer t0k\r\nContent-Length: +5\r\n\r\nabcde"
        );
        let hs: Vec<_> = [(slow_head, 408u16), (slow_body, 408), (plus5, 411)]
            .into_iter()
            .map(|(req, want)| {
                std::thread::spawn(move || {
                    let t = Instant::now();
                    let mut s = connect(p);
                    s.write_all(req.as_bytes()).unwrap();
                    (want, read_status(&mut s), t.elapsed())
                })
            })
            .collect();
        for h in hs {
            let (want, got, took) = h.join().unwrap();
            assert_eq!(got, want, "client status after {took:?}");
        }
        let conns = wait_conns(&r, 3);
        for want in [408u16, 408, 411] {
            assert!(
                conns.iter().any(|c| c.decided == Some(want)
                    && c.write_ok == Some(true)
                    && c.bytes_written > 0
                    && c.total_ms <= CONNECTION_BOUND.as_millis() as u64),
                "{want}: {conns:#?}"
            );
        }
        assert_eq!(conns.iter().filter(|c| c.decided == Some(408)).count(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// F-3c ordering and drain: a rejected head gets its status before any
    /// body byte is sent; a body sent up front (1 MiB) is drained and the
    /// status still arrives; the drain stops at its byte and time bounds.
    #[test]
    fn rejection_ordering_and_drain_bounds() {
        let d = tmp();
        let r = test_receiver(&d);
        let p = r.port();
        let host = format!("127.0.0.1:{p}");
        let head = |len: usize| {
            format!(
                "POST /hook HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer bad\r\nContent-Length: {len}\r\n\r\n"
            )
        };
        let mib = vec![b'x'; 1 << 20];
        // Body withheld until the status was read.
        let mut s = connect(p);
        s.write_all(head(mib.len()).as_bytes()).unwrap();
        assert_eq!(read_status(&mut s), 401);
        let _ = s.write_all(&mib[..1024]);
        drop(s);
        // reject_with_body_sent: the whole 1 MiB body up front.
        let mut s = connect(p);
        s.write_all(head(mib.len()).as_bytes()).unwrap();
        let mut w = s.try_clone().unwrap();
        let body = mib.clone();
        let writer = std::thread::spawn(move || w.write_all(&body).is_ok());
        assert_eq!(read_status(&mut s), 401);
        assert!(writer.join().unwrap(), "the drain accepted the whole body");
        drop(s);
        // Byte cap: 10 bytes declared, 200 KiB sent; the socket stays open.
        let mut s = connect(p);
        s.write_all(head(10).as_bytes()).unwrap();
        let mut w = s.try_clone().unwrap();
        let writer = std::thread::spawn(move || {
            let _ = w.write_all(&vec![b'y'; 200 * 1024]);
        });
        assert_eq!(read_status(&mut s), 401);
        let conns = wait_conns(&r, 3);
        drop(s);
        let _ = writer.join();
        // Time bound: nothing more is sent and the socket stays open.
        let mut s = connect(p);
        s.write_all(head(1000).as_bytes()).unwrap();
        assert_eq!(read_status(&mut s), 401);
        let conns4 = wait_conns(&r, 4);
        drop(s);
        let c = &conns4[3];
        assert_eq!(c.drain_stop.as_deref(), Some("deadline"), "{c:#?}");
        assert_eq!(c.drained, 0);
        let up_front = &conns[1];
        assert_eq!(up_front.drained, mib.len(), "{up_front:#?}");
        assert_eq!(up_front.drain_stop.as_deref(), Some("peer_closed"));
        let capped = &conns[2];
        assert_eq!(
            capped.drain_stop.as_deref(),
            Some("byte_cap"),
            "{capped:#?}"
        );
        assert_eq!(capped.drained, 10 + DRAIN_SLACK);
        assert!(
            conns4
                .iter()
                .all(|c| c.decided == Some(401) && c.write_ok == Some(true))
        );
        assert_eq!(fixtures(&d), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// F-3c: a client still sending its body when the 408 is decided reads
    /// 408; the drain is stopped by its own deadline and the connection ends
    /// inside the 7 s bound.
    #[test]
    fn drain_under_deadline_is_bounded() {
        let d = tmp();
        let r = test_receiver(&d);
        let p = r.port();
        let t = Instant::now();
        let mut s = connect(p);
        s.write_all(
            format!(
                "POST /hook HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nAuthorization: Bearer t0k\r\nContent-Length: {}\r\n\r\n",
                1 << 20
            )
            .as_bytes(),
        )
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (mut w, st) = (s.try_clone().unwrap(), stop.clone());
        let trickle = std::thread::spawn(move || {
            while !st.load(Ordering::SeqCst) && t.elapsed() < Duration::from_secs(9) {
                if w.write_all(&[b'z'; 512]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        assert_eq!(read_status(&mut s), 408);
        let conns = wait_conns(&r, 1);
        stop.store(true, Ordering::SeqCst);
        let _ = trickle.join();
        let c = &conns[0];
        assert_eq!(c.decided, Some(408));
        assert_eq!(c.write_ok, Some(true));
        assert!(c.drained > 0, "{c:#?}");
        assert_eq!(c.drain_stop.as_deref(), Some("deadline"));
        assert!(c.total_ms <= CONNECTION_BOUND.as_millis() as u64, "{c:#?}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
