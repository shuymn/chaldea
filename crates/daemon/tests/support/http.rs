//! The HTTP test harness (`docs/design/http-surface.md` §7): a server on a `127.0.0.1:0`
//! listener over a temp store, a recording lifecycle sink, a skewable clock, token helpers,
//! and a raw HTTP/1.1 client over `TcpStream`, so trickled or malformed heads, withheld or
//! in-flight bodies, and EOF are observable exactly.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chaldea_core::auth::TokenKind;
use chaldea_core::http::ErrorBody;
use chaldea_core::ids::{LaunchId, ThreadId};
use chaldea_daemon::http::{
    HttpConfig, HttpServer, HttpShutdownReport, LifecycleSink, MonotonicClock, Mounts, ServeHandle,
    StoreFatal, TokenService,
};
use chaldea_daemon::store::{NewEvent, TokenSelector};
use sha2::Digest as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{TestStore, registered, reserved, soon, started};

/// The short deadline of test servers.
pub const DEADLINE: Duration = Duration::from_secs(2);
/// The header read timeout of test servers.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a raw client waits for a whole reply.
pub const CLIENT_WAIT: Duration = Duration::from_secs(15);

/// Records every fatal store outcome.
#[derive(Default)]
pub struct RecordingSink(pub Mutex<Vec<StoreFatal>>);

impl LifecycleSink for RecordingSink {
    fn store_fatal(&self, f: StoreFatal) {
        self.0.lock().unwrap().push(f);
    }
}

/// The real monotonic clock plus a settable offset.
#[derive(Default)]
pub struct SkewClock {
    offset_ms: AtomicU64,
}

impl SkewClock {
    pub fn skew(&self, by: Duration) {
        self.offset_ms
            .store(u64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
    }
}

impl SkewClock {
    fn offset(&self) -> Duration {
        Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
    }
}

impl MonotonicClock for SkewClock {
    fn now(&self) -> Instant {
        Instant::now() + self.offset()
    }

    /// `at` less the current offset (an instant before the platform's earliest is long past).
    fn real(&self, at: Instant) -> Instant {
        at.checked_sub(self.offset()).unwrap_or_else(Instant::now)
    }
}

/// A served `HttpServer` over a temp store.
pub struct TestServer {
    pub server: HttpServer,
    pub port: u16,
    pub sink: Arc<RecordingSink>,
    pub clock: Arc<SkewClock>,
    handle: Option<ServeHandle>,
    // Dropped last: the store is shut down before its directory is removed.
    pub store: TestStore,
}

impl TestServer {
    /// A server over `mounts` with the test configuration.
    pub async fn start(mounts: Mounts) -> TestServer {
        Self::start_with(mounts, |_| {}).await
    }

    /// As [`TestServer::start`], with the configuration adjusted by `tune`.
    pub async fn start_with(mounts: Mounts, tune: impl FnOnce(&mut HttpConfig)) -> TestServer {
        Self::start_on(TestStore::new(), mounts, tune).await
    }

    /// As [`TestServer::start_with`], over the given store.
    pub async fn start_on(
        store: TestStore,
        mounts: Mounts,
        tune: impl FnOnce(&mut HttpConfig),
    ) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let clock = Arc::new(SkewClock::default());
        let mut cfg = HttpConfig::new(port);
        cfg.short_deadline = DEADLINE;
        cfg.header_read_timeout = HEAD_TIMEOUT;
        cfg.write_stall_timeout = Duration::from_secs(5);
        cfg.clock = clock.clone();
        tune(&mut cfg);
        let sink = Arc::new(RecordingSink::default());
        let server = HttpServer::new(cfg, store.store().clone(), sink.clone(), mounts);
        let handle = server.serve(listener);
        TestServer {
            server,
            port,
            sink,
            clock,
            handle: Some(handle),
            store,
        }
    }

    /// Shuts the server down (deadline in 10 s), then the store.
    pub async fn stop(mut self) -> HttpShutdownReport {
        let report = self
            .shutdown_http(Instant::now() + Duration::from_secs(10))
            .await;
        self.store.shutdown();
        report
    }

    /// Shuts only the server down.
    pub async fn shutdown_http(&mut self, deadline: Instant) -> HttpShutdownReport {
        self.handle.take().expect("served").shutdown(deadline).await
    }

    /// Appends events in one job.
    pub async fn append(&self, events: Vec<NewEvent>) {
        self.store
            .store()
            .submit(soon(), move |tx| Ok(tx.append(events)?))
            .unwrap()
            .await
            .unwrap();
    }

    /// Inserts a token of `kind` and returns its secret.
    pub async fn token(
        &self,
        kind: TokenKind,
        thread: Option<ThreadId>,
        launch: Option<LaunchId>,
    ) -> String {
        let m = self.server.tokens().mint();
        let row = TokenService::new_token(&m, kind, thread, launch);
        self.store
            .store()
            .submit(soon(), move |tx| Ok(tx.token_insert(row)?))
            .unwrap()
            .await
            .unwrap();
        m.secret.expose().to_owned()
    }

    /// Registers thread `t` with a started launch `l` and returns an agent token for it.
    pub async fn agent(&self, t: ThreadId, l: LaunchId) -> String {
        self.append(vec![registered(t, false)]).await;
        self.append(vec![reserved(t, l), started(t, l)]).await;
        self.token(TokenKind::Agent, Some(t), Some(l)).await
    }

    /// A token of `kind`: an agent token gets its own thread and launch.
    pub async fn token_of(&self, kind: TokenKind, n: u128) -> String {
        match kind {
            TokenKind::Agent => {
                self.agent(
                    ThreadId::from_u128(0xA000 + n),
                    LaunchId::from_u128(0xB000 + n),
                )
                .await
            }
            k => self.token(k, None, None).await,
        }
    }

    /// Revokes one token by its secret.
    pub async fn revoke(&self, secret: &str) {
        let sha: [u8; 32] = sha2::Sha256::digest(secret.as_bytes()).into();
        let row = self
            .store
            .read(move |tx| tx.token_by_hash(&sha))
            .expect("token exists");
        let n = self
            .server
            .tokens()
            .revoke(
                TokenSelector::Token(row.token_id),
                Instant::now() + DEADLINE,
            )
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    /// A request builder for this server.
    #[must_use]
    pub fn req(&self, method: &str, path: &str) -> Req {
        Req::new(self.port, method, path)
    }

    /// Sends `req` on a new connection and reads the reply to EOF.
    pub async fn send(&self, req: &Req) -> Reply {
        send(self.port, &req.bytes()).await
    }
}

/// A raw HTTP/1.1 request.
#[derive(Clone)]
pub struct Req {
    method: String,
    target: String,
    host: Option<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    length: bool,
}

impl Req {
    #[must_use]
    pub fn new(port: u16, method: &str, target: &str) -> Req {
        Req {
            method: method.to_owned(),
            target: target.to_owned(),
            host: Some(format!("127.0.0.1:{port}")),
            headers: Vec::new(),
            body: Vec::new(),
            length: method == "POST",
        }
    }

    #[must_use]
    pub fn host(mut self, h: Option<&str>) -> Req {
        self.host = h.map(str::to_owned);
        self
    }

    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Req {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    #[must_use]
    pub fn bearer(self, token: &str) -> Req {
        self.header("Authorization", &format!("Bearer {token}"))
    }

    #[must_use]
    pub fn body(mut self, b: &[u8]) -> Req {
        self.body = b.to_vec();
        self
    }

    /// Sends no automatic `Content-Length`.
    #[must_use]
    pub fn no_length(mut self) -> Req {
        self.length = false;
        self
    }

    /// The head only, ending with the blank line.
    #[must_use]
    pub fn head(&self) -> Vec<u8> {
        let mut s = format!("{} {} HTTP/1.1\r\n", self.method, self.target);
        if let Some(h) = &self.host {
            s.push_str(&format!("Host: {h}\r\n"));
        }
        for (k, v) in &self.headers {
            s.push_str(&format!("{k}: {v}\r\n"));
        }
        if self.length {
            s.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }
        s.push_str("\r\n");
        s.into_bytes()
    }

    #[must_use]
    pub fn bytes(&self) -> Vec<u8> {
        let mut b = self.head();
        b.extend_from_slice(&self.body);
        b
    }

    #[must_use]
    pub fn body_bytes(&self) -> &[u8] {
        &self.body
    }
}

/// One parsed reply, plus whatever followed it on the connection.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Bytes after the first reply (a second reply would be here).
    pub rest: Vec<u8>,
    /// How the read ended: EOF (`None`) or an error kind (a reset on Windows).
    pub end: Option<io::ErrorKind>,
}

impl Reply {
    /// The single value of header `name` (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let mut v = self.headers_named(name).into_iter();
        let first = v.next();
        assert!(
            v.next().is_none(),
            "header {name} repeated: {:?}",
            self.headers
        );
        first
    }

    #[must_use]
    pub fn headers_named(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// The `ErrorBody` code, if the body is one.
    #[must_use]
    pub fn code(&self) -> Option<String> {
        serde_json::from_slice::<ErrorBody>(&self.body)
            .ok()
            .map(|b| {
                serde_json::to_value(b.code)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
    }

    /// Status and code, for compact assertions.
    #[must_use]
    pub fn outcome(&self) -> (u16, Option<String>) {
        (self.status, self.code())
    }
}

/// Reads from `s` until EOF, an error, or [`CLIENT_WAIT`].
pub async fn read_to_end(s: &mut TcpStream) -> (Vec<u8>, Option<io::ErrorKind>) {
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let until = tokio::time::Instant::now() + CLIENT_WAIT;
    loop {
        match tokio::time::timeout_at(until, s.read(&mut buf)).await {
            Err(_) => panic!(
                "no EOF within {CLIENT_WAIT:?}; read so far: {:?}",
                String::from_utf8_lossy(&out)
            ),
            Ok(Ok(0)) => return (out, None),
            Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
            Ok(Err(e)) => return (out, Some(e.kind())),
        }
    }
}

/// Parses the first reply in `bytes`.
///
/// # Panics
///
/// When `bytes` holds no complete head.
#[must_use]
pub fn parse(bytes: &[u8], end: Option<io::ErrorKind>) -> Reply {
    let text_end = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| {
            panic!(
                "no complete reply head (connection ended with {end:?}): {:?}",
                String::from_utf8_lossy(bytes)
            )
        });
    let head = String::from_utf8_lossy(&bytes[..text_end]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap();
    let status: u16 = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("bad status line {status_line:?}"));
    let headers: Vec<(String, String)> = lines
        .map(|l| {
            let (k, v) = l.split_once(':').unwrap();
            (k.trim().to_owned(), v.trim().to_owned())
        })
        .collect();
    let after = &bytes[text_end + 4..];
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map_or(after.len(), |(_, v)| {
            v.parse::<usize>().unwrap().min(after.len())
        });
    Reply {
        status,
        headers,
        body: after[..len].to_vec(),
        rest: after[len..].to_vec(),
        end,
    }
}

/// Connects, writes `bytes`, and reads the reply to EOF.
pub async fn send(port: u16, bytes: &[u8]) -> Reply {
    let mut s = connect(port).await;
    s.write_all(bytes).await.unwrap();
    let (out, end) = read_to_end(&mut s).await;
    parse(&out, end)
}

pub async fn connect(port: u16) -> TcpStream {
    TcpStream::connect(("127.0.0.1", port)).await.unwrap()
}

/// Every row of every table of the store at `path`, through a separate read-only
/// connection (WAL readers see the committed state).
#[must_use]
pub fn dump(path: &std::path::Path) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
    let c = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    let mut names: Vec<String> = c
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let rows = super::table_rows(&c, &n);
            (n, rows)
        })
        .collect()
}
