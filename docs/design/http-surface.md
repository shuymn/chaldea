# Design: HTTP Surface (MVP PR 7)

Status: design. Scope is the PR 7 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): HTTP route and action matrices, tokens, pairing, the sync API, blobs, WebSocket notifications, and command receipts, in `crates/daemon` on `axum` + `hyper` + `tower-http` (mvp §2.5, arch §3). The contract is mvp §2.4 (replay, backward history, live notifications, commands and receipts, restart), mvp §2.5 (common checks, routes, token kinds, actions, order, secrets, rejections), the PR 7 design obligations of mvp §4, and the claims of mvp §6.2 that name PR 7 (CL-8, CL-13, CL-15). It builds on the store of `docs/design/event-store.md` (cited as "store §N"). It proposes a split into three PRs (§8 D1).

**What PR 7 decides and what it leaves.** PR 7 owns everything between the socket and a store call: connection handling, the order of checks, credentials and their lifetimes, the authorization tables and the shared object checks, the mapping of store outcomes to status codes, the read routes, the WebSocket hub, and the command envelope, receipts, runner, and dispatch seam. It does not decide daemon lifecycle, port, state directory, the single-instance lock, or what the daemon does on a fatal store outcome (PR 8), hook ingest and binding (PR 9), any command's effect (PR 10, 16, 20, 21), attach byte handling (PR 11), `POST /events` scope semantics and A2UI (PR 20), the doctor (PR 21), or the PWA (PR 18). For each of these it provides the seam named in §2.11 and §4.

**Commitments from PR 6 this design keeps** (store §2.2 "What callers learn", §2.5, §2.4, §5 "Waits"):

1. The store returns `SubmitError`, `WriteError`, and `ReadError` kinds and never chooses a status code; PR 7 owns the mapping (§2.9), including mvp §2.5's rule that a command answers 408 only if its deadline prevented the intent commit (§2.8).
2. A command whose intent job ended `CommitUnknown` has, after a restart, a receipt that is absent, `completed`, or `in_progress` (→ `unknown_after_crash`); PR 7 answers that request with 503 `outcome_unknown`, reports the outcome to PR 8's lifecycle sink whether or not a client still waits, and lets the retry after restart find out (§2.8).
3. The blob download type for `(stream_id, sha256)` is the store's (`application/octet-stream` on disagreement); PR 7 applies mvp §2.5's serving rule and headers (§2.7).
4. The v1 schema is frozen; PR 7 needs no schema change. It moves the store's `TokenKind` enum to `core::auth` and re-exports it from the store (a Rust type move, no stored value changes). The extension token's scope (PR 20) is a numbered migration (§4).
5. `Store::read` is synchronous: every async caller runs it in `spawn_blocking`; read closures and write jobs capture only data (ids, hashes, parsed arguments, principals, a non-blocking semaphore handle), never a network, PTY, supervisor, or channel handle (§2.3, §2.8).

Sources read for this document (paths are relative to each root):

| Source | Revision | Read for |
|---|---|---|
| This repository | `a08820b` | `docs/mvp-design.md`, `docs/architecture.md` §3–§4, `docs/design/event-store.md`, `crates/core/src/{sync,command,ids,lib,payload}.rs`, `crates/daemon/src/store/{mod,error,readers,rows,write_tx,writer,clock}.rs`, `Cargo.toml`, `Cargo.lock`, `.github/workflows/ci.yml`, `docs/spikes/pty-hooks-spike.md` (B-8, F-3c) |
| T3 Code (`pingdotgg/t3code`) | `10586f9e` | `apps/server/src/orchestration-v2/ThreadStream.ts`, `auth/{utils,PairingGrantStore,SessionStore,EnvironmentAuth}.ts`, `persistence/AuthPairingLinks.ts`, `persistence/Migrations/{031_AuthAuthorizationScopes,055_OrchestrationV2}.ts`, `ws.ts` |
| `hyper` | crate 1.12.0 | `src/server/conn/http1.rs`, `src/proto/h1/{role,conn,dispatch}.rs`, `src/headers.rs`, `src/common/time.rs`, `src/error.rs` |
| `axum` | crate 0.8.9 | `Cargo.toml` (features), `src/serve/{mod,listener}.rs`, `src/extract/ws.rs`, `src/routing/method_routing.rs` |
| `tower-http` | crate 0.7.1 | `src/limit/service.rs`, `src/timeout/service.rs`, `src/trace/make_span.rs`, `src/sensitive_headers.rs` |
| `hyper-util` 0.1.21, `getrandom` 0.4.3, `subtle` 2.6.1 | crates | feature lists, `getrandom::fill` |

**Facts this design relies on.** T-facts are exercised by named tests of §7 on both runners; D-facts rest on the cited source and are not re-proven.

| # | Fact | Where | Kind |
|---|---|---|---|
| H1 | `axum::serve` builds a `hyper_util` auto connection builder per connection with no timer, so hyper's default 30 s header read timeout is dropped with a warning, and it has no connection bound. | axum `src/serve/mod.rs:388-396`; hyper `src/common/time.rs:73-85` | D |
| H2 | hyper's HTTP/1 builder has `timer`, `header_read_timeout` (panics if set without a timer), `max_header_size`, `max_headers`, `keep_alive`; `serve_connection(..).with_upgrades()` lets an upgraded connection reach axum's `WebSocketUpgrade`. The header read timeout is armed whenever the connection starts reading a head, so it also bounds an idle keep-alive connection. | hyper `src/server/conn/http1.rs:201`, `:245-262`, `:341`, `:356`, `:370-373`, `:429`; `src/proto/h1/conn.rs:224-236` | T (`http_layers::slow_head_is_closed`, `http_ws::*`) |
| H3 | hyper rejects a `Content-Length` that is not all digits (`+5`), and two different values, as a header parse error answered 400 before the service is called; a head over the size or count bound is 431. | hyper `src/headers.rs:101-125`; `src/proto/h1/role.rs:295-312`, `:493-500`; `src/error.rs:513`, `:637-638` | T (`http_layers::parser_layer_cases`) |
| H4 | With `Transfer-Encoding` present, hyper drops `Content-Length` from the parsed headers and keeps `Transfer-Encoding`. | hyper `src/proto/h1/role.rs:275-293` | T (`http_layers::te_with_content_length_is_400`) |
| H5 | When a response is sent before the request body was read, hyper drains what it can read without waiting and otherwise closes the read side. On Windows a close with unread data can reset the connection (spike F-3c). | hyper `src/proto/h1/conn.rs:875-891`, `src/proto/h1/dispatch.rs:242-243`; spike `docs/spikes/pty-hooks-spike.md` §3 | D here; PR 9 records the observed outcome per OS |
| H6 | `tower-http`'s `RequestBodyLimit` answers 413 for a declared length above the limit without reading the body and wraps the body in `Limited`; `TimeoutLayer` answers 408 and drops the inner future. | `tower-http` `src/limit/service.rs:48-63`; `src/timeout/service.rs:30-35` | D |
| H7 | `tower-http`'s default `MakeSpan` records the full request URI, query included. | `tower-http` `src/trace/make_span.rs:78-90` | D (why §2.10 has its own span) |
| H8 | axum's `WebSocketUpgrade` checks method, `Connection`, `Upgrade`, `Sec-WebSocket-Key`, and version, not `Origin`; tungstenite answers pings itself; `max_message_size`, `max_frame_size`, and `max_write_buffer_size` are per-socket settings. | axum `src/extract/ws.rs:189-201`, `:449-470`, `:781-790`, `:1220-1221` | D |
| H9 | `getrandom::fill` fills a buffer from the OS CSPRNG; `uuid` v4 (the store's epoch and event ids) uses the same `getrandom` 0.4.3. | `getrandom` `src/lib.rs:87`; `Cargo.lock` (`uuid` → `getrandom 0.4.3`) | D |
| H10 | hyper's connection future completes when it hands the transport to an upgrade, and axum's `on_upgrade` runs its callback in a task it spawns itself, so neither the connection task nor any task the server owns holds an upgraded socket unless the callback hands it over. | hyper `src/server/conn/http1.rs:568-576`; axum `src/extract/ws.rs:350-365` | T (`http_ws::shutdown_aborts_a_blocked_socket`) |
| H11 | axum's `MethodRouter` sends `HEAD` to a `GET` handler, and `MethodRouter::layer` also wraps the method fallback. | axum `src/routing/method_routing.rs:967`, `:1157-1158` | T (`http_matrix::methods_are_exact`) |
| H12 | hyper may read request body bytes into its own buffer before the service polls the body (read-ahead on the connection). | hyper `src/proto/h1/dispatch.rs:166`, `:247`, `:317` | D (why §2.2 states the boundary as "no application body use") |
| H13 | axum's listener ignores per-connection accept errors (`ConnectionRefused`, `ConnectionAborted`, `ConnectionReset`) and sleeps 1 s after any other accept error instead of ending. | axum `src/serve/listener.rs:140-165` | D (the accept loop follows it) |

## 1. Goal and Dependencies

**Goal.** An `http` module in `chaldea-daemon` that serves mvp §2.5's route table on a listener PR 8 hands it; decides every request in the order Host → method → Origin → credential → token kind → action → framing → size before the application polls, decodes, or uses any body byte; answers reads from one store snapshot each; issues and checks tokens, tickets, and pair codes; notifies WebSocket clients of stream heads; and runs commands through a receipt, a daemon-owned runner, and a dispatch seam that later PRs fill. Every claim has a test on `macos-latest` and `windows-latest`.

**Files.**

```
crates/core/src/auth.rs              # Sans-IO policy: TokenKind (moved from the store), Action, Grant, MVP_COMMANDS, the action table, object-scope functions
crates/core/src/http.rs              # public wire bodies: CommandRequest, ErrorBody, WsTicket*, PairCode*, Pair*, ThreadsResponse, WorktreesResponse (§8 OQ2)
crates/daemon/clippy.toml            # disallows axum Router::{route, route_service, nest, nest_service, merge, fallback, fallback_service} (§2.2)
crates/daemon/src/store/rows.rs      # `pub use chaldea_core::auth::TokenKind` replaces its own enum
crates/daemon/src/http/mod.rs        # HttpServer, HttpConfig, LifecycleSink, ServeHandle, HttpShutdownReport
crates/daemon/src/http/listener.rs   # accept loop, ConnIo (write-stall bound), hyper http1 builder, connection tasks
crates/daemon/src/http/tasks.rs      # Tracked: the one task set for connections, sockets, and command runners
crates/daemon/src/http/routes.rs     # RouteSpec, ROUTES, Mount::{read, write, upgrade}, build_router (the only Router::route call)
crates/daemon/src/http/guard.rs      # OuterLayer (span, RequestStart, Host), GuardLayer (per route: method → … → size)
crates/daemon/src/http/auth.rs       # Secret, TokenService, verify_secret_in / verify_hash_in, Principal, deny_for, ObjectScope
crates/daemon/src/http/tickets.rs    # TicketStore, PairCodes (memory, single use)
crates/daemon/src/http/ctx.rs        # ReadCtx, WriteCtx, WriteDispatcher, LargeReplies
crates/daemon/src/http/error.rs      # ApiError, codes, the store-outcome mapping, extractor-rejection mapping (§2.9)
crates/daemon/src/http/sync_api.rs   # GET /api/events, /api/threads, /api/worktrees
crates/daemon/src/http/blobs.rs      # GET /api/blobs/{sha256}
crates/daemon/src/http/ws.rs         # POST /api/ws-ticket, GET /api/ws socket loop
crates/daemon/src/http/pair.rs       # POST /api/pair, POST /api/pair-codes (§8 OQ1)
crates/daemon/src/http/commands.rs   # envelope, request identity, CommandHandler, CommandRegistry, CommandRunner
crates/daemon/src/http/trace.rs      # request span, Secret redaction
crates/daemon/src/http/tests/        # unit tests needing fault points (§7)
crates/daemon/tests/http_*.rs        # integration tests (§7)
crates/daemon/tests/support/http.rs  # test server on 127.0.0.1:0, raw HTTP/1.1 client, WS client, capturing log subscriber
```

**Dependencies.** Root `[workspace.dependencies]`, exact pins with one comment line each, in the existing style:

```toml
# HTTP routing and extractors (mvp §2.5, arch §3); no defaults: no form, no axum tracing, no http2.
axum = { version = "=0.8.9", default-features = false, features = ["http1", "tokio", "ws", "query", "matched-path"] }
# The HTTP/1 connection builder with a timer, header limits, and upgrades (H1, H2); request parsing is hyper's.
hyper = { version = "=1.12.0", default-features = false, features = ["server", "http1"] }
# TokioIo, TokioTimer, and the tower-to-hyper service adapter for the own accept loop (H1).
hyper-util = { version = "=0.1.21", default-features = false, features = ["tokio", "server", "http1", "service"] }
# Layer and ServiceBuilder for the guard.
tower = { version = "=0.5.3", default-features = false, features = ["util"] }
# Sensitive-header marking (§2.10); no TimeoutLayer, no TraceLayer (§3).
tower-http = { version = "=0.7.1", default-features = false, features = ["sensitive-headers"] }
# Final equality of credential hashes (mvp §2.5, prior-art row 5d); same version as the spike.
subtle = "=2.6.1"
# 256-bit secrets for tokens, tickets, and pair codes from the OS CSPRNG (H9; already in the lock through uuid).
getrandom = "=0.4.3"
# Request spans (§2.10); PR 8 installs the subscriber.
tracing = "=0.1.44"
# Collecting a bounded request body under a deadline.
http-body-util = "=0.1.5"
# Splitting a WebSocket into its send and receive halves (§2.6; already in the lock through axum).
futures-util = { version = "=0.3.34", default-features = false, features = ["sink", "std"] }
```

Workspace changes: `tokio` keeps `default-features = false`; `crates/daemon` adds the features `rt`, `net`, `time`, `macros` (`sync` stays). Dev-dependencies of `crates/daemon`: `tokio` with `rt-multi-thread`; `tokio-tungstenite = "=0.29.0"` (the WS test client; the version axum 0.8.9's `ws` feature already locks); `tracing-subscriber = { version = "=0.3.23", default-features = false, features = ["fmt", "std"] }` (the capturing subscriber). `sha2 =0.11.1` (already a daemon dependency) hashes secrets and command requests. The `ws` feature adds `sha1 0.10.7` (the WebSocket accept key, `digest` 0.10 alongside `sha2`'s `digest` 0.11). `axum 0.8.9` needs Rust 1.80, `hyper-util 0.1.21` 1.85; the toolchain is 1.96.0. The PR records the resolved lock versions (axum-core 0.5.6, http 1.5.0, http-body 1.1.0, matchit 0.8.4, tungstenite 0.29.0) in its description.

**Workspace and CI.** No new CI job. `cargo test --workspace --locked` runs the HTTP tests on both runners against `127.0.0.1:0` listeners and stores in the OS temp directory. The `core` dependency check gains `hyper|tower|getrandom` in its forbidden list, since `core::auth` and `core::http` are pure:

```
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum|hyper|tower|getrandom) ' <<<"$deps"
```

No test is Windows-only, so the sentinel list is unchanged. `lefthook.yml` mirrors the changed check.

## 2. Types and Functions

Names may change in review; shapes may not.

### 2.1 The Rules

1. **The platform decides framing; one guard decides access.** Request parsing, head limits, and the slow-head bound are hyper's (H2, H3). Every access decision before the handler runs is one outer layer plus one tower layer per route built from that route's `RouteSpec`; the router is built only from the route table, and a literal inventory written independently of that table tests the real router (§2.2, §7).
2. **The database decides; memory only remembers what is safe to forget.** Tokens and receipts live in SQLite and are checked at use time, by hash, inside the transaction that uses them (§2.4). Tickets and pair codes live in memory, single use under one mutex each; a restart drops them, and losing one only makes its holder ask for a new one.
3. **One deciding point per concurrent operation** (§2.12): the writer's serial job order for receipts, revocations, and pair exchanges; one read snapshot for each read; a mutex conditional `remove` for tickets and pair codes; one sequential loop per WebSocket; one `Tracked` task set for everything that outlives a request future.
4. **Every wait has a deadline, and a committed intent never loses its owner.** Body reads, store admission, reads, socket sends, idle sockets, writes to a stalled client, and shutdown all have one (§5). Whatever must happen after an intent commits (its effect, its receipt completion, reporting a fatal store outcome) is owned by a daemon task, never by the request future (§2.8).

### 2.2 Server, Routes, Guard

```rust
pub struct HttpConfig {
    pub port: u16,                          // the bound port; Host must name it
    pub short_deadline: Duration,           // 5 s: every request without its own budget
    pub header_read_timeout: Duration,      // 5 s (H2): slow or idle heads end the connection (EOF)
    pub write_stall_timeout: Duration,      // 30 s with no byte accepted by the socket → the connection ends (§2.7)
    pub max_header_bytes: usize,            // 8 KiB (spike B-8); over → 431 (H3)
    pub max_body_bytes: u64,                // 4 MiB (mvp §2.5)
    pub max_connections: usize,             // 128; the accept loop waits for a permit
    pub large_reply_slots: usize,           // 4: blobs and replies over 1 MiB (§2.7)
    pub ws: WsConfig,                       // §2.6
    pub ticket_ttl: Duration,               // 30 s
    pub pair_code_ttl: Duration,            // 60 s
    pub clock: Arc<dyn MonotonicClock>,     // tickets, pair codes, deadlines; tests inject
}
/// PR 8's seam: told of every fatal store outcome an HTTP write meets, whether or not a client still waits.
pub trait LifecycleSink: Send + Sync + 'static { fn store_fatal(&self, f: StoreFatal); }
pub enum StoreFatal { CommitUnknown, WriterGone, Failed }
pub struct HttpServer { /* Arc<AppState>: store, config, TokenService, TicketStore, PairCodes, CommandRegistry,
                          WriteDispatcher, Tracked, LargeReplies, revocation nudge, LifecycleSink */ }
impl HttpServer {
    pub fn new(cfg: HttpConfig, store: Store, registry: CommandRegistry, sink: Arc<dyn LifecycleSink>, mounts: Mounts) -> HttpServer;
    pub fn serve(&self, listener: tokio::net::TcpListener) -> ServeHandle;   // the listener is PR 8's, bound to 127.0.0.1
    pub fn tokens(&self) -> &TokenService;     // PR 8 (owner token file), PR 10 (launch tokens), PR 20
    pub fn nudge_revocation(&self);            // re-check every WebSocket's token now (§2.6)
}
impl ServeHandle {
    /// Stops accepting, asks connections to finish, closes sockets, waits for every tracked task until
    /// `deadline`, aborts the rest, and reports.
    pub async fn shutdown(self, deadline: Instant) -> HttpShutdownReport;
}
pub struct HttpShutdownReport { pub connections_finished: usize, pub connections_aborted: usize,
                                pub sockets_closed: usize, pub sockets_aborted: usize,
                                pub runners_finished: usize, pub runners_aborted: usize }
```

**Accept loop** (`listener.rs`). Its own loop instead of `axum::serve`, because `axum::serve` sets no timer and so no header read timeout, and has no connection bound (H1). The loop `select!`s on: the shutdown signal; a connection permit (`Semaphore(max_connections)`, acquired before `accept`, so excess clients wait in the kernel backlog); `accept`; and `Tracked::join_next` (finished tasks are reaped while serving, not only at shutdown). Per-connection accept errors are ignored, any other accept error is logged and the loop sleeps 1 s and continues (H13); a failing `set_nodelay` is logged and the connection is served anyway. Each accepted stream is wrapped in `ConnIo` (a write that makes no progress for `write_stall_timeout` fails with `TimedOut`, which ends the connection and frees what hyper buffered; §2.7) and served with `hyper::server::conn::http1::Builder` configured with `timer(TokioTimer)`, `header_read_timeout`, `max_header_size`, `max_headers(64)`, `keep_alive(true)`, then `serve_connection(TokioIo, TowerToHyperService(app)).with_upgrades()` (H2), in a task spawned through `Tracked` that selects on its connection and the shutdown signal (calling `graceful_shutdown`). HTTP/2 is not enabled.

**`Tracked`** (`tasks.rs`). One `Mutex<JoinSet<TaskKind>>` of connection tasks, upgraded-socket tasks (§2.6), and command runners (§2.8). Nothing that must end at shutdown is spawned any other way; `ServeHandle::shutdown` signals, waits on the set until its deadline, `abort_all`s the rest, and counts each kind.

**Route table** (`routes.rs`). One static table, the single production source for the router and the guard:

```rust
pub struct RouteSpec {
    pub method: Method, pub path: &'static str,        // axum path syntax, e.g. "/api/blobs/{sha256}"
    pub origin: OriginRule,                            // AbsentOrMatching | PresentMatching | MustBeAbsent | NotChecked (static PWA)
    pub credential: Credential,                        // None | Bearer(&'static [TokenKind]) | Ticket(TicketPurposeKind) | PairCode
    pub action: ActionRule,                            // None | Fixed(Action) | ByCommandName (resolved through core::auth::MVP_COMMANDS)
    pub body: BodyRule,                                // NoBody | Json
    pub budget: Budget,                                // Short | PerCommand (the command's own) | Upgrade (handshake; the socket has its own)
    pub owner_pr: u8,                                  // the PR that implements the handler; until then 501
}
pub static ROUTES: &[RouteSpec] = &[ /* the table below */ ];
pub enum Mount {
    Read(&'static RouteSpec, ReadHandler),             // gets ReadCtx only; wrapped in timeout_at(deadline) → 408
    Write(&'static RouteSpec, WriteHandler),           // gets WriteCtx; never wrapped in a dropping timeout
    Upgrade(&'static RouteSpec, UpgradeHandler),       // gets an admitted upgrade (§2.6)
}
pub(crate) fn build_router(mounts: &[Mount], state: AppState) -> Router;  // the only Router::route call
```

| Method, path | Origin | Credential (kinds) | Action | Mount | Handler PR |
|---|---|---|---|---|---|
| `GET /`, `GET /assets/{*path}` | NotChecked | none | — | Read | 18 |
| `POST /api/pair` | PresentMatching | PairCode | — | Write | 7 |
| `POST /api/pair-codes` (§8 OQ1) | MustBeAbsent | Bearer(owner) | — | Read (memory only) | 7 |
| `POST /api/ws-ticket` | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadStream` (an `attach:` purpose also `Attach`, checked on the body) | Read (memory only) | 7 |
| `GET /api/ws` | AbsentOrMatching | Ticket(events) | — | Upgrade | 7 |
| `GET /api/attach/{thread}` | AbsentOrMatching | Ticket(attach) | `Attach` | Upgrade | 11 |
| `GET /api/events` | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadStream` | Read | 7 |
| `GET /api/blobs/{sha256}` | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadBlob` | Read | 7 |
| `GET /api/threads` | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadThreadList` | Read | 7 |
| `GET /api/worktrees` | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadWorktrees` | Read | 7 |
| `GET /api/adapters` | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadAdapters` | Read | 10 |
| `GET /api/doctor` | AbsentOrMatching | Bearer(owner, rescue) | `ReadDoctor` | Read | 21 |
| `POST /api/commands/{name}` | AbsentOrMatching | Bearer(owner, agent) | ByCommandName | Write | 7 seam; each command its PR |
| `POST /hook` | MustBeAbsent | Bearer(hook) | — | Write | 9 |
| `POST /events` | MustBeAbsent | Bearer(owner, rescue, extension) | `PostEvents` | Write | 20 |

**Read routes cannot write; write routes cannot time out by dropping.** The mount kind is the guarantee, not a flag: only `Mount::Write` routes get the `WritePermit` request extension, `WriteCtx` extraction requires it (a read mount asking for one fails to build its router in a test-covered `build_router` check and answers 500 if reached), and `build_router` wraps only `Mount::Read` handlers in `timeout_at(deadline)` → 408. The store handle is private to `ctx.rs`, so a handler reaches the store only through `ReadCtx` or `WriteCtx`. The two memory-only routes are read mounts: a 408 there leaves at most an unreturned ticket or code that expires unused.

**Building the router.** `build_router` registers each mount under exactly its spec's method (`on(MethodFilter::from(spec.method), handler)`) with a method fallback, wraps the resulting `MethodRouter` in `GuardLayer::new(spec)`, and adds a router fallback for unknown paths. `crates/daemon/clippy.toml` lists axum's `Router::route`, `route_service`, `nest`, `nest_service`, `merge`, `fallback`, and `fallback_service` under `disallowed-methods`; `build_router` carries the one `#[allow(clippy::disallowed_methods)]`. `build_router` refuses (panics at startup; tested) a mount whose spec is not in `ROUTES`, a `ROUTES` entry with no mount, or a mount kind that contradicts the spec (a `Write` mount for a read route or the reverse). Production `Mounts::mvp()` mounts every route, with 501 handlers for routes whose PR has not merged.

**Guard order** (mvp §2.5 "Order", "Rejections"):

| Step | Where | Check | Failure |
|---|---|---|---|
| 0. Parse | hyper (H2, H3) | head within `header_read_timeout`, `max_header_size`, `max_headers`; valid framing headers | EOF for a slow head; 431; 400 (hyper's own responses, §2.9) |
| 1. Span, start, Host | `OuterLayer`, before routing | opens the request span; inserts `RequestStart(now)`; exactly one `Host`, equal to `127.0.0.1:<port>` or `localhost:<port>` (`localhost` ASCII case-insensitive); a request target in absolute form must carry exactly that authority as a whole string (no userinfo, same port) | 421 `misdirected_request` |
| — | router | known path | 404 `not_found` (fallback, an `ErrorBody`) |
| 2. Method | `GuardLayer` | the method equals the spec's exactly (`HEAD` is not accepted on `GET` routes; H11) | 405 `method_not_allowed` with `Allow` |
| 3. Origin | `GuardLayer` | per `OriginRule`: at most one `Origin`; `AbsentOrMatching`: absent or exactly `http://` + the received `Host` value; `PresentMatching`: present and matching; `MustBeAbsent`: absent. `Origin: null` is present and never matches | 403 `origin_rejected` |
| 4. Credential | `GuardLayer` | Bearer: parsed and verified (§2.4). Ticket: the query is exactly one `ticket` parameter (else 401 for a missing or repeated `ticket`, 400 `bad_query` for any other parameter); the ticket is **peeked**, not removed (§2.4). PairCode: none here; the code is in the body and is checked by the handler | 401 `unauthorized` with `WWW-Authenticate: Bearer`; 503 `busy` / 500 `internal` for a failed store lookup (§2.9) |
| 5. Kind and action | `GuardLayer` | the verified kind is in the route's kinds; `Fixed(a)`: `grant(kind, a)` exists; `ByCommandName`: `{name}` is in `MVP_COMMANDS` (else 404 `not_found`) and `grant(kind, action_of(name))` exists | 403 `forbidden` |
| 6. Framing | `GuardLayer` | any `Transfer-Encoding` → 400 `bad_request` (checked first: hyper has already removed `Content-Length` when both are present, H4); `POST`: one `Content-Length` (else 411 `length_required`); `GET`: no `Content-Length` other than `0` (else 400) | as listed |
| 7. Size | `GuardLayer` | `Content-Length` ≤ `max_body_bytes` (else 413 `payload_too_large`); the body is wrapped in `http_body_util::Limited` as the second line | 413 |
| 8. Unimplemented | `GuardLayer` | the route's or command's handler PR has not merged | 501 `not_implemented` |
| 9. Deadline | `GuardLayer` | `Deadline = RequestStart + budget` is in the extensions; read mounts are wrapped in `timeout_at(deadline)` | 408 |

**The deadline starts before the first credential read.** `RequestStart` is taken in `OuterLayer` when hyper hands the parsed request to the service: for the first request of a connection and for every request on a reused one, it is the moment the head is complete. Head reading has its own bound (`header_read_timeout`, armed when the connection starts reading that head, H2), so a request's wall time from the first head byte is at most `header_read_timeout + budget`. The step-4 lookup (`wait_until = deadline`), `body_within`, and the write job's `not_after` all use the same `Deadline`. PR 9 may measure `/hook`'s budget from `ConnStart`, which the connection inserts too.

**The body boundary.** No application code polls, decodes, or uses a body byte before steps 1–8 accept the request; every rejection in them is a response with no store write, no event, no ticket or pair code consumed, and no handler call. The transport may already hold body bytes (hyper reads ahead, H12; the client may send them), so the claim is about application use, not about bytes on the wire. Every rejection decided before the body carries `Connection: close`; whether a Windows client that kept sending a large body reads that response is H5's case, which PR 9 records with the B-8 drain case.

### 2.3 Store Access: `ReadCtx`, `WriteCtx`, `WriteDispatcher`

```rust
pub struct ReadCtx { principal: Principal, deadline: Deadline, store: Store }
impl ReadCtx {
    /// One `Store::read` in `spawn_blocking`, `wait_until = deadline`, awaited until the deadline. Inside,
    /// `verify_hash_in` runs first in the same snapshot as everything `f` reads.
    pub async fn read<R: Send + 'static>(
        &self,
        f: impl FnOnce(&ReadTx<'_>, &Authorized) -> Result<R, ApiError> + Send + 'static,
    ) -> Result<R, ApiError>;
}
pub struct WriteCtx { principal: Option<Principal>, deadline: Deadline, dispatcher: WriteDispatcher }
impl WriteCtx {
    /// Hands one job (not_after = deadline) to the dispatcher and waits for its outcome until the deadline.
    pub async fn submit<R: Send + 'static>(&self, job: impl FnOnce(&mut WriteTx<'_>, Option<&Authorized>) -> Result<R, JobError> + Send + 'static)
        -> WriteOutcome<R>;               // Done(Result<R, ApiError>) | Unresolved (deadline passed, outcome not known yet)
    pub async fn body_within(&self, body: Body, max: u64) -> Result<Bytes, ApiError>;   // 408 at the deadline, 413 over max
}
```

**Typed failures survive the wrappers.** `Store::read`'s closure returns `Result<R, StoreError>`; `ReadCtx` passes it a closure that returns `Ok(f(tx, &auth))`, so a `StoreError` from a store call maps through §2.9 (503 `busy` for `Busy`, 500 `internal` otherwise) and an authorization or validation failure stays the `ApiError` `f` chose. `verify_hash_in` returns `AuthFailure::{Invalid, Store(StoreError)}`: `Invalid` (missing, revoked, malformed, agent launch not current) is 401, `Store` maps as above; a failed lookup is never a 401. Write jobs return `JobError::Refused(ApiError)` for refusals and `JobError::Store` for store errors, mapped the same way.

**`WriteDispatcher`.** Every store write the HTTP surface makes (commands, pairing, token issuance and revocation) goes through it: it submits the job, spawns through `Tracked` a task that owns the `WriteTicket`, awaits it without the request deadline (the writer replies to every accepted job, store §5 "Waits"), reports `CommitUnknown`, `WriterGone`, and `Failed` (and `SubmitError::Failed`) to `LifecycleSink::store_fatal`, and forwards the outcome to the waiting request through a `watch`, if one still listens. A request that stops waiting, or whose client disconnects, therefore never hides a fatal outcome from PR 8. Commands use the same dispatcher through `CommandRunner` (§2.8).

`spawn_blocking` read closures capture the moved `Store`, the principal (token id and hash), parsed parameters, and for large replies an `Arc<Semaphore>` used only through `try_acquire_owned`; nothing that blocks. A `spawn_blocking` read that outlives its deadline keeps running until `f` returns; reads have no effect, so the 408 is accurate (§5).

### 2.4 Tokens, Tickets, Pair Codes

**Secret format.** 32 bytes from `getrandom::fill`, base64url without padding (43 characters), with a fixed prefix per credential: `chd_` (token), `chdt_` (ticket), `chdp_` (pair code). The prefix lets the log scrubber (§2.10) and secret scanners match them; the kind is not encoded. `Secret(String)` has no `Display`, and its `Debug` prints `Secret(<redacted>)`. Only `TokenService::mint` and the pair-code and ticket minting functions create secrets; after a request is verified, only the token's id and SHA-256 travel (`Principal`, tickets, pair codes, socket state); no plaintext secret is kept past the request that carried it.

**Bearer parsing.** Exactly one `Authorization` header; its value is the scheme `Bearer` (ASCII case-insensitive), exactly one space, and a token matching `chd_[A-Za-z0-9_-]{43}`, nothing else (hyper has already trimmed surrounding whitespace). Anything else is 401.

**Hashing and comparison.** The stored and looked-up value is `SHA-256(secret bytes)`. Lookups go by hash (`tokens.sha256` is `UNIQUE`; the ticket and pair-code maps are keyed by hash), so timing reveals at most the position of a SHA-256 output in an index; the final equality between the computed hash and the found row's hash uses `subtle::ConstantTimeEq` (mvp §2.5, prior-art row 5d). T3 Code compares secrets with `timingSafeEqual` (`apps/server/src/auth/utils.ts:83-89`).

```rust
pub struct MintedToken { pub token_id: TokenId, pub secret: Secret, pub sha256: [u8; 32] }
pub struct Principal { pub token_id: TokenId, pub token_sha256: [u8; 32], pub kind: TokenKind, pub thread: Option<ThreadId> }
impl TokenService {
    pub fn mint(&self) -> MintedToken;                                   // random id (v4) and secret; touches nothing
    pub fn new_token(m: &MintedToken, kind: TokenKind, thread: Option<ThreadId>, launch: Option<LaunchId>) -> NewToken;
    pub fn verify_secret_in(tx: &ReadTx<'_>, secret: &str) -> Result<Authorized, AuthFailure>;   // parse, hash, verify_hash_in
    pub fn verify_hash_in(tx: &ReadTx<'_>, sha256: &[u8; 32]) -> Result<Authorized, AuthFailure>;
    pub async fn issue_owner(&self, issuer: Option<[u8; 32]>, deadline: Instant) -> Result<Secret, ApiError>;   // §2.4 pairing
    pub async fn revoke(&self, which: TokenSelector, deadline: Instant) -> Result<u64, ApiError>;  // then nudge_revocation
}
```

**`verify_hash_in`** (the one token check; step 4 and every `ReadCtx`/`WriteCtx`/socket read run it): `token_by_hash`, constant-time equality, `revoked_at IS NULL`; for `agent`, also: the token's launch is its thread's `current_launch_id` and that launch has no `exit_reason` (mvp §2.5 "one launch" holds at use time even before PR 10 revokes); for `hook`, only revocation (PR 9 adds its launch and binding checks). `rescue` and `owner`: until revoked. `extension`: until revoked; its one-run lifetime is PR 20's revocation after the run. No `token_by_id` is needed: every later check has the hash.

| Credential | Lives in | Restart | Expiry enforced at | Single use decided by |
|---|---|---|---|---|
| Token (`owner`, `agent`, `rescue`, `hook`, `extension`) | SQLite `tokens` (hash only) | survives; mvp §2.9 restart revokes every open launch's tokens (PR 10) | `verify_hash_in` in the transaction that uses it | — (multi-use) |
| WS ticket | memory `TicketStore`: hash → `{ token_id, token_sha256, purpose, expires_at }` | lost; clients mint a new one (they reconnect after a restart anyway, mvp §2.4) | at the conditional removal | `Mutex` + conditional `remove` |
| Pair code | memory `PairCodes`: hash → `{ issuer_sha256, expires_at }` | lost; the user runs `chaldea open` again | at the conditional removal | `Mutex` + conditional `remove` |
| Command receipt | SQLite `command_receipts` | `in_progress` → `unknown_after_crash` (PR 8/10 call `receipts_mark_unknown_after_crash`) | — | the writer's job order (§2.8) |

**Tickets** (`POST /api/ws-ticket`). Body `WsTicketRequest { purpose: "events" | "attach:<thread_id>" }`; reply `WsTicketResponse { ticket, expires_in: 30 }`. `attach:` requires the `owner` kind and the `Attach` grant (403 otherwise); the thread must exist (404 otherwise). The store holds at most 1024 tickets and 32 per token; expired entries are purged on every insert; over either cap → 503 `busy` with `retry_after` (a client mints one ticket per connect, so 32 is far above any honest use).

**Redemption** (`GET /api/ws?ticket=`, `GET /api/attach/{thread}?ticket=`), so that a rejected upgrade never spends a valid ticket and the credential error keeps its place in the order:

1. Step 4 **peeks** under the mutex: absent, expired (`now ≥ expires_at`), or of another purpose (an `attach:` ticket must name the path's thread) → 401; nothing removed.
2. Step 5 checks the kind (`attach` needs an `owner` ticket); steps 6–7 the framing (a `GET` with a body is 400).
3. The upgrade handler validates the WebSocket handshake (axum's `WebSocketUpgrade` extraction; a rejection is 400 `bad_request`, §2.9) and takes a socket slot (`max_sockets`, 16 per token; none free → 503 `busy`).
4. **Conditional removal** under the mutex: remove the entry only if it is still present and `now < expires_at`, sampling the clock at the removal; if another request removed it first or it expired since the peek → 401, and the slot is released.
5. **Admission read**: `verify_hash_in` on the ticket's token hash (a revoked token kills its tickets, mvp §2.5) and, for `attach`, the `owner` kind again → 401 on failure (the ticket is spent, as its token is dead).
6. `101`, and the socket is handed to its tracked task (§2.6).

Restart safety: a ticket is worth 30 s and one socket; losing it only makes the client mint another with its Bearer token. T3 Code uses signed stateless WebSocket tickets with a 5-minute expiry and a session-revocation check at use, not single use (`auth/SessionStore.ts:493`, `:1049-1064`, `:1113`, `:1138`; its media tickets are reused on purpose, `auth/EnvironmentAuth.ts:1500-1502`); Chaldea's tickets are single use (mvp §2.5), which a stateless signature cannot enforce without a spent list, so they are memory entries.

**Pair codes, bound to their issuer.** `POST /api/pair-codes` (owner Bearer, Origin absent, §8 OQ1) mints a code, keeps `{ issuer_sha256: the caller's token hash, expires_at: now + 60 s }` under its hash (at most 8 outstanding; purge on insert; full → 503 `busy`), and returns `PairCodeResponse { code, expires_in: 60 }`; `chaldea open` opens `http://127.0.0.1:<port>/#pair=<code>`. `POST /api/pair` (Origin present and matching) takes `PairRequest { code }`:

1. Under the mutex, conditional `remove` by hash (present and `now < expires_at` at the removal), else 401 `unauthorized`. The code is spent from here on.
2. One write job: `verify_hash_in(issuer_sha256)` (issuer revoked or gone → `Refused(401)`, nothing inserted), then `token_insert` of a new `owner` token. The writer's order decides the race with a revocation of the issuer: a revocation committed first refuses the exchange; one committed after it does not revoke the token just issued (each owner token is revoked on its own).
3. The reply is `PairResponse { token }`. If the job fails, is refused, or its outcome is not known by the deadline, the code stays spent and the reply is the error (401, or 503 `outcome_unknown`; never `in_progress`, as no receipt exists). A token whose reply was lost is a valid owner token nobody holds; it is harmless and revocable.

T3 Code spends a one-time link in one conditional `UPDATE … WHERE consumed_at IS NULL AND expires_at > now` (`persistence/AuthPairingLinks.ts:239-247`) and in-memory grants through an atomic `Ref.modify` (`auth/PairingGrantStore.ts:546`); `PairCodes` is the latter shape. A pair code is never persisted: it would be a secret at rest for 60 s of value.

### 2.5 Authorization: `core::auth`

```rust
#[derive(Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum TokenKind { Owner, Agent, Rescue, Hook, Extension }           // mvp §2.5 public names; the store re-exports it
pub enum Action { ReadStream, ReadBlob, ReadThreadList, ReadWorktrees, ReadAdapters, ReadDoctor,
    Spawn, Send, Stop, Resume, Close, TaskSet, InputCancel, Key, Attach, Ready, InputResolve,
    MailboxSend, WorktreeRemove, PostEvents, SurfaceAction, DoctorRun }
pub enum Grant {
    All,                    // any object
    SelfPlus,               // the token's thread and its descendants
    ChildrenOfSelf,         // spawn: parent = the token's thread; never `--role rescue`
    SelfAndDirectChildren,  // task set
    CreatedBySelf,          // input cancel: input.created_by = thread:<self>
    ParentAndChildren,      // mailbox send: recipient is the parent or a child
    OwnWorktree,            // worktree remove: owner_thread_id == the token's thread, no flags
    ExtensionScope,         // POST /events: the token's configured scope (PR 20)
    SurfaceScopeContains,   // surface_action: the surface's scope contains the target (PR 20)
}
pub struct CommandSpec { pub name: &'static str, pub action: Action, pub owner_pr: u8 }
pub static MVP_COMMANDS: &[CommandSpec];   // spawn, resume, send, ready, key, stop, close, input_resolve, input_cancel,
                                           // task_set, mailbox_send, worktree_remove, doctor_run, surface_action
pub fn grant(kind: TokenKind, action: Action) -> Option<Grant>;            // the table; None = refused
pub struct ThreadGraph { /* thread → parent, from proj_threads */ }
pub fn self_plus(g: &ThreadGraph, me: ThreadId) -> BTreeSet<ThreadId>;
pub fn may_act_on_thread(g: &ThreadGraph, me: Option<ThreadId>, grant: Grant, target: ThreadId) -> bool;
pub fn may_remove_worktree(me: Option<ThreadId>, grant: Grant, owner_thread_id: Option<ThreadId>) -> bool;
```

The table is mvp §2.5's "Actions" table transcribed cell by cell (`owner`: `All`, with `SurfaceScopeContains` for `surface_action`; `agent`: `SelfPlus` for reads, send, stop, resume, close, and the rest as named, `OwnWorktree` for `worktree remove`; `rescue`: `All` for reads and doctor read, `PostEvents` any scope; `extension`: `ExtensionScope` for `PostEvents` only; `hook`: nothing on these actions, its only route is `/hook`). `MVP_COMMANDS` gives every command a fixed action from the start, so the guard decides kind and action for every command name in 7a, while every handler is still 501. Listing worktrees (`ReadWorktrees`) stays `SelfPlus` for agents; removing one (`OwnWorktree`) needs the worktree's current owner to be the caller itself (mvp §2.5 "worktrees it owns"; §2.6 ownership passes to the parent only on `thread_closed`).

**Object checks** run on the validated body before any effect, in the same transaction as the effect (mvp §2.5 "Order"): reads in the `ReadCtx` snapshot, commands in the intent job (§2.8). For an update or delete, the command handler also checks the stored object's scope through `ObjectScope::stored_surface(tx, stream, surface_id, principal)` (the surface a `POST /events` replaces must already be in the caller's scope; PR 20 calls it). **Hiding:** an object the principal cannot read answers 404 `not_found`, identical to a missing one; an object it can read but not act on answers 403 `forbidden`. `deny_for(graph, principal, object)` decides between them after the action grant is checked, so no handler chooses its own. A refusal returns `Refused` from the intent job before `receipt_begin`, so it leaves no receipt and starts no effect.

**Readable streams:** `owner`, `rescue`: every stream; `agent`: `thread:<t>` for `t ∈ self_plus`; the `host` stream is not self+ and is 404 for agents, who read the thread list and worktrees through the filtered routes (host events cannot be filtered without breaking the gapless `seq` a client pages by). **Per-resource filtering:** `GET /api/threads` returns readable threads (all for `owner` and `rescue`); `GET /api/worktrees` returns all, human-owned included, for `owner` and `rescue`, and those with `owner_thread_id ∈ self_plus` for `agent`; a blob needs a readable stream and a `blob_refs` row; WebSocket subscriptions keep only readable streams. A receipt is visible only to the token that created it (§2.8).

### 2.6 WebSocket: Socket Tasks and the Socket Loop

```rust
pub struct WsConfig {
    pub heads_interval: Duration,        // 30 s (mvp §2.4); tests 200 ms
    pub send_timeout: Duration,          // 10 s for one frame; past it the socket is dropped
    pub idle_timeout: Duration,          // 75 s with no inbound frame (the browser answers our pings)
    pub max_message_bytes: usize,        // 64 KiB inbound (max_message_size and max_frame_size, H8)
    pub max_write_buffer_bytes: usize,   // 256 KiB (H8)
    pub max_subscriptions: usize,        // 1024 streams per socket
    pub max_sockets: usize,              // 128 total, 16 per token
    pub close_grace: Duration,           // 1 s to send a close frame at shutdown
}
```

**Socket ownership.** axum's `on_upgrade` runs its callback in a task it spawns, and hyper's connection task ends at the upgrade (H10). So the upgrade handler first spawns the socket's own task through `Tracked` (holding the socket slot and a `oneshot::Receiver<WebSocket>`), then calls `on_upgrade(|ws| async move { let _ = tx.send(ws); })`: axum's task only hands the socket over and ends. The tracked task waits for the socket until the short deadline (a failed upgrade drops the sender and ends it), then runs the loop. Every socket is therefore in the set `ServeHandle::shutdown` waits on and aborts, from before `101` to its exit; aborting the task drops the socket. PR 11's attach handler gets the same `UpgradeHandler` seam.

**Socket loop.** The task owns the split socket (`futures_util` `split`: a sink and a stream), its own `broadcast::Receiver<Committed>` (taken first), and a `watch::Receiver` of the revocation nudge (taken second, marked seen). Then a **startup read**: `verify_hash_in` on the token (a revocation committed between the admission read and here is caught; one committed after it fires the nudge, which the socket already watches) and nothing else; on failure close 1008. Then `hello { epoch, server_time }`.

The loop is one sequential `select!` whose branches are: an inbound frame; a commit notification; the in-flight send (at most one frame, its future held across iterations with an absolute `send_deadline`); the next heads time (`sleep_until(next_heads_at)`, `next_heads_at += heads_interval` from an absolute schedule, so a slow iteration does not add drift); the nudge; the shutdown signal; and the idle deadline. Control branches never wait behind a send: a heads tick or nudge whose read finds the token invalid closes the socket even while a send is pending, and a send that misses its deadline drops the socket.

State: the subscription set `S`, `last_sent: BTreeMap<StreamId, u64>` (the highest head announced per stream), and `pending: BTreeMap<StreamId, u64>` (heads to announce, at most `|S|` entries).

| Event | Effect |
|---|---|
| `subscribe { stream_ids }` | parse (malformed ids dropped; more than `max_subscriptions` → close 1008 `too_many_subscriptions`); one read with the token re-verified: keep readable streams, read their heads; **replace** `S`, **clear** `pending`, set `last_sent` to the heads read, and queue `heads` for `S`. Unreadable and unknown ids are dropped silently |
| `Committed { heads }` | for each entry with `stream ∈ S` and `head > last_sent[stream]`: `pending[stream] = max(pending[stream], head)` |
| send ready | if a `heads` frame is queued, send it; else take the lowest `pending` entry, send `stream_advanced`, set `last_sent` |
| `Lagged(n)` | one read of the heads of `S` (token re-verified); clear `pending`, set `last_sent`, queue `heads` (store §2.2: a lagging receiver resyncs from heads) |
| heads time | one read: `verify_hash_in`, heads of `S`; queue `heads` with entries newer than `last_sent` and the rest unchanged; send a `Ping` with it |
| nudge | one read: `verify_hash_in`; on failure close 1008 `token_revoked` |
| inbound text that is not a `WsClientMessage`, binary, or over size | close 1003 / 1009 |
| `Closed` from the receiver, or the shutdown signal | close 1001 within `close_grace`, then drop |
| send deadline, read error, idle deadline | drop the socket |

**Why a socket converges and never goes backwards.** Notifications are published after `COMMIT` (store §2.2) and the receiver exists before the startup read; every change of `S` and every lag is followed by a heads read that resets `pending` and `last_sent`; a `Committed` buffered before that read and delivered after it carries a head ≤ `last_sent` and is suppressed. A lost notification is repaired by the next heads time within `heads_interval` and by the subscribe-time heads on reconnect, which is mvp §2.4's recovery bound and CL-13's premise. Only streams in `S` are ever announced.

**Revocation.** Tickets die with their token at redemption; a new socket re-verifies at startup. An open socket re-verifies on every read it makes (subscribe, heads time, lag) and on the nudge, which `TokenService::revoke` and `HttpServer::nudge_revocation` fire after a revoking job commits (PR 10 and PR 16 call it after their own revoking jobs). The nudge carries no state; each socket decides by reading the database. **Unannounced window (decided):** a revocation committed without a nudge is applied at the socket's next read, at the latest one `heads_interval` (30 s) later; during it the socket can send only `stream_advanced` / `heads` numbers for streams the token could read before the revocation, never event content, which every `GET /api/events` re-checks. Closing this window would need a nudge from every revoking path, which the nudge seam already offers.

**Subscription cap.** PR 18 subscribes to the streams it renders (the host stream and the threads in view), so 1024 is far above use; a client that wants more pulls the rest on demand. A subscribe over the cap is a client bug and closes the socket (1008 `too_many_subscriptions`) rather than silently dropping streams; splitting a large set across subscribes is not supported, since each replaces the set.

T3 Code's WebSocket is an Effect RPC transport (`apps/server/src/ws.ts`) with server-pushed streams; it is not used as precedent for the notify-then-pull loop, which follows mvp §2.4.

### 2.7 Sync Read API, Blobs, Large Replies

**`GET /api/events`** (mvp §2.4). Query parameters: `stream_id` (required, `StreamId` syntax), `after_seq`, `before_seq` (decimal `u64` up to `MAX_SAFE_INTEGER`), `limit` (decimal `u32`), `epoch` (string). A missing `stream_id`, a malformed or repeated parameter, or an unknown parameter → 400 `bad_query` (before any read). Then one `ReadCtx::read`:

1. `verify_hash_in` (inside `ReadCtx`); the stream must exist (`stream_exists`) and be readable (§2.5), else 404 `not_found` (one body for both).
2. `head(stream, after_seq.unwrap_or(0))`, then `core::sync::decide_replay(&query, head, store.epoch())`: `LimitOutOfRange` and `AfterAndBefore` → 400 `bad_query`.
3. Size admission (below), then: `Forward` → `events_after`, `Backward` → `events_before` → `EventsResponse::Replay { stream_id, epoch, events, head_seq, has_more, has_more_before }`; `Snapshot { reason }` → `thread_snapshot` or `host_snapshot` (agents never reach the host stream) → `EventsResponse::Snapshot { stream_id, epoch, snapshot, as_of_seq, reason }`.

Head, page, snapshot, and `as_of_seq` come from that one read transaction (store §2.2, PR6-CL-18), so a commit during the request is either wholly in the reply or wholly after `head_seq`/`as_of_seq`. The byte bound uses `cum_bytes` through `StreamHead`. The reply always carries the server epoch; a backward page with a mismatched epoch is still a page (`decide_replay`), and the client resyncs forward (mvp §2.4). `before_seq = 1` returns `events: []` with `has_more_before: false`; `before_seq > head_seq` selects the latest page by the store's `seq < before_seq` predicate (a separate matter from the store clamping values above `i64::MAX`). Events of an unknown kind, newer `schema_version`, or invalid payload are returned as stored (store §2.7). The 404 precedes the 400 of step 2 because `decide_replay` takes the stream head (`crates/core/src/sync.rs`, "a 404 decided before this call"). This is T3 Code's `decideThreadResume` (`apps/server/src/orchestration-v2/ThreadStream.ts:100-118`, bounds at `:18`, `:20`) with mvp's bounds and epoch.

**`GET /api/threads`** → `ThreadsResponse { epoch, threads: Vec<ThreadInfo> }` filtered (§2.5). **`GET /api/worktrees`** → `WorktreesResponse { epoch, worktrees: Vec<WorktreeChanged> }` filtered. **`GET /api/adapters`** and **`GET /api/doctor`**: route, guard, and 501 until PR 10 and PR 21.

**`GET /api/blobs/{sha256}?stream_id=`.** `sha256` must be 64 lowercase hex and `stream_id` present and valid (no other parameter), else 400 `bad_query`. One read: readable stream (else 404), size admission, `ReadTx::blob(stream, sha256)` (`None` → 404, the same body). Response: 200, `Content-Type` = the store's download type if it is exactly `text/plain; charset=utf-8` or `application/json`, else `application/octet-stream` (mvp §2.5), `Content-Length`, `X-Content-Type-Options: nosniff`, `Content-Disposition: attachment`, `Cache-Control: no-store`; no `Range`, no `ETag`.

**Large replies and stalled clients.** Two bounds, at the boundaries where the memory is actually held:

- **Write stall at the I/O boundary.** `ConnIo` fails a write that has made no progress for `write_stall_timeout` (30 s); hyper then ends the connection and drops its buffers and the response body. This bounds every reply (blobs, replay pages, snapshots, WebSocket frames) against a client that stops reading, and replaces a per-body transfer timer.
- **Admission before loading.** A reply estimated above 1 MiB takes one of `large_reply_slots` (4) permits **before** it loads: a blob by its stored `size`, a replay page by `cum_bytes` between its bounds (two `head` reads), a snapshot always. The permit is taken inside the read closure with `try_acquire_owned` (non-blocking); none free → 503 `busy` with `retry_after`. The permit moves into the response body, which yields the bytes as copied 64 KiB frames from the buffer it owns, so when hyper drops the body (sent, connection ended, or stalled out) the buffer is freed with the permit, and hyper holds at most its own write buffer beyond it.

A forward page is at most 8 MiB of payload (`decide_replay`'s byte bound); a backward page has no byte bound in mvp §2.4 and can reach `limit` × 64 KiB inline payloads (64 MiB), which is why admission covers replies of every kind, not only blobs. Memory held by large replies is at most `large_reply_slots` × 64 MiB plus hyper's per-connection buffers. A streaming blob reader (`rusqlite` `blob` feature) stays a later, schema-free change.

### 2.8 Commands and Receipts

**Envelope** (public, §8 OQ2): `POST /api/commands/{name}` with `CommandRequest { command_id: CommandId, args: <JSON object> }`; `args` that is not an object is 400 `bad_request`. `name` is one of `MVP_COMMANDS` (snake_case: `spawn`, `resume`, `send`, `ready`, `key`, `stop`, `close`, `input_resolve`, `input_cancel`, `task_set`, `mailbox_send`, `worktree_remove`, `doctor_run`, `surface_action`). Unknown name → 404; a kind without the command's grant → 403; a command whose PR has not merged → 501 `not_implemented`; all three decided by the guard before the body, with no receipt, so a retry after that PR merges runs normally.

**Request identity.** `SHA-256("chaldea-command-v1" ‖ 0x00 ‖ name ‖ 0x00 ‖ canonical(validated args))`. `validated args` is the handler's typed argument value (`parse` deserializes `args` into its own type, which refuses duplicate keys, unknown fields, and numbers outside the field's type, then re-serializes it to a JSON value); `canonical` writes that value with object keys sorted by UTF-8 bytes at every level and no whitespace. Two requests with the same meaning therefore have the same identity whatever their key order, whitespace, or number spelling (`1` and `1.0` for an integer field: the latter is refused by the integer type; for a float field both parse to the same `f64` and are one identity). The order is explicit rather than taken from `serde_json::Map`, which a `preserve_order` feature anywhere in the build graph would change. `command_id` is the receipt key, not part of the hash.

**Dispatch seam.**

```rust
pub trait CommandHandler: Send + Sync + 'static {
    fn name(&self) -> &'static str;                         // must be in MVP_COMMANDS; its action comes from there
    fn deadline(&self) -> Duration;                         // the request budget; 5 s default; long commands state theirs (mvp §2.5)
    /// Parses `args` into the handler's type (400 `bad_args`), returns the validated value for the identity
    /// and an intent job built from data only.
    fn prepare(&self, args: &serde_json::Value) -> Result<Prepared, ApiError>;
    /// After the intent committed, for synchronous side-effect commands; runs on the runner, not the request.
    fn effect(&self, run: EffectRun) -> Option<BoxFuture<'static, CommandResult>> { None }
}
pub struct Prepared {
    pub validated: serde_json::Value,
    /// Runs inside the intent job after the receipt lookup: object authorization, then the intent's writes.
    pub intent: Box<dyn FnOnce(&mut WriteTx<'_>, &Authorized, &mut CommandCtx) -> Result<Intent, JobError> + Send>,
}
pub enum Intent { Completed(CommandResult), Pending(EffectRun) }
pub struct CommandRegistry { /* name → Box<dyn CommandHandler> or Unimplemented */ }
impl CommandRegistry {
    pub fn mvp() -> CommandRegistry;                        // every MVP_COMMANDS entry, Unimplemented in PR 7
    pub fn register(&mut self, h: Box<dyn CommandHandler>); // PR 10, 16, 20, 21 replace their entries
}
```

The intent closure is built by `prepare` from the parsed arguments alone; the handler object (which may hold the supervisor) is used only by `effect` on the runner. This keeps store commitment 5 for the intent job; that the closure captures no blocking handle is a review rule (store §5), since Rust cannot see what a closure captured.

**`CommandRunner`.** A command's life after the guard belongs to one task spawned through `Tracked`, not to the request future:

1. The handler reads the body (`body_within`), calls `prepare`, computes the identity, and starts the runner with `{ principal, command_id, identity, name, prepared, deadline }`, receiving a `watch::Receiver<Progress>` (`Submitted` → `IntentCommitted` → `Done(reply)`).
2. The runner submits **the intent job** (`not_after = deadline`; one `BEGIN IMMEDIATE` transaction, the single deciding point): `verify_hash_in` (invalid → `Refused(401)`, nothing written); `tx.read().receipt(command_id)` → `check_receipt(existing, token_id, identity)`, and anything but `New` returns that lookup with nothing written; `prepared.intent(…)`: object authorization first (`deny_for` → `Refused`), then `receipt_begin` (inserts `in_progress`) and the intent's writes, and for `Intent::Completed(r)` also `receipt_complete(r)`.
3. The runner owns the ticket and awaits it without the request deadline (the writer replies to every accepted job). Fatal outcomes go to `LifecycleSink::store_fatal` here, whether or not a request still listens.
4. On `Ok(Pending(run))` it publishes `IntentCommitted`, starts `effect(run)` once, awaits it, and then **completes the receipt** with its own jobs: `receipt_complete(command_id, &r)` with `not_after = now + 5 s` per attempt, retried on `Expired`, `Busy`, `QueueFull`, and `RolledBack(Busy)` with backoff for up to 60 s, with no token check (the effect has run; a caller revoked meanwhile, possibly by the command itself, does not undo it). It publishes `Done(r)` only after that job commits. If completion cannot commit (store closed or failed), the receipt stays `in_progress`, which a restart turns into `unknown_after_crash` (mvp §2.4).
5. Every other outcome publishes `Done(mapped reply)` at once.

The request waits on the `watch` until its deadline and replies:

| Progress at reply time | Reply |
|---|---|
| `Done(Lookup::InProgress)` | 409 `in_progress`, `retry_after` = 1 s |
| `Done(Lookup::Replay(r))` | `r.http_status` and `r.body` byte for byte |
| `Done(Lookup::Conflict)` | 409 `conflict`: one body whatever the receipt's state |
| `Done(Lookup::UnknownAfterCrash)` | 409 `unknown_after_crash` |
| `Done(Completed(r))` (in the intent job, or by the runner after the effect) | `r` |
| `Done(Expired)` | 408 `request_timeout`: this attempt's deadline passed before its intent job held the lock (store §2.2 step 3) |
| `Done(Refused(e))` | `e` (401, 403, 404, or the command's own refusal such as 409 `card_closed`) |
| `Done(Busy)`, `QueueFull`, `RolledBack(Busy)` | 503 `busy`, `retry_after` |
| `Done(RolledBack(DiskFull))` | 503 `disk_full` |
| `Done(RolledBack(_))`, `Rejected(Store(_))`, `Panicked` | 500 `internal` |
| `Done(SubmitError::Closed)` | 503 `shutting_down` |
| `Done(SubmitError::Failed)`, `Done(Failed(_))` | 503 `store_failed` |
| `Done(CommitUnknown)`, `Done(WriterGone)` | 503 `outcome_unknown` |
| `IntentCommitted` (effect still running) | 409 `in_progress`, `retry_after`: the receipt exists and is `in_progress` |
| `Submitted` (the writer has not replied by the deadline) | 503 `outcome_unknown`, `retry_after` (§8 OQ3) |

**Why 408 is exact.** The writer runs a job only if its deadline had not passed once it held the lock (store §2.2 step 3), and `Expired` is its only "deadline prevented the commit" reply; no layer drops a write handler (§2.2), and nothing after `IntentCommitted` maps to 408. A 408 describes **this attempt** only: an earlier attempt with the same `command_id` may have its own receipt, which the retry finds. The last row is the one case where PR 7 replies before knowing the intent's outcome: the job is still queued behind others, and it will either run (its receipt then exists) or expire (none exists); a retry with the same `command_id` is correct either way, and the client is told the outcome is not known rather than that a receipt exists.

**Concurrent duplicates.** Two requests with one `command_id` reach the writer as two jobs; the second runs after the first committed and reads its receipt (`InProgress`, `Replay`, or `Conflict`), so only one intent is ever written; the receipts primary key is the backstop for a second process (store §2.3). A synchronous command's retry while its effect runs gets 409 `in_progress`; after completion, `Replay`.

**Receipt visibility.** `check_receipt` returns `Conflict` for another token before looking at status (`crates/core/src/command.rs`), and `Conflict` has one body, so another token learns nothing about the command's state or result. It does learn that the `command_id` is taken (the same request with a fresh id would proceed); `command_id`s are random UUIDs chosen by their sender, so this reveals only that someone guessed an id in use. T3 Code's receipts (`persistence/Migrations/055_OrchestrationV2.ts:42-50`) key by `command_id` with no token or request hash; Chaldea adds both (mvp §2.4).

**What PR 7 completes end to end.** No mvp command: each needs its owning PR's state machine (PR 10: `spawn`, `resume`, `send`, `ready`, `key`, `stop`, `close`, `input_resolve`, `input_cancel`; PR 16: `task_set`, `mailbox_send`, `worktree_remove`; PR 20: `surface_action`; PR 21: `doctor_run`). PR 7's tests register test-only handlers under test-only names (a counter intent, a completed-in-intent handler, a slow-effect handler, a refusing handler, a worktree-owner check using `OwnWorktree`) through a test registry and a test `CommandSpec` table to prove the seam.

### 2.9 Status Mapping and the Error-Body Boundary

**Application decisions** (the outer layer, the guard, the router fallbacks, handlers, and the socket admission) answer `ErrorBody { code, retry_after? }` as `application/json` with `Cache-Control: no-store`; `retry_after` is omitted when absent (never `null`) and is whole seconds; codes are snake_case. Codes: `misdirected_request` (421), `origin_rejected` (403), `unauthorized` (401), `forbidden` (403), `not_found` (404), `method_not_allowed` (405, with `Allow`), `request_timeout` (408), `length_required` (411), `payload_too_large` (413), `bad_request`, `bad_query`, `bad_args` (400), `in_progress`, `conflict`, `unknown_after_crash` (409), `not_implemented` (501), `busy`, `disk_full`, `shutting_down`, `store_failed`, `outcome_unknown` (503), `internal` (500). Command handlers may add refusal codes of their own PRs (e.g. `card_closed`, mvp §2.10).

**Not application decisions**, and so not `ErrorBody`: hyper's own responses before the service runs (400 for a malformed head or `Content-Length`, 431 for an oversized head, EOF for a slow head; H2, H3), and a connection ended by the write stall or by shutdown. Extractor rejections are mapped explicitly, never left to axum's defaults: a `Path` rejection → 404 `not_found`, a `WebSocketUpgrade` rejection → 400 `bad_request`, a query that does not parse → 400 `bad_query`; JSON bodies are parsed by the handler, not by axum's `Json` extractor (the `json` feature is off).

**Credential responses** (`PairResponse`, `PairCodeResponse`, `WsTicketResponse`) also carry `Cache-Control: no-store`. The PWA's CSP and asset caching are PR 18's mount contract.

**Reads:** `ReadError::Busy` (no free reader before the deadline) and `ReadError::Store(Busy)` → 503 `busy`; `ReadError::Closed` → 503 `shutting_down`; any other `StoreError` → 500 `internal`; a `spawn_blocking` read still running at the deadline → 408 (no effect). **Writes:** §2.8's table, also used by `POST /api/pair` and token revocation, except that a write without a receipt never answers `in_progress`.

### 2.10 Secrets in Logs

- **Span:** `OuterLayer` creates one span per request, outermost, so 421s and 404s are spanned too, with `method`, `route = Empty`, `status`, `latency`, and `token_id = Empty`. The guard records `route` from `MatchedPath` (the template, e.g. `/api/blobs/{sha256}`), the fallbacks record `unmatched`, and step 4 records `token_id`. Never the URI, the query, header values, or bodies (H7 is why `TraceLayer` is not used).
- **Headers:** `SetSensitiveRequestHeadersLayer` marks `Authorization`, `Cookie`, and `Sec-WebSocket-Key`, so a `Debug` of the headers prints `Sensitive`.
- **Types:** `Secret`, `MintedToken`, `PairRequest`, `PairResponse`, `PairCodeResponse`, `WsTicketResponse`, `CommandRequest` (its `args` can hold prompt text) implement `Debug` with the secret or the arguments replaced; no error type carries a secret or a body.
- **Bodies:** no layer or handler logs a request or response body; `/hook` and `/api/pair` bodies are never in a log (mvp §2.5). This is structural: arbitrary body text cannot be recognised by a pattern, so the guarantee is that no code path formats a body.
- **Scrubber:** `trace::redact_secrets` replaces `chd_`, `chdt_`, and `chdp_` secrets in a string; PR 8's log formatter applies it to every message and field.
- **Panics:** a handler or socket task that panics ends its task; Rust's panic hook writes the panic message to stderr, outside `tracing`. PR 7 guarantees by type that no secret reaches a panic message through its own types (`Secret` has no `Display`, redacted `Debug`); PR 8 owns the process's panic hook and must route it through `redact_secrets` (§4). PR 7's claim covers what reaches `tracing`.

### 2.11 What Later PRs Plug In

| Seam | Type | Used by |
|---|---|---|
| Listener and lifecycle | `HttpServer::new`, `serve(TcpListener)`, `ServeHandle::shutdown(deadline)`, `LifecycleSink` | PR 8 |
| Token issuance and revocation | `TokenService::{mint, new_token, issue_owner, revoke}`, `nudge_revocation` | PR 8 (owner token file), PR 10 (launch tokens), PR 16, PR 20 |
| Commands | `CommandHandler`, `CommandRegistry::register`, `Prepared`, `Authorized`, `deny_for`, `ObjectScope` | PR 10, 16, 20, 21 |
| Hook route | `Mount::Write` for `POST /hook` (spec in `ROUTES`), `WriteCtx`, `ConnStart` | PR 9 |
| Attach | `Mount::Upgrade` for `GET /api/attach/{thread}`; the handler gets `AttachGrant { thread, token_id, token_sha256 }` and runs in a tracked socket task | PR 11 |
| Read routes | `Mount::Read` for `/api/adapters`, `/api/doctor`, `/`, `/assets/*` with `ReadCtx` | PR 10, 21, 18 |
| `POST /events` | `Mount::Write`, `ObjectScope::stored_surface` | PR 20 |

### 2.12 Single Deciding Points

| Operation several requests can hit at once | Decided by | Why a second caller cannot act |
|---|---|---|
| Duplicate `command_id` | the intent job: token check, receipt lookup, authorization, `receipt_begin`, and intent in one `BEGIN IMMEDIATE` transaction on the one writer | jobs run one at a time; the second reads the first's receipt; the primary key refuses a second process's insert (store §2.3) |
| A committed intent's effect and completion | its `CommandRunner` task (tracked) | only the runner holds the ticket and starts the effect, once; the request only watches |
| Token revocation racing a request | reads: the request's one snapshot runs `verify_hash_in` and the data reads; writes: the job runs it first | a revocation commits before or after the snapshot or job in the writer's order; no cached token state exists |
| Ticket redemption | `TicketStore`'s mutex: conditional `remove` after the handshake and slot checks | only the caller whose removal returned the entry has it; rejected attempts peek only |
| Pair-code exchange | `PairCodes`' mutex: conditional `remove`; the issuer check and the insert in one job | as above; the writer orders the insert against a revocation of the issuer |
| Ticket and pair-code caps | the same mutexes | counted and inserted under one lock |
| WS subscribe racing a commit notification | the socket's sequential loop; heads read after each set change; `last_sent` suppression | a commit is in the heads read or delivered afterwards with a newer head; older ones are dropped |
| Revocation racing an open socket | each socket's own read (`verify_hash_in`) at startup, on nudge, tick, subscribe, or resync | the nudge carries no decision; the database does |
| Fatal store outcome | the dispatcher or runner task that owns the ticket → `LifecycleSink` | the ticket has one owner; no request future holds it |
| Socket, connection, and large-reply caps | `Semaphore`s acquired before the upgrade, before `accept`, before loading | permits are atomic |
| Shutdown racing a new connection or socket | the accept loop's `select!` on the shutdown signal; `Tracked` holds every task | after the signal no stream is accepted; every task that outlives a request is in the set |

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **Serving loop** | A. `axum::serve` | axum `src/serve/mod.rs:388-396` | Rejected: no timer, so no header read timeout (H1), and no connection bound. |
| | B. Own accept loop on hyper's HTTP/1 builder with `TokioTimer`, `with_upgrades`, a write-stall I/O wrapper, and axum's accept-error rule | hyper `server/conn/http1.rs` (H2); axum `serve/listener.rs:140-165` (H13) | **Adopted.** |
| **Protocol** | A. HTTP/1.1 and HTTP/2 (auto) | axum's default build | Rejected: a second framing to test for B-8; the clients are local HTTP/1.1. |
| | B. HTTP/1.1 only | — | **Adopted.** Adding HTTP/2 later is additive. |
| **Where checks live** | A. Per-handler extractors | — | Rejected: a new route could forget one. |
| | B. Global `Router::layer`s that look up the route's rules | — | Rejected: rules separated from their route. |
| | C. An outer layer (span, start, Host) and one `GuardLayer` per route from its `RouteSpec`; the router built only from the table; an independent literal inventory tests the real router | — | **Adopted.** |
| **Write timeout** | A. `TimeoutLayer` on every route, writes marked by a flag | `src/timeout/service.rs:30-35` (H6) | Rejected: a wrong flag answers 408 while a job commits. |
| | B. Mount kinds: only `Mount::Read` gets a dropping timeout; only `Mount::Write` can obtain `WriteCtx` | store §2.2 step 3 | **Adopted.** |
| **After-commit ownership** | A. The request future awaits the ticket and runs the effect | — | Rejected: a disconnect or reply bound drops the only owner of a committed intent. |
| | B. A tracked `CommandRunner` per command owns the ticket, the effect, completion, and fatal reporting; the request watches it | — | **Adopted.** |
| **Reply before the outcome is known** | A. 409 `in_progress` | mvp §2.4 (for an existing receipt) | Rejected: a receipt may never exist. |
| | B. Wait for the writer's reply however long | store §5 "Waits" | Rejected: no request deadline. |
| | C. 503 `outcome_unknown` with `retry_after`, the same code as a known `CommitUnknown` | — | **Recommended**, §8 OQ3. |
| **Socket ownership** | A. axum's `on_upgrade` task runs the loop | axum `ws.rs:350-365` (H10) | Rejected: shutdown cannot see or abort it. |
| | B. A tracked task spawned before `101` receives the socket from the `on_upgrade` callback | — | **Adopted.** |
| **WS library** | A. axum `ws` (tokio-tungstenite 0.29 underneath) | axum `src/extract/ws.rs` | **Adopted**: upgrade handling, per-socket limits, automatic pongs (H8). |
| | B. `tokio-tungstenite` directly with hand-written upgrade | — | Rejected: duplicates the handshake; kept as the test client. |
| **Token check** | A. A memory cache of valid tokens | — | Rejected: cross-call state the database does not hold. |
| | B. Look up by hash in the transaction that uses it; principals and tickets carry the hash | mvp §2.5 | **Adopted**; no plaintext kept past its request, no `token_by_id` needed. |
| **Ticket form** | A. Signed stateless ticket | T3 Code `auth/SessionStore.ts:1049-1064`, `:1113` | Rejected: cannot be single use without a spent list. |
| | B. Random ticket in a memory map; peek at the credential step, conditional `remove` after the handshake and slot | — | **Adopted**; a rejected upgrade does not spend it. |
| | C. Tickets in SQLite | — | Rejected: a write per WebSocket connect for a 30 s credential. |
| **Pair-code authority** | A. Expiry only | — | Rejected: a code minted by a since-revoked owner would still confer owner access. |
| | B. Bound to the issuer's token hash, checked in the token-insert job | — | **Adopted** (with §8 OQ1). |
| **Pair code issuance** | A. A dedicated owner-only route (`POST /api/pair-codes`, Origin absent) | T3 Code pairing links (`Migrations/031_AuthAuthorizationScopes.ts:11-24`) | **Recommended**, §8 OQ1. |
| | B. A command `open` | — | Rejected: the receipt stores the reply body, i.e. the code. |
| | C. A ticket purpose `pair` | — | Rejected: browsers could mint, and the ticket route's 30 s and Origin rules differ. |
| **Notification fan-out** | A. One hub task fanning out to per-socket channels | — | Rejected: a second queue per socket with its own lag rule. |
| | B. Each socket subscribes to the store's broadcast, coalesces per stream, suppresses heads not newer than announced | store §2.2 | **Adopted.** |
| **Host stream for agents** | A. Serve it filtered | — | Rejected: filtered events break the gapless `seq`. |
| | B. 404; agents use `/api/threads` and `/api/worktrees` | — | **Adopted.** |
| **Large-reply bound** | A. A per-body transfer timer and a blob-only permit | — | Rejected: the body can be dropped while hyper still holds its bytes, and backward pages are as large as blobs. |
| | B. A write-stall bound at the connection I/O plus admission before loading for every reply over 1 MiB, with the permit freed with the buffer | — | **Adopted.** |
| **Command handler shape** | A. One HTTP handler per command | — | Rejected: receipt, deadline, runner, and mapping repeated 14 times. |
| | B. `CommandHandler` with a data-only intent closure and an optional effect on the runner | — | **Adopted.** |
| **Request identity** | A. Bytes as received | — | Rejected: two serializations of one request conflict. |
| | B. Canonical form of the raw `serde_json::Value` | — | Rejected: duplicate keys collapse and number spellings round silently. |
| | C. Canonical form of the handler's validated typed arguments | — | **Adopted.** |
| **PR size** | A. One PR | mvp §4 | Rejected: large and on the longest path. |
| | B. Two PRs (server and reads; commands) | — | Rejected (§8 D1). |
| | C. Three PRs: 7a, 7b, 7c | — | **Adopted** (§8 D1). |

## 4. Same-Class Inventory

Every later PR that adds a route, a command, a token use, or a WebSocket purpose, and the seam it uses (§2.11). None needs a change to PR 7's types.

| PR | Adds | Seam | What PR 7 provides |
|---|---|---|---|
| 8 lifecycle | starts and stops the server; owner token file; fatal store outcomes; panic hook | `HttpServer::new`, `serve`, `ServeHandle::shutdown`, `LifecycleSink`, `TokenService::issue_owner`, `trace::redact_secrets` | Startup order (PR 8): open store → `receipts_mark_unknown_after_crash` and launch recovery (PR 10's) → `serve`. Shutdown order: `ServeHandle::shutdown(budget)` first, then `Store::shutdown` with the same budget. PR 8 decides the response to `store_fatal` (stop), and installs a panic hook that writes through `redact_secrets`. |
| 9 hooks | `POST /hook` handler; hook token use | `Mount::Write` (spec already in `ROUTES`), `WriteCtx`, `ConnStart` | Host, `Origin` must be absent, `hook` kind only, framing, size, deadline before the handler; `verify_hash_in` checks revocation only; hook bodies never logged. B-8 cases on both OSes, including H5's drain case, are PR 9's. |
| 10 supervisor | `spawn`, `resume`, `send`, `ready`, `key`, `stop`, `close`, `input_resolve`, `input_cancel`; agent and hook tokens; `GET /api/adapters` | `CommandHandler`, `TokenService::{mint, new_token, revoke}`, `nudge_revocation`, `Mount::Read` | Guard (kind and action per command name), receipt, identity, runner, mapping, object-scope functions, `deny_for` |
| 11 attach | `GET /api/attach/{thread}` socket bytes | `Mount::Upgrade`, `AttachGrant` | ticket purpose `attach:<thread>`, owner-only at issue and redeem, single use, a tracked socket task |
| 16 agent control | `task_set`, `mailbox_send`, `worktree_remove` | `CommandHandler` | `SelfAndDirectChildren`, `ParentAndChildren`, `OwnWorktree`, `CreatedBySelf` grants |
| 17 `stateright` | the Sync model | — | the replay rules (§2.7) from 7b and the receipt and runner rules (§2.8) from 7c, cited by claim id |
| 18 PWA | `/`, `/assets/*`; pairing; ticket and events client | `Mount::Read` | pair flow, `ws-ticket`, `/api/ws`, `/api/events`, the public bodies (§8 OQ2), the subscription rule (§2.6) |
| 20 extensions | `POST /events`, `surface_action`, extension tokens with a scope, rescue | `Mount::Write`, `CommandHandler`, `TokenService` | kind check, `ExtensionScope`/`SurfaceScopeContains` grants, `ObjectScope::stored_surface`; the scope column is a v2 migration (store §2.4) |
| 21 doctor | `doctor_run`, `GET /api/doctor` | `CommandHandler`, `Mount::Read` | owner and rescue read, owner run |

## 5. Resource Paths

| Resource | Created by | Bound | Ends on |
|---|---|---|---|
| Listener | PR 8 (`serve` takes it) | one | `ServeHandle::shutdown`: the accept task stops at the signal and drops it |
| Connection task | accept loop, through `Tracked` | `max_connections` (128) permits, acquired before `accept` | client close; `header_read_timeout` (EOF); write stall; graceful shutdown at the signal; aborted at the shutdown deadline |
| Request handler | hyper per request | one per connection at a time | response; read mounts at the deadline (408); write mounts at their bounded waits; connection end |
| `spawn_blocking` read | `ReadCtx::read`, the guard's lookup, socket reads | the store's reader pool (4) and `wait_until` | `f` returns (not bounded by the store, store §5); the waiter stops at its deadline; at daemon shutdown the store abandons and reports it |
| Write dispatcher task | `WriteDispatcher` / `CommandRunner`, through `Tracked` | one per write | the writer's reply (store §5: every accepted job is answered); aborted at the shutdown deadline |
| Effect future | `CommandRunner` | one per synchronous command | its own completion (bounded by its owning PR); aborted with the runner at the shutdown deadline (the receipt stays `in_progress` → `unknown_after_crash` at restart) |
| Receipt completion | `CommandRunner` | one job per attempt, 5 s each, 60 s total | committed; budget exhausted; store closed or failed |
| WebSocket socket task | upgrade handler, through `Tracked`, before `101` | `max_sockets` (128), 16 per token | client close; send deadline (10 s); idle (75 s); token invalid (1008); `Closed` from the receiver or the shutdown signal (1001 within `close_grace`); aborted at the shutdown deadline |
| Commit notification receiver | each socket at start | the store's broadcast (1024) | dropped with its socket; `Lagged` → heads resync; `Closed` → close |
| Tickets | `POST /api/ws-ticket` | 1024 total, 32 per token, 30 s | redemption, expiry purge on insert, daemon exit |
| Pair codes | `POST /api/pair-codes` | 8, 60 s | redemption, purge on insert, daemon exit |
| Large reply (body and permit) | read handlers | `large_reply_slots` (4), ≤ 64 MiB each | body dropped by hyper (sent, connection ended, write stall, shutdown) |

**Endings.**

| Ending | What happens |
|---|---|
| Daemon shutdown (PR 8) | `ServeHandle::shutdown(deadline)`: the accept loop stops; connections get `graceful_shutdown`; sockets send 1001 and drop within `close_grace`; `Tracked` is awaited until `deadline`, the rest aborted and counted by kind. An aborted runner leaves its job to the writer (it runs or expires) and its receipt `in_progress` if the intent committed; restart makes it `unknown_after_crash`. PR 8 then shuts the store down with the same budget. |
| Store `Closed` | `submit` → 503 `shutting_down`; `read` → 503 `shutting_down`; sockets get `Closed` and close 1001. |
| Store failed, `CommitUnknown`, `WriterGone` | the owning dispatcher or runner reports to `LifecycleSink`; the reply (if anyone waits) is 503 `store_failed` / `outcome_unknown`; reads continue; PR 8 stops the daemon. |
| Lagged notifications | heads read for the socket's set (§2.6). |
| Client disconnect mid-request | hyper drops the handler future; a submitted write and any effect continue on their tracked task; a read finishes on its own. |
| Client stops reading | the write stall ends the connection after 30 s; a socket's send deadline drops it after 10 s. |
| Token revoked | requests: 401 at their next `verify_hash_in`; sockets: close 1008 on the nudge or within one `heads_interval`; outstanding tickets fail at redemption; outstanding pair codes of a revoked issuer fail at exchange. |

**Waits.** Head read: `header_read_timeout`. Credential lookup, body, read: the request `Deadline`. Write: `not_after = deadline`; the request's watch until the deadline; the dispatcher's ticket wait follows the writer's reply guarantee. Completion: 5 s per attempt, 60 s total. WebSocket: each send `send_timeout`; inbound `idle_timeout`; reads the short deadline; close `close_grace`. Writes to a client: `write_stall_timeout`. Ticket and pair-code mutexes are held only for a map operation. Shutdown: its deadline. Unbounded by PR 7: a started `spawn_blocking` read and a started writer job (store §5), and an effect future (its owning PR bounds it); PR 7 never makes a request wait on them past its own deadline, and shutdown aborts the tasks that await them.

**Windows and macOS.** The code has no `cfg`. Loopback behaviour differs only where H5 applies (a rejection while the client still sends a large body); PR 7's tests send small in-flight bodies (which hyper holds in its buffer) or none, and PR 9 records the per-OS outcome of the large-body drain case.

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked on a running daemon (PR 8's binary once it exists; until then the test server of `tests/support/http.rs` on `127.0.0.1:<port>`). `$T` is an owner token, `$A` an agent token, `$P` the port.

| ID | Criterion (EARS) | Source | How to observe |
|---|---|---|---|
| PR7-P-1 | If a request has a wrong or missing `Host`, a wrong method, a wrong `Origin`, a missing or wrong token, or a token kind or command the caller may not use, then the daemon shall reject it with 421, 405, 403, 401, or 403 and change nothing. | mvp P-20; §2.5 | `curl -i -H 'Host: evil:$P' http://127.0.0.1:$P/api/threads` → 421; `-X DELETE` → 405; `-H "Authorization: Bearer $T" -H 'Origin: http://evil'` → 403; no token → 401; hook token on `/api/threads` → 403; `$A` on `/api/commands/key` → 403; `GET /api/events?stream_id=host` with `$T` shows the same `head_seq` before and after. |
| PR7-P-2 | When a client replays a stream from `after_seq`, pages backward with `before_seq`, or sends a stale epoch, the daemon shall answer per mvp §2.4 with events, a snapshot, or 400/404. | mvp §2.4; PR 7 obligations | `curl -s -H "Authorization: Bearer $T" "…/api/events?stream_id=host&after_seq=0&limit=0"` → 400; `limit=1001` → 400; `after_seq=999999` → `snapshot`, `cursor_ahead`; `epoch=x` → `epoch_changed`; `after_seq=1&before_seq=5` → 400; unknown thread → 404. |
| PR7-P-3 | When a stream advances, a subscribed WebSocket client shall receive `stream_advanced` or, at the latest, a `heads` message within 30 s. | mvp §2.4; CL-13 | `curl -X POST …/api/ws-ticket -d '{"purpose":"events"}'`, then `websocat "ws://127.0.0.1:$P/api/ws?ticket=…"`, send `{"type":"subscribe","stream_ids":["host"]}`: `hello`, `heads`; a new thread → `stream_advanced`. |
| PR7-P-4 | When a ticket or pair code is used a second time or after its lifetime, or a pair code's issuer was revoked, the daemon shall refuse it. | mvp §2.5 | Second `websocat` with the same ticket → 401; after 31 s → 401; `POST /api/pair` twice with one code → second 401. |
| PR7-P-5 | When a command is retried with the same `command_id` by the same token, the daemon shall return the stored result and cause no second effect; another token shall get 409 `conflict`. | mvp CL-8; §2.4 | With a registered command (PR 10's first, or the test handler): `curl -X POST …/api/commands/<name> -d '{"command_id":"…","args":{…}}'` twice → equal status and body, one event; with `$A` → 409 `conflict`. |
| PR7-P-6 | When a blob is downloaded, the daemon shall serve its bytes only through a stream that references it, with the safe type and download headers. | mvp §2.5; P-21 | `curl -i "…/api/blobs/<sha>?stream_id=thread:<t>"` → bytes equal, `nosniff`, `attachment`; another stream → 404. |
| PR7-P-7 | While the daemon serves requests carrying tokens, tickets, and pair codes, its request log shall contain the request records and none of the secrets, queries, or `/hook` and `/api/pair` bodies. | mvp §2.5 "Secrets" | Run requests with known secrets; the log has one line per request with its route template; `grep -c 'chd_\|chdt_\|chdp_'` → 0. |

### 6.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| PR7-CL-1 | If a request's `Host` is missing, repeated, or not `127.0.0.1:<port>` / `localhost:<port>`, or its absolute-form target carries any other authority string (other host, other port, userinfo), then the daemon shall answer 421 before routing, authentication, or any body use, and change nothing. | mvp §2.5; P-20 | `http_layers::host_rejections` (each case on a known and an unknown path; store dump unchanged) |
| PR7-CL-2 | When a route checks `Origin`, the daemon shall accept it only per the route's rule (absent or exactly `http://` + the received `Host`; present and matching for `/api/pair`; absent for `/api/pair-codes`, `/hook`, and `/events`), and otherwise answer 403. | mvp §2.5; mvp §4 PR 7 "`Origin` vs received `Host`" | `http_matrix::origin_matrix` (every inventory route × {absent, matching for `127.0.0.1:<port>`, matching for `localhost:<port>` sent with the other `Host`, other port, `null`, `https://`, duplicate}; expectations from the literal inventory) |
| PR7-CL-3 | If a Bearer route gets no `Authorization`, several, another scheme, extra whitespace or characters, a malformed token, an unknown or revoked token, or an agent token whose launch is not current or has exited, then the daemon shall answer 401 with nothing changed; a failed token lookup shall answer 503 or 500, never 401. | mvp §2.5; P-20 | `http_layers::bearer_rejections` (each case; `agent_token_after_launch_exit` inserts `launch_exited` without revoking); unit `auth::tests::lookup_failure_is_not_unauthorized` (reader pool exhausted → 503) |
| PR7-CL-4 | The real router shall serve exactly the routes of mvp §2.5 (plus OQ1's), each under exactly its method, with `HEAD` and every other method answered 405 `method_not_allowed` before authentication; for every route, every token kind (and none), and every command name, the outcome shall be the inventory's: handler reached, 401, 403, 404, or 501. | CL-15; mvp §4 PR 7 "every route × token kind cell" | `http_matrix::inventory_matches_router` (an independent literal list of method, path, Origin rule, credential in `tests/http_matrix.rs`, probed against the production router: a missing route is 404, a changed rule fails its cell), `http_matrix::methods_are_exact` (`HEAD`, `PUT`, `DELETE` on every route: 405 with `ErrorBody`, `Allow`, before auth), `http_matrix::route_kind_matrix` (inventory × {owner, agent, rescue, hook, extension, none} through the Bearer path), `http_matrix::command_matrix` (every `MVP_COMMANDS` name × kinds: agent on `key`, `ready`, `input_resolve`, `doctor_run`, `surface_action` → 403, owner and agent on their granted commands → 501 in PR 7; an unknown name → 404), `http_matrix::credential_paths` (ticket routes with a Bearer header and no ticket → 401, with a ticket minted per kind; `/api/pair` with a Bearer header and no code → 401), `http_matrix::build_router_rejects_inconsistent_mounts` |
| PR7-CL-5 | The action table and `MVP_COMMANDS` shall equal mvp §2.5's "Actions" table and §2.4's command list cell by cell, and object scopes shall follow their definitions on any thread tree; an agent may remove only a worktree whose current owner is itself. | CL-15; mvp §2.5, §2.6 | `core::auth::tests::grant_table_matches_mvp`, `…::mvp_commands_match`, proptest `…::self_plus_is_the_descendant_closure`, `…::scope_grants_on_random_trees`, `…::own_worktree_only` (A-owned allowed; B-owned with B a child of A refused; transferred to A after B's close allowed) |
| PR7-CL-6 | When a request is rejected at the Host, method, Origin, credential, kind, action, framing, or size step, the daemon shall answer without the application polling or using the body, in that order. | mvp §2.5 "Order", "Rejections" | `http_layers::rejections_precede_body` (head sent, body withheld until the response is read, one case per step), `http_layers::rejections_with_body_in_flight` (head and a small body sent together; the decided status arrives; the handler stub records no call), `http_layers::layer_order` (requests failing several steps get the earliest step's status) |
| PR7-CL-7 | If a POST has `Transfer-Encoding` (with or without `Content-Length`), has no `Content-Length`, or declares more than 4 MiB, or a GET has a nonzero `Content-Length`, then the daemon shall answer 400, 411, 413, or 400; a malformed `Content-Length` shall get hyper's 400 and an oversized head 431 without the service being called; a head not completed within `header_read_timeout` shall end the connection with EOF. | mvp §2.5 rejections; spike B-8; H2–H4 | `http_layers::te_with_content_length_is_400`, `http_layers::framing_and_size`, `http_layers::get_with_body_is_400`, `http_layers::parser_layer_cases` (`Content-Length: +5`, two lengths, 9 KiB of headers, 65 headers; counting stub), `http_layers::slow_head_is_closed` |
| PR7-CL-8 | The request deadline shall start when the head is complete, before the credential lookup, and bound the lookup, the body, and the write admission together; a read mount past it shall answer 408; a write mount shall never be dropped by a timeout, and 408 shall mean only that this attempt's intent job did not run. | mvp §2.5 deadlines | `http_layers::deadline_is_cumulative` (lookup held 3 s at a fault point, then a body trickled: 408 at 5 s from the head, no receipt), `http_reads::read_past_deadline_is_408`, `http_commands::held_after_writer_admission_is_not_408` (the intent job held inside the writer past the deadline through the production router: 503 `outcome_unknown`, 409 `in_progress`, or the result, never 408), `http_matrix::build_router_rejects_inconsistent_mounts` (a write route mounted as read) |
| PR7-CL-9 | When `GET /api/events` is asked, the daemon shall answer per `decide_replay` from one read snapshot, with exact events and flags: 400 for `limit` 0 or 1001, `after_seq` with `before_seq`, and malformed, repeated, or unknown parameters; 404 with one body for an unknown or unreadable stream; `replay`, `events: []`, `head_seq: 0`, both flags false for an empty stream; an empty replay at `after_seq = head_seq`; a replay at 10 000 remaining events and a snapshot at 10 001; a replay at exactly 8 MiB of remaining payload and a snapshot one byte above; a snapshot for `after_seq` above the head and for an epoch mismatch; backward pages of the nearest events with exact flags, empty at `before_seq = 1`, the latest page for `before_seq` above the head, carrying the server epoch without switching mode on a mismatch; and stored rows of an unknown kind or newer `schema_version` returned unchanged. | mvp §2.4; mvp §4 PR 7 obligations; CL-13 | `http_events::forward_cases`, `http_events::backward_cases` (each asserts `stream_id`, exact event ids, `head_seq`, `has_more`, `has_more_before`), `http_events::event_bound_edges`, `http_events::byte_bound_edges` (payload sizes chosen so the remaining `cum_bytes` is exactly 8 MiB and 8 MiB + 1), `http_events::unreadable_equals_unknown`, `http_events::unknown_kind_passes_unchanged` (rows inserted raw through the store's test helper; bytes equal) |
| PR7-CL-10 | When events are appended while a client pages forward from 0 toward a finite catch-up target (appends stop at a known head) within the replay bounds, the union of its replay pages shall equal the stream's events up to that head, each once. | CL-13; mvp §4 PR 7 "append between pages" | `http_events::append_between_pages` (a writer appends N events while the client pages with `limit` 7; total stays below both bounds; the union equals the stream's ids in order) |
| PR7-CL-11 | A thread snapshot shall equal, field by field, the values computed independently from the stream's events at `as_of_seq`, including the latest `vt_snapshot_taken` exactly while an attention cause is active or a card is open and none otherwise; a host snapshot shall list the threads, worktrees, and host- and workspace-scoped surfaces current at `as_of_seq`, all from one snapshot. | mvp §2.4 snapshot; mvp §4 PR 7 "snapshot contents restore every rendered field" | `http_events::snapshot_equals_model` (a history that sets every field, the expected value computed by the test's own model; exhaustive destructuring so a new field fails to compile), `http_events::vt_snapshot_branches` (attention active, card open, both cleared), `http_events::host_snapshot_surfaces` (host and workspace scopes, a removed surface absent), `http_events::snapshot_as_of_is_one_snapshot` (concurrent appends; every value matches the model at the reply's `as_of_seq`) |
| PR7-CL-12 | An agent token shall read only self+ streams, threads, worktrees owned by self+, and blobs referenced by those streams, and get 404 on the `host` stream; owner and rescue tokens shall read every stream, thread, and worktree, human-owned included; WebSocket subscriptions shall keep only readable streams. | CL-15; mvp §4 PR 7 "per-resource filtering" | `http_reads::agent_sees_self_plus_only` (parent, agent, child, grandchild, sibling; each route), `http_reads::rescue_and_owner_see_all`, `http_ws::subscription_drops_unreadable` |
| PR7-CL-13 | When a blob is requested, the daemon shall serve it only for a readable stream with a `blob_refs` row, with the store's type only if it is `text/plain; charset=utf-8` or `application/json` and `application/octet-stream` otherwise, and the headers `X-Content-Type-Options: nosniff`, `Content-Disposition: attachment`, `Cache-Control: no-store`. | mvp §2.5 blobs; store commitment 3 | `http_blobs::served_only_through_a_referencing_stream`, `http_blobs::type_rule` (text, JSON, `text/html`, disagreeing types), `http_blobs::bad_query_is_400`, `http_blobs::bytes_equal` |
| PR7-CL-14 | A ticket shall be redeemable once, before its expiry sampled at removal, only while its token is valid, only for its purpose (and its thread for `attach:`), and an `attach:` ticket only for an owner token; a request rejected for its handshake, framing, or a full socket cap shall not spend a valid ticket; of concurrent valid redemptions exactly one shall succeed; a ticket query other than exactly one `ticket` shall be refused. | mvp §2.5; mvp §4 PR 7 "ticket reuse and expiry" | `http_ws::ticket_single_use`, `http_ws::ticket_expiry_boundary` (injected clock at 29.999 s and 30 s, sampled at removal), `http_ws::ticket_dies_with_token`, `http_ws::ticket_purpose_mismatch`, `http_ws::attach_ticket_owner_only`, `http_ws::rejected_upgrade_keeps_ticket` (missing `Upgrade` header, a GET body, sockets full; then a valid attempt succeeds), `http_ws::concurrent_redemption_once` (32 valid requests), `http_ws::ticket_query_rules` |
| PR7-CL-15 | A pair code shall exchange once, before its expiry, only with a matching `Origin`, for one new owner token, and only while its issuing token is valid in the token-insert transaction; concurrent exchanges of one code shall yield one token. | mvp §2.5 secrets; §8 OQ1 | `http_pair::single_use`, `http_pair::expires`, `http_pair::origin_required`, `http_pair::concurrent_exchange_once`, `http_pair::issuer_revoked_before_exchange` (401, no token), `http_pair::issuer_revoked_after_exchange` (revocation job queued after the insert job: the new token stays valid), `http_pair::lost_after_restart` |
| PR7-CL-16 | When a WebSocket opens, its token shall be verified before `hello`, and `hello` shall carry the store epoch; each `subscribe` shall be answered with the heads of its readable streams; a commit on a subscribed stream shall produce a `stream_advanced` with a newer head; no frame shall announce an unsubscribed stream or a head not newer than one already announced; a socket whose notifications were lost or lagged shall receive the current heads within `heads_interval`. | mvp §2.4; mvp §4 PR 7 "lost notification recovered by `heads`"; CL-13 | `http_ws::hello_then_heads`, `http_ws::commit_is_announced`, `http_ws::lost_notification_recovered_by_heads` (fault point drops notifications), unit `ws::tests::lag_with_buffered_old_notifications` (loop held while more than 1024 commits land; old buffered notifications after the resync heads are suppressed), unit `ws::tests::resubscribe_clears_pending`, proptest `http_ws::subscribe_racing_commits` (interleaved subscribes and commits; every announced head per stream increases and the client's final heads equal the store's) |
| PR7-CL-17 | When a socket's token is revoked, the socket shall close with 1008 at startup, on the nudge, or at the latest at the next heads time, even while a send is pending; at shutdown, sockets shall close with 1001 within `close_grace` and any socket still running at the shutdown deadline shall be aborted and counted; a socket that does not read shall be dropped after `send_timeout`, and one with no inbound frame after `idle_timeout`. | mvp §2.5 "die with their token"; §5 | `http_ws::revocation_closes_socket` (with and without the nudge; a fresh commit in the window is announced at most as a head number), unit `ws::tests::revoked_between_admission_and_startup` (fault point between the admission read and the startup read), unit `ws::tests::control_deadlines_fire_during_a_pending_send`, `http_ws::shutdown_aborts_a_blocked_socket` (send held pending; shutdown budget shorter than `send_timeout`; reported as aborted), `http_ws::idle_socket_dropped` |
| PR7-CL-18 | One `command_id` shall cause at most one intent, and a repeat by its token with the same request identity shall return the stored status and body byte for byte; another token or another identity shall get 409 `conflict` with one body; an `in_progress` receipt 409 `in_progress` with `retry_after`; an `unknown_after_crash` receipt 409 `unknown_after_crash`. | CL-8; mvp §2.4; mvp §4 PR 7 "receipt HTTP codes" | `http_commands::concurrent_duplicates_one_intent` (32 concurrent requests, counter handler: one event), `http_commands::replay_is_byte_equal`, `http_commands::other_token_conflicts` (receipt in each state: bodies equal), `http_commands::other_identity_conflicts`, `http_commands::in_progress_while_effect_runs`, `http_commands::unknown_after_crash` |
| PR7-CL-19 | Once an intent commits, its effect shall start exactly once and its receipt shall be completed by the runner, independently of the request: after a client disconnect before the outcome, after the reply deadline, under writer contention, and after the caller's token was revoked during the effect; no reply after the intent commit shall be 408; a stored result shall be returned only after its completion committed. | CL-8; mvp §2.4, §2.5; store commitment 1 | `http_commands::disconnect_before_outcome` (client closes while the intent job is queued; the effect runs once; a retry replays), `http_commands::late_successful_intent` (writer delayed past the request deadline: 503 `outcome_unknown`, then the intent commits and the effect runs; a retry replays), `http_commands::completion_under_contention` (a test job saturates the queue during completion; completion retries and commits), `http_commands::revoked_during_effect` (effect revokes the caller; the receipt completes), `http_commands::expired_before_lock_is_408` (a test job holds the writer past the deadline: 408, no receipt, no effect) |
| PR7-CL-20 | Every fatal store outcome a write meets (`CommitUnknown`, `WriterGone`, `Failed`) shall reach `LifecycleSink` whether or not a request still waits, and a waiting request shall get 503 `outcome_unknown` or `store_failed`; after a restart the command's retry shall find the receipt absent, completed, or `unknown_after_crash`. | store commitment 2; mvp §4 PR 8 | unit `commands::tests::late_commit_unknown_reaches_the_sink` (waiter dropped; the store's fault point returns `SQLITE_IOERR` after the real `COMMIT`), unit `commands::tests::store_outcomes_map_to_statuses` (every `WriteError` and `SubmitError` through the writer's fault points), `http_commands::retry_after_restart_outcomes` (the three receipt states set up through the store, then a new server: New, `Replay`, 409 `unknown_after_crash`) |
| PR7-CL-21 | If a command's token is revoked before its intent job runs, or its object check fails, then the job shall write nothing, start no effect, and reply 401, or 404 for an object the token cannot read and 403 for one it can read but not act on. | CL-15; mvp §2.5 "Order" | `http_commands::revoked_between_guard_and_job`, `http_commands::object_denials` (unreadable target 404, readable but refused 403, an agent removing a child's worktree 403; no receipt, effect counter 0) |
| PR7-CL-22 | Every command of mvp §2.4 shall have a fixed action from 7a on; an unknown name shall be 404, a kind without the command's grant 403, and a registered unimplemented command 501, all before the body and with no receipt; `args` that is not an object shall be 400. | mvp §2.4 commands; CL-15 | `http_commands::every_mvp_command_has_an_action`, `http_commands::unimplemented_is_501_without_receipt`, `http_commands::kind_refused_before_body` (body withheld), `http_commands::args_must_be_an_object` |
| PR7-CL-23 | The command request identity shall be equal for requests whose `args` mean the same validated value whatever their key order, whitespace, or number spelling, and different for any other change, including the command name; duplicate keys shall be refused. | mvp §2.4 | `commands::tests::identity_is_canonical` (proptest over the test handler's argument type with shuffled keys and number spellings), `commands::tests::identity_covers_name_and_values`, `commands::tests::duplicate_keys_refused` |
| PR7-CL-24 | While the daemon serves requests carrying tokens, tickets, and pair codes, the `tracing` records shall include one record per request with its route template (or `unmatched`) and status, and none shall contain a secret, a query string, or a `/hook` or `/api/pair` body; the scrubber shall replace every secret pattern it is given. | mvp §2.5 secrets; mvp §4 PR 7 "secrets absent from logs" | `http_logs::secrets_absent` (a capturing subscriber at `TRACE`; positive control first: records for `/api/threads`, `/api/ws`, `/api/pair`, `/hook`, and `unmatched` exist with their status; then requests with a token in `Authorization`, a ticket in the query, a pair code, and a hook body with unique markers, including rejected requests; no marker appears), `http_logs::span_is_outermost` (a 421 and a 404 have records), `trace::tests::redact_secrets` |
| PR7-CL-25 | Tokens shall be stored only as SHA-256 hashes: no plaintext token, ticket, or pair code shall appear in the database files. | mvp §2.5 | `http_tokens::no_plaintext_at_rest` (issue tokens of each kind, pair once, mint tickets; shut the store down; scan the `.db`, `-wal`, and `-shm` bytes for every secret: none found) |
| PR7-CL-26 | When a resource cap is reached, the daemon shall answer as stated: tickets (1024, 32 per token), pair codes (8), sockets (128, 16 per token), and large replies (4) → 503 `busy` with `retry_after`; subscriptions over 1024 → close 1008; inbound frames over 64 KiB → close 1009; binary or invalid frames → close 1003; and the next request after a slot frees shall succeed. | §2.4, §2.6, §2.7, §5 | `http_caps::each_cap_at_its_boundary` (the last allowed and the first refused for each), `http_caps::large_reply_slot_frees_after_a_stalled_client` (a real client that never reads a blob: the connection ends at the write stall and the next download acquires the slot), `http_server::shutdown_with_connection_permits_exhausted` |
| PR7-CL-27 | `ObjectScope::stored_surface` shall allow an update or delete only when the stored surface is in the caller's scope, and refuse when it is not or does not exist. | mvp §2.5 "Order"; CL-15 | unit `auth::tests::stored_surface_scope` (thread, workspace, host scopes against owner, rescue, extension principals) |
| PR7-CL-28 | When `ServeHandle::shutdown` runs, no new connection shall be accepted and it shall return by its deadline with every tracked connection, socket, and runner finished or aborted and counted; finished tasks shall be reaped while serving; transient accept errors shall not end the listener. | §5 | `http_server::shutdown_stops_accepting`, `http_server::shutdown_returns_by_its_deadline` (a request, a socket, and a runner each held past the deadline), `http_server::tasks_are_reaped`, unit `listener::tests::accept_error_continues` (an injected accept error) |
| PR7-CL-29 | Read routes shall map a store read that finds no free reader before the deadline, or a store that has begun shutting down, to 503 `busy` or 503 `shutting_down`, never to a success. | §2.9 | unit `error::tests::read_outcomes_map` (pool exhausted; store shut down) |

**Mapping.** mvp §4 PR 7 obligations: `limit` 0 and above max → CL-9; empty, unknown, unreadable stream → CL-9; `after_seq = head_seq` and above → CL-9; append between pages → CL-10; epoch mismatch forward and backward → CL-9; byte bound → CL-9; `after_seq` + `before_seq` → CL-9; `before_seq` at 1 → CL-9; lost notification recovered by `heads` → CL-16; snapshot contents → CL-11; every route × token kind cell → CL-4 (with CL-2, CL-3, CL-22); per-resource filtering → CL-12; `Origin` vs received `Host` → CL-2; ticket reuse and expiry → CL-14; secrets absent from logs → CL-24, CL-25; receipt HTTP codes → CL-18, CL-19, CL-20. mvp §6.2: CL-8 → PR7-CL-18, CL-19, CL-20 (PR 10 adds its own concurrent duplicates); CL-13 → PR7-CL-9, CL-10, CL-11, CL-16 (with PR 17's model and PR 18's reducer); CL-15 → PR7-CL-4, CL-5, CL-12, CL-21, CL-22, CL-27 (PR 20 adds the rescue and scope cases). mvp P-20 → PR7-CL-1, CL-3, CL-4, CL-6. CL-16 (B-8 per layer) stays PR 9's; PR7-CL-6 and CL-7 cover the same steps for PR 7's routes.

**Known limits** (no test, by design):

- **Unannounced revocation window**: a revocation committed without a nudge reaches an open socket at its next read, at most one `heads_interval` later; meanwhile the socket can send only head numbers of streams the token could already read (§2.6). Every PR 7 revoking path nudges; the window concerns only a later PR's revoking job that does not.
- **Large in-flight bodies on Windows** (H5): a client that keeps sending a large body after a pre-body rejection may see a reset; PR 9 records it with the B-8 drain case, and PR 7's contract is the decided status.
- **Intent closures' captures** are a review rule (store §5): Rust cannot see what a closure captured.
- **Panic output** goes through PR 8's panic hook, outside PR 7's `tracing` claim (§2.10).
- **Effects that never resolve** keep their receipt `in_progress` until a restart makes it `unknown_after_crash` (mvp §2.4); the owning PR bounds its effect.

## 7. Tests and Completion Commands

**Harness.** `tests/support/http.rs` starts an `HttpServer` on a `127.0.0.1:0` listener with a store in a `tempfile::TempDir` (fixed `Clock` and counter `IdSource` from the store's test support), an injectable monotonic clock for tickets and pair codes, `heads_interval` of 200 ms, a recording `LifecycleSink`, and helpers to mint tokens of each kind and build thread trees through the store's `submit`. Requests use a raw HTTP/1.1 client over `tokio::net::TcpStream`, so malformed heads, withheld or in-flight bodies, non-reading clients, and EOF are observable exactly; WebSocket tests use `tokio-tungstenite`. Each test shuts the server and the store down before its directory drops. Fault points (`#[cfg(test)]` hooks: hold the guard's lookup, hold a read, hold a socket loop or its send, drop a socket's notifications, inject an accept error) are crate-internal, as the store's are; tests needing them are unit tests in `src/http/tests/`, and they also use the store's writer fault points. Concurrency cases use barriers or fault points, never sleeps, for ordering.

**Matrix tests against an independent inventory.** `tests/http_matrix.rs` holds a literal inventory written from mvp §2.5 (method, path, Origin rule, credential kinds, action) and `MVP_COMMANDS`' names written out again; the tests probe the production router built by `Mounts::mvp()`, so deleting a route from both `ROUTES` and its mount, or changing a route's Origin rule or kinds, fails a cell. The action table has its own literal test (CL-5). `ROUTES` stays the production source; the inventory is the test's.

**Contract fixtures.** `core::http`'s types join the drift test of `packages/contracts`. `tests/http_contract.rs` captures one HTTP response per `ErrorBody` code, per credential body, and per list body from the running server and checks that each decodes into its exported type and validates against its generated JSON Schema; it also captures hyper's own 400 and 431 and checks that they are not `ErrorBody` (the boundary of §2.9).

**Regression rule.** Each named test is seen failing with its behaviour reverted (the Host check removed, the ticket conditional removal replaced by an unconditional one at the peek, the receipt lookup skipped, the runner replaced by awaiting in the request, completion re-verifying the token, a write route mounted as read, `last_sent` suppression removed, the span replaced by `TraceLayer`'s default, `OwnWorktree` widened to self+) and passing with it restored before the PR merges; the PR description lists the reverts. hyper-owned parsing (H3) is checked by integration assertions against the pinned version, not by reverts.

Completion commands:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum|hyper|tower|getrandom) ' <<<"$deps"
deps="$(cargo tree -p chaldea-pty-runtime -e normal --prefix none)" && ! grep -Eq '^tokio ' <<<"$deps"
```

plus the `workspace` job green on `macos-latest` and `windows-latest`, the contract drift test regenerated for `core::http`'s types (§8 OQ2), and the `pty-hooks` job unchanged.

## 8. Decisions

**D1. PR 7 ships as three PRs.** **7a** (server and access): `HttpServer`, the accept loop, `Tracked`, `OuterLayer`, `GuardLayer`, `ROUTES` and `Mounts::mvp()` with every route mounted (handlers 501), `MVP_COMMANDS` and `core::auth`, `TokenService` and `verify_*_in`, `ReadCtx`/`WriteCtx`/`WriteDispatcher`, `LifecycleSink`, the status mapping, the span and redaction, `ServeHandle::shutdown`, the public bodies of OQ2; claims CL-1–CL-7, CL-27–CL-29, the Bearer and command-name cells of CL-4, and the guard, read, and route cases of CL-8, CL-22, CL-24, and CL-25; their remaining cases (commands, `args`, pairing, tickets) run in the part that adds those handlers. **7b** (reads and live): `/api/events`, `/api/threads`, `/api/worktrees`, blobs and large replies, tickets, pairing, the WebSocket loop; CL-9–CL-17, CL-26, and the ticket and pair-code cells of CL-4. **7c** (commands): identity, `CommandHandler`, registry, `CommandRunner`, receipts; CL-18–CL-21, CL-23. PR 8 and PR 9 are siblings that each depend on PR 7 (mvp §4); both need only 7a and start once it merges, while 7b and 7c proceed in parallel with them. PR 10 depends on 8 and 9 and also needs 7c; PR 17 consumes the receipt rules and needs 7c (and 7b's replay rules); PR 18 needs 7b. 7c is on the longest path only if it takes longer than PR 8 and PR 9. The two-way split was rejected because its first half would hold PR 8 and PR 9 behind the sync API and WebSocket, which neither needs.

**D2. Own accept loop on hyper's HTTP/1 builder with a timer**, a 5 s header read timeout, 8 KiB heads, 64 headers, 128 connections, a 30 s write-stall bound, reaping while serving, and axum's accept-error rule; HTTP/1.1 only.

**D3. Access is decided by an outer layer and one per-route guard built from a static route table**, which is the only production way to build the router; an independent literal inventory tests the real router. Order: Host → (routing) → method → Origin → credential → kind and action → framing (Transfer-Encoding first) → size → unimplemented → deadline; no application body use before the end. Mount kinds, not flags, decide which routes may write and which may time out by dropping.

**D4. Tokens are 256-bit `chd_` secrets stored as SHA-256 and checked by hash at use time in the transaction that uses them**, with a `subtle` final comparison; after a request, only the id and hash travel. Agent tokens also need their launch current and not exited. Tickets (`chdt_`, 30 s) and pair codes (`chdp_`, 60 s, bound to their issuer) are memory-only, peeked at the credential step and spent by conditional removal; restart loses them safely.

**D5. A committed intent is owned by a tracked `CommandRunner`, not the request.** It runs the effect once and completes the receipt with its own budget and no token check; every write reports fatal store outcomes to `LifecycleSink`. 408 means only that this attempt's intent job expired before the lock; an `IntentCommitted` reply is 409 `in_progress`; a reply before the writer answered is 503 `outcome_unknown` (OQ3).

**D6. Each WebSocket is a tracked task from before `101`**; it subscribes to the store's broadcast itself, verifies its token at startup and on every read and nudge, runs control deadlines independently of sends, coalesces heads per stream, and never announces an unsubscribed stream or a head not newer than announced. `subscribe` replaces the set; unreadable streams are dropped silently; the unannounced revocation window is one `heads_interval`.

**D7. Agents get 404 on the `host` stream** and read threads and worktrees through filtered routes; objects a token cannot read answer 404, objects it can read but not act on 403, decided by one `deny_for`. Agents remove only worktrees they currently own.

**D8. No mvp command is completed in PR 7**; all are registered with fixed actions and 501 handlers until their PRs, with test-only handlers proving the seam.

**D9. Logs carry route templates, never URIs, queries, header values, or bodies**; secrets have redacted `Debug`, a pattern scrubber is the second line, and the panic hook is PR 8's.

**D10. Large replies are bounded where their memory is held**: a write-stall bound at the connection I/O and admission before loading for every reply over 1 MiB.

### Open Questions

Each changes mvp §2.4–§2.5's public contract; the design is written for the recommendation.

**OQ1. A route for `chaldea open` to obtain a pair code, and the authority a code carries.** `chaldea open` must get a code the daemon accepts, and the route table has no route that mints one.

| Option | Gains | Losses | Reversibility |
|---|---|---|---|
| A. `POST /api/pair-codes`: owner Bearer, `Origin` must be absent, reply `{ code, expires_in }`; the code is bound to the issuing token and refused if that token is revoked before the exchange commits | explicit; only non-browser owner clients can mint; no secret at rest; a revoked owner cannot confer access | one more public route | additive; removable before PR 18 ships |
| B. As A without the issuer binding | simpler map | a code minted by a since-revoked owner still yields an owner token for 60 s | additive |
| C. Ticket purpose `pair` on `POST /api/ws-ticket` | no new route | browsers could mint; overloads the ticket route's lifetime and Origin rule | changes a public purpose set |
| D. A command `open` | uses the command seam | the receipt persists the reply body, i.e. the code | — |

Recommendation: **A**; add the row and the issuer rule to mvp §2.5 "Routes" / "Secrets"; `open` stays a CLI subcommand (mvp D2).

**OQ2. The public wire bodies and the error-body boundary.** mvp §2.4–§2.5 fix routes, codes, and messages but not these bodies: `CommandRequest { command_id, args: object }` and the snake_case command names; `ErrorBody { code, retry_after? }` (omitted when absent, whole seconds) with §2.9's codes (including 405 `method_not_allowed`, 501 `not_implemented`, and the 503 codes) and the boundary that hyper's own responses are not `ErrorBody`; `WsTicketRequest { purpose }` / `WsTicketResponse { ticket, expires_in }`; `PairCodeResponse { code, expires_in }`, `PairRequest { code }` / `PairResponse { token }`; `ThreadsResponse { epoch, threads }`, `WorktreesResponse { epoch, worktrees }`; `Cache-Control: no-store` on every API and credential response. Options: (A) a new `core::http` module with these types, exported with the drift test and checked against captured HTTP responses (§7); (B) untyped in the daemon, with PR 18 writing TypeScript by hand. A keeps one source of truth for the PWA and the relay at the cost of fixing names now; B defers naming but splits the contract. Recommendation: **A**, landing with 7a.

**OQ3. The reply when the writer has not answered by the request deadline.** The intent job is still queued; it will run (its receipt then exists) or expire (none exists), and a retry with the same `command_id` is correct either way. mvp §2.4 defines 409 `in_progress` for an existing receipt only.

| Option | Gains | Losses | Reversibility |
|---|---|---|---|
| A. 503 `outcome_unknown` with `retry_after`, the code also used for a known `CommitUnknown` / `WriterGone` | one meaning: "retry with the same `command_id`; the daemon does not know yet"; no new meaning for 409 | one more 503 code in the public vocabulary | additive |
| B. 409 `in_progress` | no new code | tells the client a receipt exists when none may ever exist | narrows later only by a contract change |
| C. No reply bound: wait for the writer | no unknown outcome | a request with no deadline, against mvp §2.5 | — |

Recommendation: **A**; add `outcome_unknown` to mvp §2.4 "`ReceiptLookup` and HTTP" as the reply for an intent whose outcome is not known at reply time.

### Where Other Documents Would Change

Proposals, not applied in this PR.

- `docs/mvp-design.md` §2.5 "Routes" and "Secrets": `POST /api/pair-codes` and the issuer rule (OQ1).
- `docs/mvp-design.md` §2.4 "Commands": the envelope, the snake_case names (OQ2), and 503 `outcome_unknown` for an intent whose outcome is not known at reply time (OQ3); that 408 describes the current attempt only.
- `docs/mvp-design.md` §2.5 "Actions" / "Order": the hiding rule (404 for unreadable, 403 for readable but refused), agents' 404 on the `host` stream (D7), and that `worktree remove` by an agent needs the worktree's current owner to be the agent itself, while listing stays self+.
- `docs/mvp-design.md` §4 PR 8: the `LifecycleSink` seam (stop on a fatal store outcome) and the panic hook through `redact_secrets`.
- `docs/mvp-design.md` §3: `core` gains `auth` (named there as "auth policy") and `http` (OQ2); `getrandom` and `subtle` are daemon dependencies (the list names `subtle` under `core`, which does not use it).
- `crates/core`: new modules `auth` (with `TokenKind` moved from the store) and `http`; no existing public type changes.
- `docs/design/event-store.md`: `TokenKind` is re-exported from `core::auth`; no schema change; the extension scope column is PR 20's v2 migration.
