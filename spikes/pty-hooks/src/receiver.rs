//! Hook receiver (§4.3): std `TcpListener` + `httparse`, one request per
//! connection, 5 s absolute deadline, head/auth/framing checks before any
//! body byte, fixtures stored byte-for-byte, launch binding, witnesses.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
pub const MAX_CONNS: usize = 16;

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
    /// Claude: the predetermined `--session-id`.
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

/// C-4 / C-5 probe routes, present only while a probe runs.
pub trait ProbeHandler: Send + Sync {
    fn credential(&self) -> String;
    fn screen(&self) -> Vec<u8>;
    fn input(&self, body: &[u8]) -> u16;
    fn key(&self, body: &[u8]) -> u16;
}

#[derive(Default)]
struct Inner {
    seq: u64,
    events: Vec<EventRecord>,
    witnesses: Vec<WitnessRecord>,
    rejections: BTreeMap<u16, u32>,
    bound_session: Option<String>,
    bound_at: Option<u64>,
    ambiguous: Vec<String>,
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
}

pub struct Receiver {
    state: Arc<State>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
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

    pub fn set_cli_pid(&self, pid: u32) {
        *self.state.cli_pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(pid);
        self.state.cli_cv.notify_all();
    }

    pub fn set_probe(&self, p: Option<Arc<dyn ProbeHandler>>) {
        *self.state.probe.lock().unwrap_or_else(|e| e.into_inner()) = p;
    }

    fn wait_until<T>(
        &self,
        deadline: Instant,
        mut f: impl FnMut(&Inner) -> Option<T>,
    ) -> Option<T> {
        let mut g = self.state.inner.lock().unwrap_or_else(|e| e.into_inner());
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

    /// Waits for the readiness event (a bound SessionStart); returns the bound session id.
    pub fn wait_bound(&self, deadline: Instant) -> Option<String> {
        self.wait_until(deadline, |i| i.bound_session.clone())
    }

    pub fn wait_event(
        &self,
        deadline: Instant,
        pred: impl Fn(&EventRecord) -> bool,
    ) -> Option<EventRecord> {
        self.wait_until(deadline, |i| i.events.iter().find(|e| pred(e)).cloned())
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
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .events
            .clone()
    }

    pub fn witnesses(&self) -> Vec<WitnessRecord> {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .witnesses
            .clone()
    }

    pub fn rejections(&self) -> BTreeMap<u16, u32> {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rejections
            .clone()
    }

    pub fn bound(&self) -> Option<String> {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bound_session
            .clone()
    }

    pub fn bound_at_ms(&self) -> Option<u64> {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bound_at
    }

    pub fn ambiguous(&self) -> Vec<String> {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ambiguous
            .clone()
    }

    pub fn summary(&self) -> Value {
        let g = self.state.inner.lock().unwrap_or_else(|e| e.into_inner());
        json!({
            "port": self.state.port,
            "bound_session": g.bound_session,
            "bound_at_ms": g.bound_at,
            "ambiguous": g.ambiguous,
            "events": g.events,
            "witnesses": g.witnesses,
            "rejections": g.rejections,
        })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.accept.take() {
            let _ = j.join();
        }
        // Connection threads end by their own 5 s deadline; wait for them, bounded.
        let end = Instant::now() + REQUEST_DEADLINE + Duration::from_secs(1);
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
        _ => "Status",
    }
}

fn respond(s: &mut TcpStream, code: u16, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        reason(code),
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    s.flush()
}

fn find_head_end(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

enum Outcome {
    Reply(u16, Vec<u8>),
    /// Peer closed before a full head; nothing to answer.
    Closed,
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

impl State {
    fn reject(&self, code: u16) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g.rejections.entry(code).or_insert(0) += 1;
    }

    fn handle(&self, mut s: TcpStream) {
        let deadline = Instant::now() + REQUEST_DEADLINE;
        let outcome = self.process(&mut s, deadline);
        if let Outcome::Reply(code, body) = outcome {
            if code != 200 {
                self.reject(code);
            }
            let left = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(100));
            let _ = s.set_write_timeout(Some(left));
            let _ = respond(&mut s, code, &body);
        }
        let _ = s.shutdown(Shutdown::Both);
    }

    fn read_some(s: &mut TcpStream, buf: &mut [u8], deadline: Instant) -> Result<usize, u16> {
        let now = Instant::now();
        if now >= deadline {
            return Err(408);
        }
        s.set_read_timeout(Some(deadline - now))
            .map_err(|_| 400u16)?;
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

    fn process(&self, s: &mut TcpStream, deadline: Instant) -> Outcome {
        let mut buf = vec![0u8; MAX_HEAD];
        let mut len = 0;
        let head_end = loop {
            if let Some(e) = find_head_end(&buf[..len]) {
                break e;
            }
            if len == MAX_HEAD {
                return Outcome::Reply(431, Vec::new());
            }
            match Self::read_some(s, &mut buf[len..], deadline) {
                Ok(0) => return Outcome::Closed,
                Ok(n) => len += n,
                Err(code) => return Outcome::Reply(code, Vec::new()),
            }
        };
        let head = match parse_head(&buf[..head_end]) {
            Ok(h) => h,
            Err(code) => return Outcome::Reply(code, Vec::new()),
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
            ("/probe/screen" | "/probe/input" | "/probe/key", _) if probe.is_some() => {
                return Outcome::Reply(405, Vec::new());
            }
            _ => return Outcome::Reply(404, Vec::new()),
        };
        // 2. Host
        let ok_hosts = [
            format!("127.0.0.1:{}", self.port),
            format!("localhost:{}", self.port),
        ];
        if head.hosts.len() != 1 || !ok_hosts.contains(&head.hosts[0]) {
            return Outcome::Reply(421, Vec::new());
        }
        // 3. Authorization (probe routes use the separate per-run credential)
        let secret = match (&probe, route) {
            (Some(p), "screen" | "input" | "key") => p.credential(),
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
        let mut body = buf[head_end..len].to_vec();
        if body.len() > body_len {
            return Outcome::Reply(400, Vec::new());
        }
        let mut chunk = vec![0u8; 64 * 1024];
        while body.len() < body_len {
            let want = (body_len - body.len()).min(chunk.len());
            match Self::read_some(s, &mut chunk[..want], deadline) {
                Ok(0) => return Outcome::Reply(400, Vec::new()),
                Ok(n) => body.extend_from_slice(&chunk[..n]),
                Err(code) => return Outcome::Reply(code, Vec::new()),
            }
        }
        match route {
            "hook" => self.store_hook(&head, body),
            "witness" => self.store_witness(&body),
            "screen" => Outcome::Reply(200, probe.map(|p| p.screen()).unwrap_or_default()),
            "input" => Outcome::Reply(probe.map(|p| p.input(&body)).unwrap_or(404), Vec::new()),
            "key" => Outcome::Reply(probe.map(|p| p.key(&body)).unwrap_or(404), Vec::new()),
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

    fn cli_pid(&self) -> Option<u32> {
        let g = self.cli_pid.lock().unwrap_or_else(|e| e.into_inner());
        let (g, _) = self
            .cli_cv
            .wait_timeout_while(g, Duration::from_secs(1), |p| p.is_none())
            .unwrap_or_else(|e| e.into_inner());
        *g
    }

    fn store_hook(&self, head: &Head, body: Vec<u8>) -> Outcome {
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
            BindRule::Ancestor if event == "SessionStart" => self.cli_pid(),
            _ => None,
        };

        let dir = self
            .cfg
            .fixtures_dir
            .join(util::sanitize_component(&self.cfg.agent))
            .join(util::sanitize_component(&self.cfg.cli_version));
        if std::fs::create_dir_all(&dir).is_err() {
            return Outcome::Reply(500, Vec::new());
        }
        let received_ms = self.t0.elapsed().as_millis() as u64;
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.seq += 1;
        let seq = g.seq;
        let name = format!("{event}-{seq}");
        let path = dir.join(format!("{name}.json"));
        if std::fs::write(&path, &body).is_err() {
            return Outcome::Reply(500, Vec::new());
        }
        let meta = json!({
            "received_unix_ms": util::now_ms(),
            "run_id": self.cfg.run_id,
            "thread_id": head.thread,
            "agent": head.agent,
            "emitter_version": head.emitter_version,
            "emitter_ancestry": chain,
        });
        let _ = std::fs::write(
            dir.join(format!("{name}.meta.json")),
            serde_json::to_vec_pretty(&meta).unwrap_or_default(),
        );

        let (class, why) = if parsed.is_none() {
            ("raw_unknown", Some("unparseable".to_string()))
        } else {
            match (&self.cfg.rule, event.as_str()) {
                (BindRule::SessionId(want), _) => {
                    if session_id.as_deref() == Some(want.as_str()) {
                        if event == "SessionStart" && g.bound_session.is_none() {
                            g.bound_session = Some(want.clone());
                            g.bound_at = Some(received_ms);
                        }
                        ("bound", None)
                    } else {
                        ("raw_unknown", Some("foreign_session".into()))
                    }
                }
                (BindRule::Ancestor, "SessionStart") => {
                    let thread_ok = head.thread.as_deref() == Some(self.cfg.thread_id.as_str());
                    let ancestor_ok = nearest.is_some() && nearest == cli_pid;
                    match (thread_ok && ancestor_ok, &g.bound_session, &session_id) {
                        (true, None, Some(sid)) => {
                            g.bound_session = Some(sid.clone());
                            g.bound_at = Some(received_ms);
                            ("bound", None)
                        }
                        (true, Some(b), Some(sid)) if b == sid => ("bound", None),
                        (true, Some(_), Some(sid)) => {
                            g.ambiguous.push(sid.clone());
                            ("raw_unknown", Some("ambiguous_candidate".into()))
                        }
                        _ => ("raw_unknown", Some("foreign_session".into())),
                    }
                }
                (BindRule::Ancestor, _) => {
                    if g.bound_session.is_some() && g.bound_session == session_id {
                        ("bound", None)
                    } else {
                        ("raw_unknown", Some("foreign_session".into()))
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
        drop(g);
        self.cv.notify_all();
        Outcome::Reply(200, Vec::new())
    }

    fn store_witness(&self, body: &[u8]) -> Outcome {
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
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .witnesses
            .push(rec);
        self.cv.notify_all();
        Outcome::Reply(200, Vec::new())
    }
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

    pub fn test_receiver(dir: &Path) -> Receiver {
        Receiver::start(Config {
            token: "t0k".into(),
            thread_id: "thread-1".into(),
            agent: "fake".into(),
            cli_version: "v1".into(),
            rule: BindRule::SessionId("sid-1".into()),
            fixtures_dir: dir.join("fixtures"),
            run_id: "r".into(),
            cli_names: vec!["chs-fake-cli".into()],
        })
        .unwrap()
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
        let cases: Vec<(&str, Vec<u8>, u16)> = vec![
            ("bad host", req("evil:1", good_auth, "", BODY), 421),
            (
                "dup host",
                req(&host, good_auth, &format!("Host: {host}\r\n"), BODY),
                421,
            ),
            ("no token", req(&host, "", "", BODY), 401),
            (
                "wrong token",
                req(&host, "Authorization: Bearer t0x\r\n", "", BODY),
                401,
            ),
            ("dup token", req(&host, good_auth, good_auth, BODY), 401),
            (
                "te",
                req(&host, good_auth, "Transfer-Encoding: chunked\r\n", BODY),
                400,
            ),
            (
                "dup cl",
                req(&host, good_auth, "Content-Length: 1\r\n", BODY),
                411,
            ),
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
        assert_eq!(raw(p, &req(&host, good_auth, "", BODY)), 200);
        assert_eq!(fixtures(&d), 2);
        assert_eq!(r.bound().as_deref(), Some("sid-1"));
        r.stop();
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
}
