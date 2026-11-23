//! The HTTP test harness (`docs/design/http-surface.md` §7): a server on a `127.0.0.1:0`
//! listener over a temp store, a recording lifecycle sink, a skewable clock, token helpers,
//! and a raw HTTP/1.1 client over `TcpStream`, so trickled or malformed heads, withheld or
//! in-flight bodies, and EOF are observable exactly.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chaldea_core::auth::TokenKind;
use chaldea_core::command::CommandReceipt;
use chaldea_core::http::{ErrorBody, PairCodeResponse, WsTicketResponse};
use chaldea_core::ids::{CommandId, LaunchId, StreamId, ThreadId};
use chaldea_core::sync::{StreamHeadEntry, WsServerMessage};
use chaldea_daemon::http::{
    CommandRegistry, HttpConfig, HttpServer, HttpShutdownReport, LifecycleSink, MonotonicClock,
    Mounts, ServeHandle, StoreFatal, TokenService,
};
use chaldea_daemon::store::{NewEvent, TokenSelector, WriteError, WriteTicket};
use futures_util::{SinkExt, StreamExt};
use sha2::Digest as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::{TestStore, registered, reserved, soon, started};

/// The short deadline of test servers.
pub const DEADLINE: Duration = Duration::from_secs(2);
/// The header read timeout of test servers.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a raw client waits for a whole reply.
pub const CLIENT_WAIT: Duration = Duration::from_secs(15);
/// The WebSocket heads interval of test servers.
pub const HEADS_INTERVAL: Duration = Duration::from_millis(200);
/// How long a WebSocket client waits for one frame or the close.
pub const WS_WAIT: Duration = Duration::from_secs(10);

/// Records every fatal store outcome.
#[derive(Default)]
pub struct RecordingSink(pub Mutex<Vec<StoreFatal>>);

impl LifecycleSink for RecordingSink {
    fn store_fatal(&self, f: StoreFatal) {
        self.0.lock().unwrap().push(f);
    }
}

/// The real monotonic clock plus a settable offset, frozen at one instant, or stopped.
#[derive(Default)]
pub struct SkewClock {
    offset_ms: AtomicU64,
    /// While set, the clock stands still.
    held: Mutex<Option<Held>>,
}

/// A clock that stands still: frozen (real time keeps running for later readings) or
/// stopped (no later reading arrives).
#[derive(Clone, Copy)]
struct Held {
    /// The instant `now` returns.
    at: Instant,
    /// The real instant it was set at.
    real: Instant,
    stopped: bool,
}

/// How far past the real stop instant a stopped clock puts every later reading: beyond
/// every bound a test waits for.
const NEVER: Duration = Duration::from_secs(24 * 60 * 60);

impl SkewClock {
    pub fn skew(&self, by: Duration) {
        self.offset_ms
            .store(u64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
    }

    /// Stops the clock at `at`; real time keeps running for timers.
    pub fn freeze(&self, at: Instant) {
        *self.held.lock().unwrap() = Some(Held {
            at,
            real: Instant::now(),
            stopped: false,
        });
    }

    /// Stops the clock at its current reading for good: no later reading arrives, so a
    /// deadline taken from this clock never passes and its timers do not fire while the
    /// test runs. Timers of their own (hyper's header read, the write stall, the raw
    /// client's waits) still run.
    pub fn stop(&self) {
        let mut held = self.held.lock().unwrap();
        let at = held.map_or_else(|| Instant::now() + self.offset(), |h| h.at);
        *held = Some(Held {
            at,
            real: Instant::now(),
            stopped: true,
        });
    }

    fn offset(&self) -> Duration {
        Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
    }
}

impl MonotonicClock for SkewClock {
    fn now(&self) -> Instant {
        match *self.held.lock().unwrap() {
            Some(h) => h.at,
            None => Instant::now() + self.offset(),
        }
    }

    /// `at` less the current offset; held, the real instant as far from the real set
    /// instant as `at` is from the held reading, except that a stopped clock puts every
    /// later `at` at [`NEVER`] past its real stop instant. When that is before the
    /// platform's earliest instant it is long past, and the fixture reads `Instant::now()`
    /// instead.
    fn real(&self, at: Instant) -> Instant {
        match *self.held.lock().unwrap() {
            Some(h) if h.stopped && at > h.at => h.real + NEVER,
            Some(h) if at >= h.at => h.real + (at - h.at),
            Some(h) => h.real.checked_sub(h.at - at).unwrap_or_else(Instant::now),
            None => at.checked_sub(self.offset()).unwrap_or_else(Instant::now),
        }
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
        Self::start_full(store, mounts, CommandRegistry::mvp(), tune).await
    }

    /// As [`TestServer::start_on`], with the command handlers of `commands`.
    pub async fn start_full(
        store: TestStore,
        mounts: Mounts,
        commands: CommandRegistry,
        tune: impl FnOnce(&mut HttpConfig),
    ) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let clock = Arc::new(SkewClock::default());
        let mut cfg = HttpConfig::new(port);
        cfg.short_deadline = DEADLINE;
        cfg.header_read_timeout = HEAD_TIMEOUT;
        cfg.write_stall_timeout = Duration::from_secs(5);
        cfg.ws.heads_interval = HEADS_INTERVAL;
        cfg.clock = clock.clone();
        tune(&mut cfg);
        let sink = Arc::new(RecordingSink::default());
        let server = HttpServer::new(cfg, store.store().clone(), commands, sink.clone(), mounts);
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

    /// A server with the test command handlers of [`super::commands`] over a new store.
    pub async fn with_commands(
        tune: impl FnOnce(&mut HttpConfig),
    ) -> (TestServer, Arc<super::commands::Probe>) {
        Self::with_commands_on(TestStore::new(), tune).await
    }

    /// As [`TestServer::with_commands`], over the given store.
    pub async fn with_commands_on(
        store: TestStore,
        tune: impl FnOnce(&mut HttpConfig),
    ) -> (TestServer, Arc<super::commands::Probe>) {
        let probe = Arc::new(super::commands::Probe::default());
        let commands = super::commands::registry(store.store(), &probe);
        let srv = Self::start_full(store, Mounts::mvp(), commands, tune).await;
        (srv, probe)
    }

    /// `POST /api/commands/{name}` with `command_id` and the raw `args` text.
    #[must_use]
    pub fn command(&self, name: &str, secret: &str, command_id: CommandId, args: &str) -> Req {
        self.req("POST", &format!("/api/commands/{name}"))
            .bearer(secret)
            .body(format!(r#"{{"command_id":"{command_id}","args":{args}}}"#).as_bytes())
    }

    /// The receipt of `command_id`, if any.
    #[must_use]
    pub fn receipt(&self, command_id: CommandId) -> Option<CommandReceipt> {
        self.store.read(move |tx| tx.receipt(command_id))
    }

    /// Shuts the server down (deadline in 10 s), then the store.
    pub async fn stop(mut self) -> HttpShutdownReport {
        let report = self
            .shutdown_http(Instant::now() + Duration::from_secs(10))
            .await;
        self.store.shutdown();
        report
    }

    /// Shuts the server down (deadline `within` from now) and returns its report and the
    /// store, still open, for a server started after it.
    pub async fn into_store(mut self, within: Duration) -> (HttpShutdownReport, TestStore) {
        let report = self.shutdown_http(Instant::now() + within).await;
        (report, self.store)
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
            .map(settled)
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
            .map(settled)
            .unwrap()
            .await
            .unwrap();
        m.secret.expose().to_owned()
    }

    /// Registers thread `t` with a started launch `l` and returns an agent token for it.
    pub async fn agent(&self, t: ThreadId, l: LaunchId) -> String {
        self.started_token(TokenKind::Agent, t, l).await
    }

    /// Registers thread `t` with a started launch `l` and returns a hook token scoped to
    /// both (`docs/design/hook-ingest.md` §2.8).
    pub async fn hook(&self, t: ThreadId, l: LaunchId) -> String {
        self.started_token(TokenKind::Hook, t, l).await
    }

    /// Registers thread `t`, then reserves and starts launch `l` (two jobs), and returns a
    /// token of `kind` scoped to both.
    async fn started_token(&self, kind: TokenKind, t: ThreadId, l: LaunchId) -> String {
        self.append(vec![registered(t, false)]).await;
        self.append(vec![reserved(t, l), started(t, l)]).await;
        self.token(kind, Some(t), Some(l)).await
    }

    /// A token of `kind`: an agent or hook token gets its own thread and started launch.
    pub async fn token_of(&self, kind: TokenKind, n: u128) -> String {
        match kind {
            TokenKind::Agent => {
                self.agent(
                    ThreadId::from_u128(0xA000 + n),
                    LaunchId::from_u128(0xB000 + n),
                )
                .await
            }
            TokenKind::Hook => {
                self.hook(
                    ThreadId::from_u128(0xC000 + n),
                    LaunchId::from_u128(0xD000 + n),
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

    /// Occupies the store writer until the returned sender is used or dropped.
    pub async fn hold_writer(&self) -> std::sync::mpsc::Sender<()> {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let ticket = self
            .store
            .store()
            .submit(soon(), move |_tx| {
                let _ = entered_tx.send(());
                let _ = wait.recv_timeout(Duration::from_secs(20));
                Ok(())
            })
            .unwrap();
        tokio::spawn(ticket);
        tokio::time::timeout(Duration::from_secs(10), entered)
            .await
            .expect("the writer runs the holding job within 10 s")
            .unwrap();
        release
    }

    /// Holds `n` readers of the pool (all of them, for the default pool) until the returned
    /// sender drops.
    pub fn hold_readers(&self, n: usize) -> std::sync::mpsc::Sender<()> {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
        for _ in 0..n {
            let store = self.store.store().clone();
            let (held, release) = (held_tx.clone(), release_rx.clone());
            std::thread::spawn(move || {
                let _ = store.read(Instant::now() + Duration::from_secs(10), move |_tx| {
                    held.send(()).unwrap();
                    // Released when the sender drops; every holder then returns.
                    let _ = release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(30));
                    Ok(())
                });
            });
        }
        for _ in 0..n {
            held_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        release_tx
    }

    /// Mints a WebSocket ticket for `purpose` with the token `secret`, or the reply.
    pub async fn ticket(&self, secret: &str, purpose: &str) -> Result<String, Reply> {
        let r = self
            .send(
                &self
                    .req("POST", "/api/ws-ticket")
                    .bearer(secret)
                    .body(format!(r#"{{"purpose":"{purpose}"}}"#).as_bytes()),
            )
            .await;
        if r.status != 200 {
            return Err(r);
        }
        let body: WsTicketResponse = serde_json::from_slice(&r.body).unwrap();
        Ok(body.ticket)
    }

    /// Opens `GET /api/ws` with a fresh `events` ticket of `secret` and reads `hello`.
    pub async fn ws(&self, secret: &str) -> WsClient {
        let ticket = self.ticket(secret, "events").await.expect("a ticket");
        let mut c = self.ws_with(&ticket).await.expect("a 101");
        match c.next().await {
            Some(WsServerMessage::Hello { epoch, .. }) => {
                assert_eq!(
                    &epoch,
                    self.store.store().epoch(),
                    "hello carries the epoch"
                );
            }
            other => panic!("expected hello, got {other:?}"),
        }
        c
    }

    /// Opens `GET /api/ws` with `ticket`; the handshake's error otherwise. The handshake
    /// ends within [`WS_WAIT`] or the test fails.
    pub async fn ws_with(
        &self,
        ticket: &str,
    ) -> Result<WsClient, tokio_tungstenite::tungstenite::Error> {
        let url = format!("ws://127.0.0.1:{}/api/ws?ticket={ticket}", self.port);
        let (ws, resp) = tokio::time::timeout(WS_WAIT, tokio_tungstenite::connect_async(url))
            .await
            .expect("the WebSocket handshake ends within the wait")?;
        assert_eq!(resp.status(), 101);
        Ok(WsClient { ws, closed: None })
    }

    /// A pair code minted with the token `secret`.
    pub async fn pair_code(&self, secret: &str) -> Result<String, Reply> {
        let r = self
            .send(
                &self
                    .req("POST", "/api/pair-codes")
                    .bearer(secret)
                    .body(b"{}"),
            )
            .await;
        if r.status != 200 {
            return Err(r);
        }
        let body: PairCodeResponse = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(body.expires_in, 60);
        Ok(body.code)
    }

    /// `POST /api/pair` with `code` and a matching `Origin`.
    pub async fn pair(&self, code: &str) -> Reply {
        self.send(&self.pair_req(code)).await
    }

    /// The `POST /api/pair` request for `code`, with a matching `Origin`.
    #[must_use]
    pub fn pair_req(&self, code: &str) -> Req {
        self.req("POST", "/api/pair")
            .header("Origin", &format!("http://127.0.0.1:{}", self.port))
            .body(format!(r#"{{"code":"{code}"}}"#).as_bytes())
    }

    /// The token row stored under the secret's digest.
    #[must_use]
    pub fn token_row(&self, secret: &str) -> Option<chaldea_daemon::store::TokenRow> {
        let sha: [u8; 32] = sha2::Sha256::digest(secret.as_bytes()).into();
        self.store.read(move |tx| tx.token_by_hash(&sha))
    }

    /// Revokes one token by its secret in a write of its own, without the nudge.
    pub async fn revoke_quietly(&self, secret: &str) {
        let id = self.token_row(secret).expect("token exists").token_id;
        let n = self
            .store
            .store()
            .submit(soon(), move |tx| {
                Ok(tx.token_revoke(TokenSelector::Token(id))?)
            })
            .map(settled)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    /// A request builder for this server.
    #[must_use]
    pub fn req(&self, method: &str, path: &str) -> Req {
        Req::new(self.port, method, path)
    }

    /// A raw WebSocket handshake for `target`, with [`ws_key`].
    #[must_use]
    pub fn ws_handshake(&self, target: &str) -> Req {
        self.req("GET", target)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", &ws_key())
    }

    /// Sends `req` on a new connection and reads the reply to EOF.
    pub async fn send(&self, req: &Req) -> Reply {
        send(self.port, &req.bytes()).await
    }
}

/// A WebSocket client for `GET /api/ws`.
pub struct WsClient {
    pub ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    /// The close code received, once the server closed.
    pub closed: Option<Option<u16>>,
}

impl WsClient {
    /// Sends `subscribe` with `ids` as given.
    pub async fn subscribe(&mut self, ids: &[StreamId]) {
        let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
        self.text(&serde_json::json!({"type": "subscribe", "stream_ids": ids}).to_string())
            .await;
    }

    pub async fn text(&mut self, t: &str) {
        self.ws
            .send(WsMessage::Text(t.to_owned().into()))
            .await
            .unwrap();
    }

    /// The next server message within [`WS_WAIT`], or `None` once the server closed or the
    /// connection ended (the close code, if any, in `closed`). Pings are answered by the
    /// client library and skipped.
    pub async fn next(&mut self) -> Option<WsServerMessage> {
        loop {
            let m = tokio::time::timeout(WS_WAIT, self.ws.next())
                .await
                .expect("a frame or the close within the wait");
            match m {
                Some(Ok(WsMessage::Text(t))) => {
                    return Some(
                        serde_json::from_str(t.as_str())
                            .unwrap_or_else(|e| panic!("not a WsServerMessage ({e}): {t}")),
                    );
                }
                Some(Ok(WsMessage::Close(f))) => {
                    self.closed = Some(f.map(|f| u16::from(f.code)));
                    return None;
                }
                Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => {}
                Some(Ok(other)) => panic!("unexpected frame {other:?}"),
                Some(Err(_)) | None => {
                    self.closed.get_or_insert(None);
                    return None;
                }
            }
        }
    }

    /// The next `heads` frame whose streams are exactly `want` (frames before it skipped,
    /// each checked by `seen`).
    pub async fn heads_for(
        &mut self,
        want: &[StreamId],
        mut seen: impl FnMut(&WsServerMessage),
    ) -> Vec<StreamHeadEntry> {
        let mut want: Vec<StreamId> = want.to_vec();
        want.sort();
        loop {
            let m = self.next().await.expect("open");
            seen(&m);
            if let WsServerMessage::Heads { heads } = &m {
                let mut got: Vec<StreamId> = heads.iter().map(|h| h.stream_id).collect();
                got.sort();
                if got == want {
                    return heads.clone();
                }
            }
        }
    }

    /// Reads until the server closes; returns the close code (if a close frame came) and
    /// the messages before it.
    pub async fn until_closed(&mut self) -> (Option<u16>, Vec<WsServerMessage>) {
        let mut before = Vec::new();
        while let Some(m) = self.next().await {
            before.push(m);
        }
        (self.closed.flatten(), before)
    }
}

/// A `Sec-WebSocket-Key`: the sample nonce of RFC 6455 §1.3, encoded here.
#[must_use]
pub fn ws_key() -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b"the sample nonce")
}

/// Waits until `cond` holds, polling with yields, at most `within`.
pub async fn eventually(within: Duration, what: &str, mut cond: impl FnMut() -> bool) {
    let until = Instant::now() + within;
    while !cond() {
        assert!(Instant::now() < until, "not within {within:?}: {what}");
        tokio::task::yield_now().await;
    }
}

/// Sends `req` on its own task; the reply is the task's output.
pub fn spawn_send(srv: &TestServer, req: &Req) -> tokio::task::JoinHandle<Reply> {
    let (port, bytes) = (srv.port, req.bytes());
    tokio::spawn(async move { send(port, &bytes).await })
}

/// Waits (at most 10 s) for a message on a gate's `reached` receiver.
pub async fn reached(rx: tokio::sync::oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("the gate was reached within 10 s")
        .unwrap();
}

/// Waits (at most 10 s) for a message on a gate's `reached` receiver from a writer thread.
pub async fn reached_sync(rx: std::sync::mpsc::Receiver<()>) {
    tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .expect("the gate was reached within 10 s");
}

/// Events on the host stream carrying `id`.
#[must_use]
pub fn host_events_of(srv: &TestServer, id: CommandId) -> usize {
    srv.store
        .read(|tx| tx.events_after(StreamId::Host, 0, 1000))
        .events
        .iter()
        .filter(|e| e.command_id == Some(id))
        .count()
}

/// The head of `stream` in `heads`.
#[must_use]
pub fn head_of(heads: &[StreamHeadEntry], stream: StreamId) -> Option<u64> {
    heads
        .iter()
        .find(|h| h.stream_id == stream)
        .map(|h| h.head_seq)
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

    /// [`Req::bytes`] with the request line's version `HTTP/1.0`.
    #[must_use]
    pub fn http10_bytes(&self) -> Vec<u8> {
        let mut b = self.bytes();
        let line_end = b.windows(2).position(|w| w == b"\r\n").unwrap();
        b.splice(line_end - 8..line_end, b"HTTP/1.0".iter().copied());
        b
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
    write(&mut s, bytes).await;
    let (out, end) = read_to_end(&mut s).await;
    parse(&out, end)
}

/// Awaits a write's ticket within [`CLIENT_WAIT`].
pub async fn settled<R>(t: WriteTicket<R>) -> Result<R, WriteError> {
    tokio::time::timeout(CLIENT_WAIT, t)
        .await
        .expect("the writer replied within the client wait")
}

/// Connects within [`CLIENT_WAIT`].
pub async fn connect(port: u16) -> TcpStream {
    tokio::time::timeout(CLIENT_WAIT, TcpStream::connect(("127.0.0.1", port)))
        .await
        .expect("connected within the client wait")
        .unwrap()
}

/// Writes all of `bytes` within [`CLIENT_WAIT`].
pub async fn write(s: &mut TcpStream, bytes: &[u8]) {
    tokio::time::timeout(CLIENT_WAIT, s.write_all(bytes))
        .await
        .expect("written within the client wait")
        .unwrap();
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
