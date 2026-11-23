//! `chaldea hook-emit` against stub listeners on `127.0.0.1:0` (hook-ingest §7.4):
//! PR9-CL-23 (exit 0, silent, within 3 s), PR9-CL-24 (attempts, one `emit_id`), PR9-CL-25
//! (no request without the exact environment or with stdin open; truncation), and FakeCli's
//! hook emission (§2.14) through the real emitter.
//!
//! Every process is started through the spawn lock, its outputs collected as raw bytes, and
//! its exit awaited at most [`PARENT_BOUND`]. The three variables the emitter reads are
//! removed from every child's environment before the test sets its own.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chaldea_daemon::lifecycle::spawn_locked;
use chaldea_pty_runtime::process::{self, Ancestry};
use serde_json::{Value, json};

const EMITTER: &str = env!("CARGO_BIN_EXE_chaldea");
const URL: &str = "CHALDEA_HOOK_URL";
const TOKEN: &str = "CHALDEA_HOOK_TOKEN";
const THREAD: &str = "CHALDEA_THREAD_ID";
const THREAD_ID: &str = "0198c0de-0000-7000-8000-000000000001";
/// PR9-CL-23's bound on a run, restated from the design: the 3 s watchdog plus 1.5 s for
/// process start and exit.
const RUN_BOUND: Duration = Duration::from_millis(4500);
/// The parent-enforced bound on any child of these tests.
const PARENT_BOUND: Duration = Duration::from_secs(10);
/// Every other bounded wait of these tests.
const WAIT: Duration = Duration::from_secs(30);
const MIB4: usize = 4 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(5);

fn token() -> String {
    format!("chd_{}", "Zz09-_".repeat(7) + "q")
}

// ---- Stub listener ----

/// What the stub does with connection `i` (the last entry repeats).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reply {
    /// Reads the request, answers this status, closes.
    Status(u16),
    /// Reads the request, never answers; the connection stays open until the stub drops.
    Silent,
    /// Accepts and never reads; the connection stays open until the stub drops.
    NoRead,
    /// Reads the request and closes without a byte.
    Close,
    /// Reads the request, writes part of a status line, closes.
    Partial,
    /// Reads the request, answers 200 after [`SLOW_REPLY`].
    Slow200,
}

/// How long [`Reply::Slow200`] holds its reply.
const SLOW_REPLY: Duration = Duration::from_millis(300);

/// One request the stub read.
#[derive(Debug, Clone)]
struct Req {
    /// When its connection was accepted, after the stub started.
    at: Duration,
    line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// The emitter's ancestry, walked while the emitter waited for the reply.
    ancestry: Option<Ancestry>,
}

impl Req {
    fn all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// The header's only value.
    fn one(&self, name: &str) -> &str {
        match self.all(name)[..] {
            [v] => v,
            ref vs => panic!("{name}: {vs:?}"),
        }
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

struct Stub {
    port: u16,
    reqs: Arc<Mutex<Vec<Req>>>,
    accepted: Arc<AtomicUsize>,
    /// Connections whose handler finished (a request read, held, or none).
    handled: Arc<AtomicUsize>,
    /// Accept passes that found the backlog empty.
    idle: Arc<AtomicUsize>,
    /// Counts `settled`'s waits for an unhandled connection (a test gate).
    settling: AtomicUsize,
    stop: Arc<AtomicBool>,
    held: Arc<Mutex<Vec<TcpStream>>>,
    accept: Option<JoinHandle<()>>,
}

/// Counts its connection as handled when `serve` ends, whatever the path.
struct Handled(Arc<AtomicUsize>);

impl Drop for Handled {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Stub {
    fn start(script: &[Reply]) -> Stub {
        Stub::start_with(script, false)
    }

    /// With `walk`, each request's handler walks the emitter's ancestry before replying.
    fn start_with(script: &[Reply], walk: bool) -> Stub {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let reqs = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(AtomicUsize::new(0));
        let handled = Arc::new(AtomicUsize::new(0));
        let idle = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let held = Arc::new(Mutex::new(Vec::new()));
        let script = script.to_vec();
        let t0 = Instant::now();
        let accept = {
            let (reqs, accepted, handled, idle, stop, held) = (
                reqs.clone(),
                accepted.clone(),
                handled.clone(),
                idle.clone(),
                stop.clone(),
                held.clone(),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((s, _)) => {
                            // Stamped at the accept, not in the handler.
                            let at = t0.elapsed();
                            let i = accepted.fetch_add(1, Ordering::SeqCst);
                            let reply = script[i.min(script.len() - 1)];
                            let (reqs, held) = (reqs.clone(), held.clone());
                            let done = Handled(handled.clone());
                            std::thread::spawn(move || {
                                let _done = done;
                                serve(s, reply, at, walk, &reqs, &held);
                            });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            idle.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(POLL);
                        }
                        // The accept loop ends; `drain` then fails at its bound.
                        Err(e) => panic!("accept: {e}"),
                    }
                }
            })
        };
        Stub {
            port,
            reqs,
            accepted,
            handled,
            idle,
            settling: AtomicUsize::new(0),
            stop,
            held,
            accept: Some(accept),
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/hook", self.port)
    }

    fn reqs(&self) -> Vec<Req> {
        self.reqs.lock().unwrap().clone()
    }

    /// Waits (bounded) until `n` requests were read.
    fn wait_reqs(&self, n: usize) -> Vec<Req> {
        let end = Instant::now() + WAIT;
        loop {
            let r = self.reqs();
            if r.len() >= n {
                return r;
            }
            assert!(Instant::now() < end, "{} of {n} requests: {r:?}", r.len());
            std::thread::sleep(POLL);
        }
    }

    /// Waits (bounded) until the accept loop has found the backlog empty in a full pass that
    /// started after this call, so every connection made before it was accepted.
    fn drain(&self) {
        let from = self.idle.load(Ordering::SeqCst);
        let end = Instant::now() + WAIT;
        while self.idle.load(Ordering::SeqCst) < from + 2 {
            assert!(Instant::now() < end, "the accept loop did not drain");
            std::thread::sleep(POLL);
        }
    }

    /// The connections accepted, after draining the backlog.
    fn accepted(&self) -> usize {
        self.drain();
        self.accepted.load(Ordering::SeqCst)
    }

    /// The requests read once the backlog is drained and every accepted connection was
    /// handled (bounded).
    fn settled(&self) -> Vec<Req> {
        self.drain();
        let end = Instant::now() + WAIT;
        while self.handled.load(Ordering::SeqCst) < self.accepted.load(Ordering::SeqCst) {
            self.settling.fetch_add(1, Ordering::SeqCst);
            assert!(
                Instant::now() < end,
                "connections not handled: {:?}",
                self.reqs()
            );
            std::thread::sleep(POLL);
        }
        self.reqs()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(a) = self.accept.take() {
            let _ = a.join();
        }
        for s in self.held.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

fn serve(
    mut s: TcpStream,
    reply: Reply,
    at: Duration,
    walk: bool,
    reqs: &Mutex<Vec<Req>>,
    held: &Mutex<Vec<TcpStream>>,
) {
    s.set_nonblocking(false).unwrap();
    if reply == Reply::NoRead {
        held.lock().unwrap().push(s);
        return;
    }
    s.set_read_timeout(Some(WAIT)).unwrap();
    let Some(mut req) = read_request(&mut s, at) else {
        return;
    };
    if walk {
        let pid: u32 = req.one("X-Chaldea-Emitter-Pid").parse().unwrap();
        req.ancestry = Some(process::ancestry(pid, 16));
    }
    reqs.lock().unwrap().push(req);
    match reply {
        Reply::Status(code) => {
            let _ = write!(
                s,
                "HTTP/1.1 {code} Stub\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = s.flush();
            let _ = s.shutdown(Shutdown::Write);
        }
        Reply::Slow200 => {
            std::thread::sleep(SLOW_REPLY);
            let _ = s.write_all(b"HTTP/1.1 200 Stub\r\nContent-Length: 0\r\n\r\n");
            let _ = s.shutdown(Shutdown::Write);
        }
        Reply::Silent => held.lock().unwrap().push(s),
        Reply::Close | Reply::NoRead => {}
        Reply::Partial => {
            let _ = s.write_all(b"HTTP/1.1 20");
            let _ = s.flush();
        }
    }
}

fn read_request(s: &mut TcpStream, at: Duration) -> Option<Req> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8(buf[..head_end].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap().to_owned();
    let headers: Vec<(String, String)> = lines
        .map(|l| {
            let (k, v) = l.split_once(':').unwrap();
            (k.to_owned(), v.trim().to_owned())
        })
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, v)| v.parse().unwrap());
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Req {
        at,
        line,
        headers,
        body,
        ancestry: None,
    })
}

/// The fixture's own accounting: `settled` returns only once every accepted connection was
/// handled, a request still being read and a connection closed without one included.
#[test]
fn stub_settles_after_every_connection_is_handled() {
    let stub = Stub::start(&[Reply::Silent]);
    let req = b"POST /hook HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}";
    let mut first = TcpStream::connect(("127.0.0.1", stub.port)).unwrap();
    first.write_all(req).unwrap();
    stub.wait_reqs(1);
    let mut second = TcpStream::connect(("127.0.0.1", stub.port)).unwrap();
    let end = Instant::now() + WAIT;
    while stub.accepted.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < end);
        std::thread::sleep(POLL);
    }
    // The gate: the second request is written only once `settled` has found that connection
    // unhandled and waits, or has returned without waiting.
    let n = std::thread::scope(|sc| {
        let settled = sc.spawn(|| stub.settled().len());
        let end = Instant::now() + WAIT;
        while stub.settling.load(Ordering::SeqCst) < 1 && !settled.is_finished() {
            assert!(Instant::now() < end, "settled did not start");
            std::thread::sleep(POLL);
        }
        second.write_all(req).unwrap();
        settled.join().unwrap()
    });
    assert_eq!(n, 2);
    // A connection closed without a request is handled too: `settled` does not wait out
    // its bound.
    drop(TcpStream::connect(("127.0.0.1", stub.port)).unwrap());
    let t = Instant::now();
    assert_eq!(stub.settled().len(), 2);
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    drop((first, second));
}

/// The report fixture keeps complete lines only: a process killed mid-line leaves an
/// unterminated last frame, which is discarded.
#[test]
fn reports_discard_an_unterminated_last_frame() {
    let reports = Reports::start();
    let mut s = TcpStream::connect(&reports.addr).unwrap();
    s.write_all(b"{\"event\":\"a\"}\n{\"ev").unwrap();
    drop(s);
    assert_eq!(reports.wait("a", 1).len(), 1);
    // The reader has ended on the cut frame without failing.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(reports.of("a").len(), 1);
}

/// A complete frame that is not JSON is a fixture failure.
#[test]
#[should_panic(expected = "malformed report frame")]
fn reports_fail_on_a_malformed_frame() {
    let reports = Reports::start();
    let mut s = TcpStream::connect(&reports.addr).unwrap();
    s.write_all(b"{bad}\n{\"event\":\"a\"}\n").unwrap();
    drop(s);
    reports.wait("a", 1);
}

// ---- Running a child ----

/// What a child gets on stdin.
enum Stdin {
    /// Written, then closed.
    Bytes(Vec<u8>),
    /// Never closed while the child runs.
    Open,
    /// Written, then closed this long after the spawn.
    LateEof(Duration, Vec<u8>),
}

struct Run {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    elapsed: Duration,
    pid: u32,
}

/// Reads `r` to its EOF on a thread; [`joined`] takes the result.
fn collect(r: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = r {
            let _ = r.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

/// What [`collect`] read, waiting at most [`WAIT`] for the EOF.
fn joined(rx: &mpsc::Receiver<Vec<u8>>, what: &str) -> Vec<u8> {
    rx.recv_timeout(WAIT)
        .unwrap_or_else(|_| panic!("{what} not closed within {WAIT:?}"))
}

/// Kills `c` and reaps it, waiting at most [`WAIT`].
fn kill_and_reap(c: &mut Child) {
    let _ = c.kill();
    let end = Instant::now() + WAIT;
    while matches!(c.try_wait(), Ok(None)) && Instant::now() < end {
        std::thread::sleep(POLL);
    }
}

/// Kills and reaps the child on drop, a panicking test included.
#[cfg(target_os = "macos")]
struct Killed(Child);

#[cfg(target_os = "macos")]
impl Drop for Killed {
    fn drop(&mut self) {
        kill_and_reap(&mut self.0);
    }
}

/// Waits for `c` at most [`PARENT_BOUND`]; kills it and panics past it.
fn wait_bounded(c: &mut Child, what: &str) -> std::process::ExitStatus {
    let end = Instant::now() + PARENT_BOUND;
    loop {
        if let Some(s) = c.try_wait().unwrap() {
            return s;
        }
        if Instant::now() >= end {
            kill_and_reap(c);
            panic!("{what} did not exit within {PARENT_BOUND:?}");
        }
        std::thread::sleep(POLL);
    }
}

/// Runs the emitter binary once before any measured run, so the OS's first-execution checks
/// of a freshly built binary (macOS scans it) do not count against a run's time budget.
fn warm() {
    static WARM: std::sync::Once = std::sync::Once::new();
    WARM.call_once(|| {
        let mut c = spawn_locked(
            emitter(&[])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .unwrap();
        wait_bounded(&mut c, "the warm-up emitter");
    });
}

/// Starts `cmd` with `stdin`, waits for its exit, and returns what it did.
fn run(cmd: &mut Command, stdin: Stdin) -> Run {
    warm();
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut c = spawn_locked(cmd).unwrap();
    // From the spawn's return: the wait for the spawn lock is not the child's time.
    let started = Instant::now();
    let out = collect(c.stdout.take());
    let err = collect(c.stderr.take());
    let mut keep = None;
    let pipe = c.stdin.take().unwrap();
    match stdin {
        Stdin::Bytes(b) => {
            std::thread::spawn(move || {
                let mut pipe = pipe;
                let _ = pipe.write_all(&b);
            });
        }
        Stdin::Open => keep = Some(pipe),
        Stdin::LateEof(d, b) => {
            let eof = started + d;
            std::thread::spawn(move || {
                let mut pipe = pipe;
                let _ = pipe.write_all(&b);
                std::thread::sleep(eof.saturating_duration_since(Instant::now()));
            });
        }
    }
    let status = wait_bounded(&mut c, "the child");
    let elapsed = started.elapsed();
    drop(keep);
    Run {
        code: status.code(),
        stdout: joined(&out, "stdout"),
        stderr: joined(&err, "stderr"),
        elapsed,
        pid: c.id(),
    }
}

/// `chaldea hook-emit codex` with only `env` of the emitter's variables.
fn emitter(env: &[(&str, &str)]) -> Command {
    let mut c = Command::new(EMITTER);
    c.args(["hook-emit", "codex"])
        .env_remove(URL)
        .env_remove(TOKEN)
        .env_remove(THREAD);
    for (k, v) in env {
        c.env(k, v);
    }
    c
}

fn full_env(url: &str) -> Vec<(&'static str, String)> {
    vec![
        (URL, url.to_owned()),
        (TOKEN, token()),
        (THREAD, THREAD_ID.to_owned()),
    ]
}

fn emit_to(url: &str, stdin: Stdin) -> Run {
    let env = full_env(url);
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    run(&mut emitter(&env), stdin)
}

const PAYLOAD: &[u8] =
    r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1","prompt":"日本語 🎌"}"#.as_bytes();

/// PR9-CL-23: exit 0, nothing on stdout or stderr, within the bound.
fn assert_quiet(r: &Run) {
    assert_eq!(
        r.code,
        Some(0),
        "stderr: {:?}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert!(r.stdout.is_empty(), "stdout: {:?}", r.stdout);
    assert!(r.stderr.is_empty(), "stderr: {:?}", r.stderr);
    assert!(r.elapsed < RUN_BOUND, "{:?}", r.elapsed);
}

/// The requests' emit ids, all equal, and their attempt numbers.
fn one_id_attempts(reqs: &[Req]) -> Vec<String> {
    let id = reqs[0].one("X-Chaldea-Emit-Id");
    for r in reqs {
        assert_eq!(r.one("X-Chaldea-Emit-Id"), id);
    }
    reqs.iter()
        .map(|r| r.one("X-Chaldea-Attempt").to_owned())
        .collect()
}

// ---- PR9-CL-23 and PR9-CL-24 ----

#[test]
fn accepting() {
    let stub = Stub::start(&[Reply::Status(200)]);
    let r = emit_to(&stub.url(), Stdin::Bytes(PAYLOAD.to_vec()));
    assert_quiet(&r);
    let reqs = stub.settled();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    let q = &reqs[0];
    assert_eq!(q.line, "POST /hook HTTP/1.1");
    assert_eq!(q.one("Host"), format!("127.0.0.1:{}", stub.port));
    assert_eq!(q.one("Connection"), "close");
    assert_eq!(q.one("Authorization"), format!("Bearer {}", token()));
    assert_eq!(q.one("Content-Type"), "application/json");
    assert_eq!(q.one("Content-Length"), PAYLOAD.len().to_string());
    assert_eq!(q.body, PAYLOAD);
    let id = q.one("X-Chaldea-Emit-Id");
    assert_eq!(id.len(), 36, "{id}");
    assert_eq!(&id[14..15], "4", "a v4 UUID: {id}");
    assert_eq!(id, id.to_ascii_lowercase());
    assert_eq!(q.one("X-Chaldea-Attempt"), "1");
    assert_eq!(q.one("X-Chaldea-Emitter-Pid"), r.pid.to_string());
    assert_eq!(q.one("X-Chaldea-Thread-Id"), THREAD_ID);
    assert_eq!(q.one("X-Chaldea-Agent"), "codex");
    assert_eq!(
        q.one("X-Chaldea-Emitter-Version"),
        env!("CARGO_PKG_VERSION")
    );
    assert!(q.all("X-Chaldea-Body-Truncated").is_empty());
    assert!(q.all("Origin").is_empty());
    assert_eq!(q.headers.len(), 11, "{:?}", q.headers);
}

#[test]
fn localhost_names_the_host_and_connects_to_loopback() {
    let stub = Stub::start(&[Reply::Status(200)]);
    let url = format!("http://localhost:{}/hook", stub.port);
    assert_quiet(&emit_to(&url, Stdin::Bytes(b"{}".to_vec())));
    let reqs = stub.settled();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].one("Host"), format!("localhost:{}", stub.port));
}

#[test]
fn empty_stdin_is_an_empty_body() {
    let stub = Stub::start(&[Reply::Status(200)]);
    assert_quiet(&emit_to(&stub.url(), Stdin::Bytes(Vec::new())));
    let reqs = stub.settled();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].one("Content-Length"), "0");
    assert!(reqs[0].body.is_empty());
}

#[test]
fn refused() {
    let port = {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        l.local_addr().unwrap().port()
    };
    let r = emit_to(
        &format!("http://127.0.0.1:{port}/hook"),
        Stdin::Bytes(PAYLOAD.to_vec()),
    );
    assert_quiet(&r);
}

/// PR9-CL-23 `no_reply` and PR9-CL-24: a silent first connection is cut at its 1.8 s
/// deadline; the second attempt follows with the same id and runs to the work end; no third
/// fits.
#[test]
fn no_reply_gets_a_second_attempt() {
    let stub = Stub::start(&[Reply::Silent]);
    let r = emit_to(&stub.url(), Stdin::Bytes(PAYLOAD.to_vec()));
    assert_quiet(&r);
    assert!(r.elapsed >= Duration::from_millis(2800), "{:?}", r.elapsed);
    let reqs = stub.settled();
    assert_eq!(reqs.len(), 2, "{reqs:?}");
    assert_eq!(one_id_attempts(&reqs), ["1", "2"]);
    let gap = reqs[1].at - reqs[0].at;
    assert!(
        gap >= Duration::from_millis(1800) && gap < Duration::from_millis(2500),
        "{gap:?}"
    );
}

/// PR9-CL-23 `slow_write`: a 4 MiB body to a listener that never reads.
#[test]
fn slow_write() {
    let stub = Stub::start(&[Reply::NoRead]);
    let r = emit_to(&stub.url(), Stdin::Bytes(vec![b'x'; MIB4]));
    assert_quiet(&r);
    assert!(stub.accepted() >= 1);
}

#[test]
fn status_408_retries_with_one_emit_id() {
    let stub = Stub::start(&[Reply::Status(408), Reply::Status(200)]);
    assert_quiet(&emit_to(&stub.url(), Stdin::Bytes(PAYLOAD.to_vec())));
    let reqs = stub.settled();
    assert_eq!(one_id_attempts(&reqs), ["1", "2"]);
    for q in &reqs {
        assert_eq!(q.body, PAYLOAD);
    }
}

#[test]
fn retries_503_with_one_emit_id() {
    let stub = Stub::start(&[Reply::Status(503), Reply::Status(503), Reply::Status(200)]);
    assert_quiet(&emit_to(&stub.url(), Stdin::Bytes(PAYLOAD.to_vec())));
    assert_eq!(one_id_attempts(&stub.settled()), ["1", "2", "3"]);
}

/// PR9-CL-24: three attempts at most, whatever the replies.
#[test]
fn at_most_three_attempts() {
    let stub = Stub::start(&[Reply::Status(503)]);
    assert_quiet(&emit_to(&stub.url(), Stdin::Bytes(PAYLOAD.to_vec())));
    let reqs = stub.settled();
    assert_eq!(one_id_attempts(&reqs), ["1", "2", "3"]);
    assert_eq!(stub.accepted(), 3);
}

/// PR9-CL-23 `rejecting`: a final status ends the run.
#[test]
fn no_retry_on_401() {
    for code in [401, 400, 413, 500] {
        let stub = Stub::start(&[Reply::Status(code), Reply::Status(200)]);
        assert_quiet(&emit_to(&stub.url(), Stdin::Bytes(PAYLOAD.to_vec())));
        assert_eq!(stub.settled().len(), 1, "{code}");
        assert_eq!(stub.accepted(), 1, "{code}");
    }
}

/// PR9-CL-23 `cut_before_status`: EOF with no byte, and EOF inside the status line.
#[test]
fn retries_after_cut_before_status() {
    for cut in [Reply::Close, Reply::Partial] {
        let stub = Stub::start(&[cut, Reply::Status(200)]);
        assert_quiet(&emit_to(&stub.url(), Stdin::Bytes(PAYLOAD.to_vec())));
        assert_eq!(one_id_attempts(&stub.settled()), ["1", "2"], "{cut:?}");
    }
}

// ---- PR9-CL-25 ----

/// EOF 2 s after the spawn: after the emitter's 1.5 s stdin cut even with 0.5 s of process
/// start, while without the cut it would still be sent (a first attempt fits until
/// `T0 + 2.6 s`).
#[test]
fn late_stdin_sends_nothing() {
    let stub = Stub::start(&[Reply::Status(200)]);
    let r = emit_to(
        &stub.url(),
        Stdin::LateEof(Duration::from_millis(2000), PAYLOAD.to_vec()),
    );
    assert_quiet(&r);
    assert!(r.elapsed >= Duration::from_millis(1500), "{:?}", r.elapsed);
    assert_eq!(stub.accepted(), 0, "{:?}", stub.settled());
}

#[test]
fn no_request_with_open_stdin() {
    let stub = Stub::start(&[Reply::Status(200)]);
    let r = emit_to(&stub.url(), Stdin::Open);
    assert_quiet(&r);
    assert!(
        r.elapsed >= Duration::from_millis(1500) && r.elapsed < Duration::from_millis(2500),
        "{:?}",
        r.elapsed
    );
    assert_eq!(stub.accepted(), 0);
}

/// Without the exact URL and token the emitter exits at once, before reading stdin (held
/// open here): well before the 1.5 s stdin cut.
#[test]
fn no_request_without_env() {
    let stub = Stub::start(&[Reply::Status(200)]);
    let url = stub.url();
    let tok = token();
    let port = stub.port;
    let cases: Vec<Vec<(&str, String)>> = vec![
        vec![],
        vec![(URL, url.clone())],
        vec![(TOKEN, tok.clone())],
        vec![(THREAD, THREAD_ID.to_owned())],
        vec![(URL, url.clone()), (THREAD, THREAD_ID.to_owned())],
        vec![(TOKEN, tok.clone()), (THREAD, THREAD_ID.to_owned())],
        vec![(URL, format!("{url}/")), (TOKEN, tok.clone())],
        vec![
            (URL, format!("http://127.0.0.1:{port}/hooks")),
            (TOKEN, tok.clone()),
        ],
        vec![
            (URL, format!("http://127.0.0.1:0{port}/hook")),
            (TOKEN, tok.clone()),
        ],
        vec![
            (URL, format!("https://127.0.0.1:{port}/hook")),
            (TOKEN, tok.clone()),
        ],
        vec![
            (URL, format!("http://[::1]:{port}/hook")),
            (TOKEN, tok.clone()),
        ],
        vec![(URL, String::new()), (TOKEN, tok.clone())],
        vec![(URL, url.clone()), (TOKEN, "chd_short".to_owned())],
        vec![(URL, url.clone()), (TOKEN, format!("{tok}x"))],
        vec![(URL, url.clone()), (TOKEN, format!("Bearer {tok}"))],
    ];
    for env in cases {
        let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let r = run(&mut emitter(&pairs), Stdin::Open);
        assert_quiet(&r);
        assert!(
            r.elapsed < Duration::from_millis(1400),
            "{env:?}: {:?}",
            r.elapsed
        );
    }
    assert_eq!(stub.accepted(), 0);
    // The control: the same stub with the full environment gets the request.
    assert_quiet(&emit_to(&url, Stdin::Bytes(PAYLOAD.to_vec())));
    assert_eq!(stub.settled().len(), 1);
}

#[test]
fn truncates_over_4_mib() {
    let stdin: Vec<u8> = (0..MIB4 + 1024)
        .map(|i| b'a' + u8::try_from(i % 26).unwrap())
        .collect();
    let stub = Stub::start(&[Reply::Status(200)]);
    assert_quiet(&emit_to(&stub.url(), Stdin::Bytes(stdin.clone())));
    let reqs = stub.settled();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].one("X-Chaldea-Body-Truncated"), "1");
    assert_eq!(reqs[0].one("Content-Length"), MIB4.to_string());
    assert!(reqs[0].body == stdin[..MIB4], "the first 4 MiB");
    // Exactly 4 MiB is sent whole, unmarked.
    let stub = Stub::start(&[Reply::Status(200)]);
    assert_quiet(&emit_to(&stub.url(), Stdin::Bytes(stdin[..MIB4].to_vec())));
    let reqs = stub.settled();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].all("X-Chaldea-Body-Truncated").is_empty());
    assert_eq!(reqs[0].body.len(), MIB4);
    // One byte over, closed at once: truncated.
    let stub = Stub::start(&[Reply::Status(200)]);
    assert_quiet(&emit_to(&stub.url(), Stdin::Bytes(stdin[..=MIB4].to_vec())));
    let reqs = stub.settled();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].one("X-Chaldea-Body-Truncated"), "1");
    assert!(reqs[0].body == stdin[..MIB4]);
}

/// CL-25: stdin over 4 MiB that is still open at the 1.5 s cut sends nothing, like any
/// stdin still open then.
#[test]
fn oversized_stdin_still_open_sends_nothing() {
    let stub = Stub::start(&[Reply::Status(200)]);
    let r = emit_to(
        &stub.url(),
        Stdin::LateEof(Duration::from_secs(4), vec![b'x'; MIB4 + 1]),
    );
    assert_quiet(&r);
    assert_eq!(stub.accepted(), 0);
}

/// The threads of `pid`, from `ps -M` (one line per thread after the header).
#[cfg(target_os = "macos")]
fn threads(pid: u32) -> usize {
    let mut cmd = Command::new("/bin/ps");
    cmd.args(["-M", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut c = spawn_locked(&mut cmd).unwrap();
    let out = collect(c.stdout.take());
    wait_bounded(&mut c, "ps");
    String::from_utf8_lossy(&joined(&out, "ps stdout"))
        .lines()
        .skip(1)
        .count()
}

/// `hook-emit` is dispatched before process setup (hook-ingest §2.13): while its first
/// attempt waits for a reply, the emitter runs only its main thread and the watchdog (the
/// stdin reader has ended). `prepare_process` would add the daemon's log thread.
#[cfg(target_os = "macos")]
#[test]
fn hook_emit_runs_before_process_setup() {
    warm();
    let stub = Stub::start(&[Reply::Silent]);
    let env = full_env(&stub.url());
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut cmd = emitter(&env);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut c = Killed(spawn_locked(&mut cmd).unwrap());
    c.0.stdin.take().unwrap().write_all(PAYLOAD).unwrap();
    stub.wait_reqs(1);
    // The first attempt lasts 1.8 s; the stdin thread ends right after its EOF.
    let end = Instant::now() + Duration::from_millis(1000);
    let pid = c.0.id();
    let mut seen = threads(pid);
    while seen != 2 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
        seen = threads(pid);
    }
    drop(c);
    assert_eq!(seen, 2, "threads of the emitter during its first attempt");
}

// ---- FakeCli hook emission (§2.14) ----

impl Drop for Reports {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(a) = self.accept.take() {
            let _ = a.join();
        }
    }
}

/// FakeCli, built by `cargo test --workspace` (testkit's binary) next to the test binaries'
/// `deps/` directory.
fn fake_cli() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let p = dir.join(format!("chaldea-fake-cli{}", std::env::consts::EXE_SUFFIX));
    assert!(
        p.is_file(),
        "{} is missing: build with cargo test --workspace",
        p.display()
    );
    p
}

/// A report listener: every line of every FakeCli process, as JSON. A connection is read
/// until its EOF or [`WAIT`] without a line; the accept loop ends when this is dropped.
struct Reports {
    addr: String,
    /// The events, and the first complete frame that was not JSON.
    events: Arc<Mutex<(Vec<Value>, Option<String>)>>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Reports {
    fn start() -> Reports {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        l.set_nonblocking(true).unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let events = Arc::new(Mutex::new((Vec::new(), None)));
        let stop = Arc::new(AtomicBool::new(false));
        let (ev, halt) = (events.clone(), stop.clone());
        let accept = std::thread::spawn(move || {
            while !halt.load(Ordering::SeqCst) {
                let s = match l.accept() {
                    Ok((s, _)) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(POLL);
                        continue;
                    }
                    Err(e) => panic!("report accept: {e}"),
                };
                let ev = ev.clone();
                std::thread::spawn(move || {
                    s.set_nonblocking(false).unwrap();
                    s.set_read_timeout(Some(WAIT)).unwrap();
                    // Complete frames only: an unterminated last one (a process killed while
                    // reporting) is discarded.
                    let mut r = BufReader::new(s);
                    loop {
                        let mut line = Vec::new();
                        match r.read_until(b'\n', &mut line) {
                            Ok(_) if line.ends_with(b"\n") => {}
                            _ => return,
                        }
                        let mut ev = ev.lock().unwrap();
                        match serde_json::from_slice(&line) {
                            Ok(v) => ev.0.push(v),
                            Err(_) => {
                                let frame = String::from_utf8_lossy(&line).into_owned();
                                ev.1.get_or_insert(frame);
                                return;
                            }
                        }
                    }
                });
            }
        });
        Reports {
            addr,
            events,
            stop,
            accept: Some(accept),
        }
    }

    /// The events named `event` so far; panics once a malformed frame was read.
    fn of(&self, event: &str) -> Vec<Value> {
        let ev = self.events.lock().unwrap();
        if let Some(frame) = &ev.1 {
            panic!("malformed report frame: {frame:?}");
        }
        ev.0.iter()
            .filter(|e| e["event"] == event)
            .cloned()
            .collect()
    }

    /// Waits (bounded) for `n` events named `event`.
    fn wait(&self, event: &str, n: usize) -> Vec<Value> {
        let end = Instant::now() + WAIT;
        loop {
            let e = self.of(event);
            if e.len() >= n {
                return e;
            }
            assert!(Instant::now() < end, "{event}: {e:?}");
            std::thread::sleep(POLL);
        }
    }
}

fn fake(args: &[&str], reports: &Reports, stub: &Stub) -> Command {
    warm();
    let mut c = Command::new(fake_cli());
    c.args(args)
        .args(["--emitter", EMITTER, "--report", &reports.addr])
        .args(["--max-life-secs", "60"])
        .env_remove(URL)
        .env_remove(TOKEN)
        .env_remove(THREAD)
        .envs(full_env(&stub.url()));
    c
}

/// Every emitter run that FakeCli reported exited 0 with empty outputs.
fn emitted_quietly(reports: &Reports, n: usize) {
    for e in reports.wait("emitted", n) {
        assert_eq!(e["code"], 0, "{e}");
        assert_eq!(e["stdout_len"], 0, "{e}");
        assert_eq!(e["stderr_len"], 0, "{e}");
        assert!(e["elapsed_ms"].as_u64().unwrap() < 4500, "{e}");
    }
}

/// The pids of a walk.
fn chain(r: &Req) -> Vec<u32> {
    r.ancestry
        .as_ref()
        .unwrap()
        .links
        .iter()
        .map(|l| l.identity.pid)
        .collect()
}

#[test]
fn fake_cli_echo_emits_session_start_and_prompts() {
    let stub = Stub::start_with(&[Reply::Status(200)], true);
    let reports = Reports::start();
    let mut cmd = fake(
        &[
            "--mode",
            "echo",
            "--agent",
            "claude",
            "--session-id",
            "sess-1",
        ],
        &reports,
        &stub,
    );
    let r = run(
        &mut cmd,
        Stdin::Bytes(b"hello\r\x1b[200~pasted\x1b[201~\n\nlast".to_vec()),
    );
    assert_eq!(r.code, Some(0));
    let reqs = stub.settled();
    let bodies: Vec<Value> = reqs.iter().map(Req::json).collect();
    assert_eq!(
        bodies,
        [
            json!({"hook_event_name": "SessionStart", "session_id": "sess-1", "transcript_path": null}),
            json!({"hook_event_name": "UserPromptSubmit", "session_id": "sess-1", "prompt": "hello"}),
            json!({"hook_event_name": "UserPromptSubmit", "session_id": "sess-1", "prompt": "pasted"}),
        ]
    );
    for q in &reqs {
        assert_eq!(q.one("X-Chaldea-Agent"), "claude");
        assert_eq!(q.one("X-Chaldea-Thread-Id"), THREAD_ID);
        // emitter → FakeCli → this test.
        assert_eq!(
            chain(q)[1..3],
            [r.pid, std::process::id()],
            "{:?}",
            q.ancestry
        );
        assert_eq!(
            q.ancestry.as_ref().unwrap().links[1].name,
            "chaldea-fake-cli"
        );
    }
    emitted_quietly(&reports, 3);
}

#[test]
fn fake_cli_echo_nested_emits_around_its_own_session_start() {
    let stub = Stub::start_with(&[Reply::Slow200], true);
    let reports = Reports::start();
    let mut cmd = fake(
        &["--mode", "echo", "--session-id", "own", "--nested"],
        &reports,
        &stub,
    );
    let r = run(&mut cmd, Stdin::Bytes(Vec::new()));
    assert_eq!(r.code, Some(0));
    let reqs = stub.settled();
    let sessions: Vec<String> = reqs
        .iter()
        .map(|q| q.json()["session_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(sessions.len(), 3, "{sessions:?}");
    assert_eq!(sessions[1], "own");
    assert_eq!(sessions[0], sessions[2]);
    assert_ne!(sessions[0], "own");
    // Each hook is sent only after the previous one was answered (each reply is held 300 ms).
    for w in reqs.windows(2) {
        assert!(
            w[1].at >= w[0].at + SLOW_REPLY,
            "{:?} then {:?}",
            w[0].at,
            w[1].at
        );
    }
    for (i, q) in reqs.iter().enumerate() {
        assert_eq!(q.json()["hook_event_name"], "SessionStart");
        assert_eq!(q.one("X-Chaldea-Agent"), "codex");
        let c = chain(q);
        if i == 1 {
            assert_eq!(c[1..3], [r.pid, std::process::id()], "{c:?}");
        } else {
            // emitter → nested FakeCli → FakeCli → this test.
            assert_ne!(c[1], r.pid);
            assert_eq!(c[2..4], [r.pid, std::process::id()], "{c:?}");
            assert_eq!(
                q.ancestry.as_ref().unwrap().links[1].name,
                "chaldea-fake-cli"
            );
        }
    }
    emitted_quietly(&reports, 3);
    assert_eq!(reports.wait("nested", 1)[0]["code"], 0);
}

#[test]
fn fake_cli_hooks_runs_its_script() {
    let stub = Stub::start_with(&[Reply::Status(200)], true);
    let reports = Reports::start();
    let dir = tempfile::tempdir().unwrap();
    let gate = dir.path().join("go");
    let prompt = json!({"hook_event_name": "UserPromptSubmit", "session_id": "s", "prompt": "p"});
    let start =
        json!({"hook_event_name": "SessionStart", "session_id": "s", "transcript_path": null});
    let stop = json!({"hook_event_name": "Stop", "session_id": "s"});
    let script = json!([
        {"emit": prompt},
        {"wait_file": gate.to_str().unwrap()},
        {"emit": start},
        {"concurrent": [stop, stop, "not json"]},
        {"nested": [{"emit": start}]},
    ]);
    let mut cmd = fake(
        &[
            "--mode",
            "hooks",
            "--agent",
            "codex",
            "--script",
            &script.to_string(),
        ],
        &reports,
        &stub,
    );
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut c = spawn_locked(&mut cmd).unwrap();
    let out = collect(c.stdout.take());
    let err = collect(c.stderr.take());
    let first = stub.wait_reqs(1);
    // The script holds at the gate: nothing after the first hook yet.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(stub.reqs().len(), 1, "{first:?}");
    std::fs::write(&gate, b"").unwrap();
    let status = wait_bounded(&mut c, "FakeCli hooks");
    assert_eq!(status.code(), Some(0));
    assert!(joined(&out, "stdout").is_empty());
    assert!(joined(&err, "stderr").is_empty());
    let reqs = stub.settled();
    assert_eq!(reqs.len(), 6, "{reqs:?}");
    assert_eq!(reqs[0].json(), prompt);
    assert_eq!(reqs[1].json(), start);
    let mut concurrent: Vec<&[u8]> = reqs[2..5].iter().map(|q| q.body.as_slice()).collect();
    concurrent.sort_unstable();
    let stop_bytes = stop.to_string().into_bytes();
    assert_eq!(
        concurrent,
        [b"not json".as_slice(), &stop_bytes, &stop_bytes]
    );
    let ids: std::collections::HashSet<&str> =
        reqs.iter().map(|q| q.one("X-Chaldea-Emit-Id")).collect();
    assert_eq!(ids.len(), 6, "one emit id per emitter run");
    assert_eq!(reqs[5].json(), start);
    let c5 = chain(&reqs[5]);
    assert_eq!(c5[2..4], [c.id(), std::process::id()], "{c5:?}");
    for q in &reqs[..5] {
        assert_eq!(chain(q)[1..3], [c.id(), std::process::id()]);
    }
    emitted_quietly(&reports, 6);
    assert_eq!(reports.wait("wait_file", 1)[0]["found"], true);
    assert_eq!(reports.wait("nested", 1)[0]["code"], 0);
    assert_eq!(reports.of("emitted").len(), 6);
}

#[test]
fn fake_cli_hooks_refuses_a_bad_script() {
    let stub = Stub::start(&[Reply::Status(200)]);
    let reports = Reports::start();
    let mut cmd = fake(&["--mode", "hooks", "--script", "{"], &reports, &stub);
    let r = run(&mut cmd, Stdin::Bytes(Vec::new()));
    assert_eq!(r.code, Some(2));
    assert_eq!(reports.wait("script_error", 1).len(), 1);
    assert_eq!(stub.accepted(), 0);
}

/// §2.14: the nested child emits its second hook only after the exact `go` line; on EOF or
/// another line it reports the failure and exits 1 after its first hook.
#[test]
fn fake_cli_nested_child_needs_go() {
    for input in [&b""[..], b"stop\n", b"gone\n"] {
        let stub = Stub::start(&[Reply::Status(200)]);
        let reports = Reports::start();
        let mut cmd = fake(&["--mode", "echo", "--nested-child"], &reports, &stub);
        let r = run(&mut cmd, Stdin::Bytes(input.to_vec()));
        let what = String::from_utf8_lossy(input);
        assert_eq!(r.code, Some(1), "{what:?}");
        assert_eq!(stub.settled().len(), 1, "{what:?}");
        assert_eq!(reports.wait("handshake_failed", 1).len(), 1, "{what:?}");
        assert_eq!(reports.of("emitted").len(), 1, "{what:?}");
    }
}

/// §2.14: a parent whose nested child ends before `sent` reports the failure, emits nothing
/// of its own, and exits 1. The emitter is `/bin/sh` running `hook-emit` from the working
/// directory, a script that kills its parent: the nested child, at its first hook.
#[cfg(target_os = "macos")]
#[test]
fn fake_cli_nested_parent_needs_sent() {
    let stub = Stub::start(&[Reply::Status(200)]);
    let reports = Reports::start();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hook-emit"), "kill -9 $PPID\n").unwrap();
    let mut cmd = fake(&["--mode", "echo", "--nested"], &reports, &stub);
    // The last `--emitter` wins.
    cmd.args(["--emitter", "/bin/sh"]).current_dir(dir.path());
    let r = run(&mut cmd, Stdin::Bytes(Vec::new()));
    assert_eq!(r.code, Some(1));
    let failed = reports.wait("handshake_failed", 1);
    assert_eq!(failed[0]["reason"], "eof", "{failed:?}");
    // No run of the parent; the nested child's own run killed it before its report.
    let emitted = reports.of("emitted");
    assert!(emitted.is_empty(), "{emitted:?}");
    assert_eq!(stub.accepted(), 0);
}

/// §2.14 `concurrent`: every emitter starts before any is answered. Each first attempt gets
/// no reply, so a serialized step would send a retry before the next first attempt.
#[test]
fn concurrent_hooks_start_before_any_reply() {
    let stub = Stub::start(&[Reply::Silent]);
    let reports = Reports::start();
    let script = r#"[{"concurrent":[{"n":1},{"n":2},{"n":3}]}]"#;
    let mut cmd = fake(&["--mode", "hooks", "--script", script], &reports, &stub);
    let r = run(&mut cmd, Stdin::Bytes(Vec::new()));
    assert_eq!(r.code, Some(0));
    let reqs = stub.settled();
    let first = |q: &&Req| q.one("X-Chaldea-Attempt") == "1";
    let mut firsts: Vec<u64> = reqs
        .iter()
        .filter(first)
        .map(|q| q.json()["n"].as_u64().unwrap())
        .collect();
    firsts.sort_unstable();
    assert_eq!(firsts, [1, 2, 3]);
    let first_retry = reqs
        .iter()
        .filter(|q| q.one("X-Chaldea-Attempt") == "2")
        .map(|q| q.at)
        .min()
        .expect("a silent first attempt is retried");
    assert!(
        reqs.iter().filter(first).all(|q| q.at < first_retry),
        "{reqs:?}"
    );
}

/// §2.14: FakeCli reports the bytes an emitter wrote. `/bin/sh` as the emitter finds no
/// `hook-emit` script in the working directory, says so on stderr, and exits 127.
#[cfg(target_os = "macos")]
#[test]
fn fake_cli_reports_an_emitters_output() {
    let reports = Reports::start();
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(fake_cli());
    cmd.args(["--mode", "hooks", "--emitter", "/bin/sh"])
        .args(["--script", r#"[{"emit":{}}]"#, "--report", &reports.addr])
        .args(["--max-life-secs", "60"])
        .current_dir(dir.path());
    let r = run(&mut cmd, Stdin::Bytes(Vec::new()));
    assert_eq!(r.code, Some(0));
    let e = &reports.wait("emitted", 1)[0];
    assert_eq!(e["code"], 127, "{e}");
    assert_eq!(e["stdout_len"], 0, "{e}");
    assert!(e["stderr_len"].as_u64().unwrap() > 0, "{e}");
}

/// §2.14: a handshake line counts only with its LF: `go` cut by EOF is EOF.
#[test]
fn nested_child_rejects_unterminated_go() {
    let stub = Stub::start(&[Reply::Status(200)]);
    let reports = Reports::start();
    let mut cmd = fake(&["--mode", "echo", "--nested-child"], &reports, &stub);
    let r = run(&mut cmd, Stdin::Bytes(b"go".to_vec()));
    assert_eq!((r.code, stub.settled().len()), (Some(1), 1));
    assert_eq!(reports.wait("handshake_failed", 1)[0]["reason"], "eof");
}

/// §2.14: an emitter that cannot be started fails the nested sequence: the child reports it
/// and exits 1 before `sent`, and the parent, seeing EOF, exits 1 too.
#[test]
fn nested_echo_fails_when_its_emitter_cannot_spawn() {
    let stub = Stub::start(&[Reply::Status(200)]);
    let reports = Reports::start();
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = fake(&["--mode", "echo", "--nested"], &reports, &stub);
    cmd.arg("--emitter").arg(dir.path().join("absent-emitter"));
    let r = run(&mut cmd, Stdin::Bytes(Vec::new()));
    assert_eq!(reports.wait("emitted", 1)[0]["spawn_failed"], true);
    assert_eq!((r.code, stub.accepted()), (Some(1), 0));
    // The child alone: no `sent` after its failed first hook.
    let reports = Reports::start();
    let mut cmd = fake(&["--mode", "echo", "--nested-child"], &reports, &stub);
    cmd.arg("--emitter").arg(dir.path().join("absent-emitter"));
    let r = run(&mut cmd, Stdin::Bytes(b"go\n".to_vec()));
    assert_eq!(reports.wait("emitted", 1)[0]["spawn_failed"], true);
    assert_eq!(r.code, Some(1));
    assert!(
        r.stdout.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&r.stdout)
    );
}

/// §2.14: a parent whose own emitter cannot be started writes no `go`. The emitter is a
/// script that deletes itself, so the nested child's run starts it and the parent's cannot.
#[cfg(target_os = "macos")]
#[test]
fn fake_cli_nested_parent_stops_when_its_emitter_cannot_spawn() {
    use std::os::unix::fs::PermissionsExt;
    let stub = Stub::start(&[Reply::Status(200)]);
    let reports = Reports::start();
    let dir = tempfile::tempdir().unwrap();
    let emitter = dir.path().join("once");
    std::fs::write(&emitter, "#!/bin/sh\nrm -- \"$0\"\n").unwrap();
    std::fs::set_permissions(&emitter, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut cmd = fake(&["--mode", "echo", "--nested"], &reports, &stub);
    cmd.arg("--emitter").arg(&emitter);
    let r = run(&mut cmd, Stdin::Bytes(Vec::new()));
    assert_eq!(r.code, Some(1));
    let emitted = reports.wait("emitted", 2);
    assert_eq!(emitted[0]["code"], 0, "{emitted:?}");
    assert_eq!(emitted[1]["spawn_failed"], true, "{emitted:?}");
    // No `go`: the parent ended the child, which emitted nothing more.
    reports.wait("nested", 1);
    assert_eq!(reports.of("emitted").len(), 2);
}
