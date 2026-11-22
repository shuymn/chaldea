# Design: HTTP Surface (MVP PR 7)

Status: design. Scope is the PR 7 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): HTTP route and action matrices, tokens, pairing, the sync API, blobs, WebSocket notifications, and command receipts, in `crates/daemon` on `axum` + `hyper` + `tower-http` (mvp §2.5, arch §3). The contract is mvp §2.4 (replay, backward history, live notifications, commands and receipts, restart), mvp §2.5 (common checks, routes, token kinds, actions, order, secrets, rejections), the PR 7 design obligations of mvp §4, and the claims of mvp §6.2 that name PR 7 (CL-8, CL-13, CL-15). It builds on the store of `docs/design/event-store.md` (cited as "store §N"). It proposes a split into three PRs (§8 D1).

**What PR 7 decides and what it leaves.** PR 7 owns everything between the socket and a store call: connection handling, the order of checks, credentials and their lifetimes, the authorization tables and the shared object checks, the mapping of store outcomes to status codes, the read routes, the WebSocket hub, and the command envelope, receipts, and dispatch seam. It does not decide daemon lifecycle, port, state directory, or the single-instance lock (PR 8), hook ingest and binding (PR 9), any command's effect (PR 10, 16, 20, 21), attach byte handling (PR 11), `POST /events` scope semantics and A2UI (PR 20), the doctor (PR 21), or the PWA (PR 18). For each of these it provides the seam named in §4.

**Commitments from PR 6 this design keeps** (store §2.2 "What callers learn", §2.5, §2.4, §5 "Waits"):

1. The store returns `SubmitError`, `WriteError`, and `ReadError` kinds and never chooses a status code; PR 7 owns the mapping (§2.9), including mvp §2.5's rule that a command answers 408 only if its deadline prevented the intent commit (§2.8).
2. A command whose intent job ended `CommitUnknown` has, after a restart, a receipt that is absent, `completed`, or `in_progress` (→ `unknown_after_crash`); PR 7 answers that request with 503 `outcome_unknown` and lets the retry after restart find out (§2.8).
3. The blob download type for `(stream_id, sha256)` is the store's (`application/octet-stream` on disagreement); PR 7 applies mvp §2.5's serving rule and headers (§2.7).
4. The v1 schema is frozen; PR 7 needs no schema change. The extension token's scope (PR 20) is a numbered migration (§4).
5. `Store::read` is synchronous: every async caller runs it in `spawn_blocking`; read closures and write jobs capture only data (ids, parsed queries, principals), never a network, PTY, or channel handle (§2.3).

Sources read for this document (paths are relative to each root):

| Source | Revision | Read for |
|---|---|---|
| This repository | `a08820b` | `docs/mvp-design.md`, `docs/architecture.md` §3–§4, `docs/design/event-store.md`, `crates/core/src/{sync,command,ids,lib,payload}.rs`, `crates/daemon/src/store/{mod,error,readers,rows,write_tx,clock}.rs`, `Cargo.toml`, `Cargo.lock`, `.github/workflows/ci.yml`, `docs/spikes/pty-hooks-spike.md` (B-8, F-3c) |
| T3 Code (`pingdotgg/t3code`) | `10586f9e` | `apps/server/src/orchestration-v2/ThreadStream.ts`, `auth/{utils,PairingGrantStore,SessionStore,EnvironmentAuth}.ts`, `persistence/AuthPairingLinks.ts`, `persistence/Migrations/{031_AuthAuthorizationScopes,055_OrchestrationV2}.ts`, `ws.ts` |
| `hyper` | crate 1.12.0 | `src/server/conn/http1.rs`, `src/proto/h1/{role,conn,dispatch}.rs`, `src/headers.rs`, `src/common/time.rs`, `src/error.rs` |
| `axum` | crate 0.8.9 | `Cargo.toml` (features), `src/serve/mod.rs`, `src/extract/ws.rs` |
| `tower-http` | crate 0.7.1 | `src/limit/service.rs`, `src/timeout/service.rs`, `src/trace/make_span.rs`, `src/sensitive_headers.rs` |
| `hyper-util` 0.1.21, `getrandom` 0.4.3, `subtle` 2.6.1 | crates | feature lists, `getrandom::fill` |

**Facts this design relies on.** T-facts are exercised by named tests of §7 on both runners; D-facts rest on the cited source and are not re-proven.

| # | Fact | Where | Kind |
|---|---|---|---|
| H1 | `axum::serve` builds a `hyper_util` auto connection builder per connection with no timer and serves it with upgrades; with no timer, hyper's default 30 s header read timeout is dropped with a warning. | axum `src/serve/mod.rs:388-396`; hyper `src/common/time.rs:73-85` | D |
| H2 | hyper's HTTP/1 server builder has `timer`, `header_read_timeout` (panics if set without a timer), `max_header_size`, `max_headers`, `keep_alive`; `serve_connection(..).with_upgrades()` is what lets an upgraded connection reach axum's `WebSocketUpgrade`. The header read timeout is armed whenever the connection starts reading a head, so it also bounds an idle keep-alive connection. | hyper `src/server/conn/http1.rs:201`, `:245-262`, `:341`, `:356`, `:370-373`, `:429`; `src/proto/h1/conn.rs:224-236` | T (`http_layers::slow_head_is_closed`, `http_ws::*`) |
| H3 | hyper rejects a `Content-Length` that is not all digits (`+5`), and two different values, as a header parse error answered 400 before the service is called; a head over the size or count bound is 431. | hyper `src/headers.rs:101-125`; `src/proto/h1/role.rs:295-312`, `:493-500`; `src/error.rs:513`, `:637-638` | T (`http_layers::parser_layer_cases`) |
| H4 | With `Transfer-Encoding` present, hyper drops `Content-Length` from the parsed headers and keeps `Transfer-Encoding`, so a middleware still sees it. | hyper `src/proto/h1/role.rs:275-293` | T (`http_layers::framing_and_size`) |
| H5 | When a response is sent before the request body was read, hyper drains only bytes already buffered and otherwise closes the read side; it does not drain the socket. On Windows a close with unread data can reset the connection (spike F-3c). | hyper `src/proto/h1/conn.rs:875-891`, `src/proto/h1/dispatch.rs:242-243`; spike `docs/spikes/pty-hooks-spike.md` §3 (B-8 Windows) | D here; PR 9 records the observed outcome per OS |
| H6 | `tower-http`'s `RequestBodyLimit` answers 413 for a declared length above the limit without reading the body, does nothing for a missing length, and wraps the body in `Limited`; `TimeoutLayer` answers 408 and drops the inner future. | `tower-http` `src/limit/service.rs:48-63`; `src/timeout/service.rs:30-35` | D |
| H7 | `tower-http`'s default `MakeSpan` records the full request URI, query included. | `tower-http` `src/trace/make_span.rs:78-90` | D (the reason §2.10 uses its own span) |
| H8 | axum's `WebSocketUpgrade` checks method, `Connection`, `Upgrade`, `Sec-WebSocket-Key`, and version, not `Origin`; tungstenite answers pings itself; `max_message_size`, `max_frame_size`, and `max_write_buffer_size` are per-socket settings. | axum `src/extract/ws.rs:189-201`, `:449-470`, `:781-790`, `:1220-1221` | D |
| H9 | `getrandom::fill` fills a buffer from the OS CSPRNG; `uuid` v4 (the store's epoch and event ids) uses the same `getrandom` 0.4.3. | `getrandom` `src/lib.rs:87`; this repo's `Cargo.lock` (`uuid` → `getrandom 0.4.3`) | D |

## 1. Goal and Dependencies

**Goal.** An `http` module in `chaldea-daemon` that serves mvp §2.5's route table on a listener PR 8 hands it, decides every request in the order Host → Origin → credential → token kind → framing → size before any body byte, answers reads from one store snapshot each, issues and checks tokens, tickets, and pair codes, notifies WebSocket clients of stream heads, and runs commands through a receipt and a dispatch seam that later PRs fill. Every claim has a test on `macos-latest` and `windows-latest`.

**Files.**

```
crates/core/src/auth.rs              # Sans-IO policy: TokenKind, Action, Grant, the action table, object-scope functions
crates/core/src/http.rs              # public wire bodies: CommandRequest, ErrorBody, WsTicket*, Pair*, ThreadsResponse, WorktreesResponse (§8 OQ2)
crates/daemon/clippy.toml            # disallows axum Router::{route, route_service, nest, nest_service, merge, fallback, fallback_service} (§2.2)
crates/daemon/src/http/mod.rs        # HttpServer, HttpConfig, ServeHandle, HttpShutdownReport
crates/daemon/src/http/listener.rs   # accept loop, hyper http1 builder, connection tasks, ConnStart
crates/daemon/src/http/routes.rs     # RouteSpec, ROUTES, Mount, build_router (the only Router::route call)
crates/daemon/src/http/guard.rs      # HostLayer (outermost), GuardLayer (per route: Origin → credential → kind → framing → size → deadline)
crates/daemon/src/http/auth.rs       # Secret, TokenService (mint, hash, verify_in), Principal, ObjectScope evaluation over ReadTx
crates/daemon/src/http/tickets.rs    # TicketStore, PairCodes (memory, single use)
crates/daemon/src/http/ctx.rs        # ReadCtx, WriteCtx: read_in_snapshot, submit_and_wait, body_within
crates/daemon/src/http/error.rs      # ApiError, codes, the store-outcome mapping (§2.9)
crates/daemon/src/http/sync_api.rs   # GET /api/events, /api/threads, /api/worktrees
crates/daemon/src/http/blobs.rs      # GET /api/blobs/<sha256>
crates/daemon/src/http/ws.rs         # POST /api/ws-ticket, GET /api/ws socket loop
crates/daemon/src/http/pair.rs       # POST /api/pair, POST /api/pair-codes (§8 OQ1)
crates/daemon/src/http/commands.rs   # envelope, request hash, CommandRegistry, CommandHandler, dispatch
crates/daemon/src/http/trace.rs      # request span (route template only), Secret redaction
crates/daemon/tests/http_*.rs        # integration tests (§7)
crates/daemon/tests/support/http.rs  # test server on 127.0.0.1:0, raw HTTP/1.1 client, WS client, capturing log subscriber
```

**Dependencies.** Root `[workspace.dependencies]`, exact pins with one comment line each, in the existing style:

```toml
# HTTP routing and extractors (mvp §2.5, arch §3); no defaults: no form, no axum tracing, no http2.
axum = { version = "=0.8.9", default-features = false, features = ["http1", "tokio", "ws", "query", "json", "matched-path"] }
# The HTTP/1 connection builder with a timer, header limits, and upgrades (H1, H2); request parsing is hyper's.
hyper = { version = "=1.12.0", default-features = false, features = ["server", "http1"] }
# TokioIo, TokioTimer, and the tower-to-hyper service adapter for the own accept loop (H1).
hyper-util = { version = "=0.1.21", default-features = false, features = ["tokio", "server", "http1", "service"] }
# Layer and ServiceBuilder for the guard; util for ServiceExt in tests.
tower = { version = "=0.5.3", default-features = false, features = ["util"] }
# Sensitive-header marking and the body limit wrapper (H6); no TimeoutLayer (§3 "Deadlines").
tower-http = { version = "=0.7.1", default-features = false, features = ["sensitive-headers", "limit"] }
# Final equality of credential hashes (mvp §2.5, prior-art row 5d); same version as the spike.
subtle = "=2.6.1"
# 256-bit secrets for tokens, tickets, and pair codes from the OS CSPRNG (H9; already in the lock through uuid).
getrandom = "=0.4.3"
# Request spans and redaction (§2.10); PR 8 installs the subscriber.
tracing = "=0.1.44"
# Collecting a bounded request body under a deadline.
http-body-util = "=0.1.5"
```

Workspace changes: `tokio` keeps `default-features = false`; `crates/daemon` adds the features `rt`, `net`, `time`, `macros` (`sync` stays). Dev-dependencies of `crates/daemon`: `tokio` with `rt-multi-thread`; `tokio-tungstenite = "=0.29.0"` (the WS test client; the version axum 0.8.9's `ws` feature already locks); `tracing-subscriber = { version = "=0.3.23", default-features = false, features = ["fmt", "std"] }` (the capturing subscriber of the log test). `sha2 =0.11.1` (already a daemon dependency) hashes secrets and command requests. The `ws` feature adds `sha1 0.10.7` (the WebSocket accept key, `digest` 0.10 alongside `sha2`'s `digest` 0.11); both are in the probe lock with no new C code. `axum 0.8.9` needs Rust 1.80, `hyper-util 0.1.21` 1.85; the toolchain is 1.96.0. The PR records the resolved lock versions (axum-core 0.5.6, http 1.5.0, http-body 1.1.0, matchit 0.8.4, tungstenite 0.29.0) in its description.

**Workspace and CI.** No new CI job. `cargo test --workspace --locked` runs the HTTP tests on both runners against `127.0.0.1:0` listeners and stores in the OS temp directory. The `core` dependency check gains `hyper|tower|getrandom` in its forbidden list, since `core::auth` and `core::http` are pure:

```
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum|hyper|tower|getrandom) ' <<<"$deps"
```

No test is Windows-only, so the sentinel list is unchanged. `lefthook.yml` mirrors the changed check.

## 2. Types and Functions

Names may change in review; shapes may not.

### 2.1 The Rules

1. **The platform decides framing; one guard decides access.** Request parsing, head limits, and the slow-head bound are hyper's (H2, H3). Every access decision before the handler runs is one tower layer per route built from that route's `RouteSpec`, and the router is built only from the route table (§2.2), so no route can exist without its spec, and the matrix test iterates the same table.
2. **The database decides; memory only remembers what is safe to forget.** Tokens and receipts live in SQLite and are checked at use time inside the transaction that uses them (§2.3). Tickets and pair codes live in memory, single use under one mutex each; a restart drops them, and losing one only makes its holder ask for a new one (§2.4).
3. **One deciding point per concurrent operation** (§2.12): the writer's serial job order for receipts and revocations, one read snapshot for each read, a mutex `remove` for tickets and pair codes, and one sequential loop per WebSocket.
4. **Every wait has a deadline.** Body reads, `read` calls, ticket waits, WebSocket sends, idle sockets, and shutdown all have one (§5). Handlers reach the store only through `ReadCtx` and `WriteCtx` (§2.3), which apply the request deadline.

### 2.2 Server, Routes, Guard

```rust
pub struct HttpConfig {
    pub port: u16,                          // the bound port; Host must name it
    pub short_deadline: Duration,           // 5 s: every non-streaming route without its own
    pub header_read_timeout: Duration,      // 5 s (H2): slow or idle heads end the connection (EOF)
    pub max_header_bytes: usize,            // 8 KiB (spike B-8); over → 431 (H3)
    pub max_body_bytes: u64,                // 4 MiB (mvp §2.5)
    pub max_connections: usize,             // 128; the accept loop waits for a permit
    pub ws: WsConfig,                       // §2.6
    pub ticket_ttl: Duration,               // 30 s
    pub pair_code_ttl: Duration,            // 60 s
    pub clock: Arc<dyn MonotonicClock>,     // tickets, pair codes, deadlines; tests inject
}
pub struct HttpServer { /* Arc<AppState>: store, config, TokenService, TicketStore, PairCodes, CommandRegistry, revoke nudge */ }
impl HttpServer {
    pub fn new(cfg: HttpConfig, store: Store, registry: CommandRegistry, ext: Extensions) -> HttpServer;
    /// Serves until `shutdown` is called. The listener is PR 8's (bound to 127.0.0.1).
    pub fn serve(&self, listener: tokio::net::TcpListener) -> ServeHandle;
    pub fn tokens(&self) -> &TokenService;        // PR 8 (owner token file), PR 10 (launch tokens), PR 20
    pub fn nudge_revocation(&self);               // re-check every WebSocket's token now (§2.6)
}
pub struct ServeHandle { /* accept task, JoinSet of connections, shutdown watch */ }
impl ServeHandle {
    /// Stops accepting, asks connections to finish, closes sockets, waits until `deadline`, aborts the rest.
    pub async fn shutdown(self, deadline: Instant) -> HttpShutdownReport;
}
pub struct HttpShutdownReport { pub connections_finished: usize, pub connections_aborted: usize, pub sockets_closed: usize }
```

**Accept loop** (`listener.rs`). Its own loop instead of `axum::serve`, because `axum::serve` sets no timer and so no header read timeout and no connection bound (H1). For each accepted stream: take a permit of a `Semaphore(max_connections)` (the loop waits for one, so excess clients wait in the kernel backlog), set `TCP_NODELAY`, insert `ConnStart(Instant)` into each request's extensions (PR 9 can measure its budget from connection start), and serve with `hyper::server::conn::http1::Builder` configured with `timer(TokioTimer)`, `header_read_timeout`, `max_header_size`, `max_headers(64)`, `keep_alive(true)`, then `serve_connection(TokioIo, TowerToHyperService(app)).with_upgrades()` (H2). Connection tasks live in a `JoinSet`; each selects on its connection and a `watch` shutdown signal, calling `graceful_shutdown` on the signal. HTTP/2 is not enabled: the PWA and the CLI use HTTP/1.1 on loopback, and one protocol is one set of framing rules.

**Route table** (`routes.rs`). One static table, the single source for the router, the guard, and the matrix test:

```rust
pub struct RouteSpec {
    pub method: Method, pub path: &'static str,        // axum path syntax, e.g. "/api/blobs/{sha256}"
    pub origin: OriginRule,                            // AbsentOrMatching | PresentMatching | MustBeAbsent | None (static PWA)
    pub credential: Credential,                        // None | Bearer(&'static [TokenKind]) | Ticket(TicketPurposeKind) | PairCode
    pub action: Option<Action>,                        // the action-table row checked before the body (core::auth)
    pub body: BodyRule,                                // NoBody | Json { required: true } | Raw (/hook)
    pub effects: Effects,                              // ReadOnly | Writes | Upgrade
    pub deadline: DeadlineRule,                        // Short | PerHandler (commands) | Upgrade (WS handshake, then the socket's own)
    pub owner_pr: u8,                                  // the PR that implements the handler; until then 501
}
pub static ROUTES: &[RouteSpec] = &[ /* the table below */ ];
pub struct Mount { pub spec: &'static RouteSpec, pub handler: MethodRouter<AppState> }
pub(crate) fn build_router(mounts: &[Mount], state: AppState) -> Router;  // the only Router::route call
```

| Method, path | Origin | Credential (kinds) | Action checked before body | Effects | Handler PR |
|---|---|---|---|---|---|
| `GET /`, `GET /assets/{*path}` | none | none | — | ReadOnly | 18 |
| `POST /api/pair` | PresentMatching | PairCode | — | Writes | 7 |
| `POST /api/pair-codes` (§8 OQ1) | MustBeAbsent | Bearer(owner) | — | ReadOnly (memory) | 7 |
| `POST /api/ws-ticket` | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadStream` (purpose `attach:` also `Attach`, checked on the body) | ReadOnly (memory) | 7 |
| `GET /api/ws` | AbsentOrMatching | Ticket(events) | — | Upgrade | 7 |
| `GET /api/attach/{thread}` | AbsentOrMatching | Ticket(attach) | `Attach` | Upgrade | 11 |
| `GET /api/events` | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadStream` | ReadOnly | 7 |
| `GET /api/blobs/{sha256}` | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadBlob` | ReadOnly | 7 |
| `GET /api/threads` | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadThreadList` | ReadOnly | 7 |
| `GET /api/worktrees` | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadWorktrees` | ReadOnly | 7 |
| `GET /api/adapters` | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadAdapters` | ReadOnly | 10 |
| `GET /api/doctor` | AbsentOrMatching | Bearer(owner, rescue) | `ReadDoctor` | ReadOnly | 21 |
| `POST /api/commands/{name}` | AbsentOrMatching | Bearer(owner, agent) | the command's action (§2.8) | Writes | 7 seam; each command its PR |
| `POST /hook` | MustBeAbsent | Bearer(hook) | — | Writes | 9 |
| `POST /events` | MustBeAbsent | Bearer(owner, rescue, extension) | `PostEvents` | Writes | 20 |

The credential kinds of a route are the union of the kinds that hold any grant for its action in the action table (§2.5); a test derives them from `core::auth` and compares (`http_matrix::route_kinds_follow_the_action_table`). A route whose handler PR has not merged is still mounted with its full spec and answers 501 `not_implemented` after the guard, so its access rules are tested from PR 7 on.

**Only the table builds the router.** `crates/daemon/clippy.toml` lists axum's `Router::route`, `route_service`, `nest`, `nest_service`, `merge`, `fallback`, and `fallback_service` under `disallowed-methods`; `build_router` carries the one `#[allow(clippy::disallowed_methods)]`, and `clippy -D warnings` fails on any other call. `build_router` mounts each `Mount` as `router.route(spec.path, handler.layer(GuardLayer::new(spec)))` and refuses (panics at startup, tested) a `Mount` whose spec is not in `ROUTES` or a `ROUTES` entry with no mount. The fallback (unknown path) answers 404 `not_found` after the Host check; a known path with another method answers 405.

**Guard order** (mvp §2.5 "Order", "Rejections"):

| Step | Where | Check | Failure |
|---|---|---|---|
| 0. Parse | hyper (H2, H3) | head within `header_read_timeout`, `max_header_size`, `max_headers`; valid framing headers | EOF for a slow head; 431; 400 |
| 1. Host | `HostLayer`, outermost, before routing | exactly one `Host`, equal to `127.0.0.1:<port>` or `localhost:<port>` (`localhost` ASCII case-insensitive); a request target with an authority must name the same host | 421 `misdirected_request` |
| — | router | path and method | 404 / 405 |
| 2. Origin | `GuardLayer` | per `OriginRule`: at most one `Origin`; `AbsentOrMatching`: absent or exactly `http://` + the received `Host` value; `PresentMatching`: present and matching; `MustBeAbsent`: absent. `Origin: null` is present and never matches | 403 `origin_rejected` |
| 3. Credential | `GuardLayer` | Bearer: exactly one `Authorization`, scheme `Bearer` (case-insensitive), a token in the secret format (§2.4); verified in a read (§2.3). Ticket: the `ticket` query parameter redeemed (§2.4). PairCode: checked in the handler from the body, since it is the body (§2.4) | 401 `unauthorized` with `WWW-Authenticate: Bearer` |
| 4. Kind | `GuardLayer` | the verified kind is in the route's kinds, and the route's action has a grant for it | 403 `forbidden` |
| 5. Framing | `GuardLayer` | `POST`: one `Content-Length` present (else 411 `length_required`); any `Transfer-Encoding` → 400 `bad_request` (H4); `GET` with a nonzero `Content-Length` → 400 | as listed |
| 6. Size | `GuardLayer` | `Content-Length` ≤ `max_body_bytes` (else 413 `payload_too_large`, H6's rule); the body is wrapped in `http_body_util::Limited` as the second line | 413 |
| 7. Deadline | `GuardLayer` | inserts `Deadline(now + budget)`; for `Effects::ReadOnly` routes it also wraps the handler in `timeout_at(deadline)` → 408 `request_timeout` | 408 |

Steps 1–6 read only the head, so no body byte is consumed before them; the credential lookup in step 3 is a store read that runs before the body is polled. Every rejection is a response with no store write, no event, no ticket or code consumed (except a ticket or pair code redeemed in step 3, which is spent by design), and no handler call. Writes routes are not wrapped in a timeout that drops the handler (H6): dropping a handler after `submit` would answer 408 while the job may still commit, which mvp §2.5 forbids; they bound their waits through `WriteCtx` (§2.3, §2.8).

**Rejected-request bodies.** Every error response is `application/json` `ErrorBody { code, retry_after? }` with `Cache-Control: no-store` (§2.9). PR 7 adds `Connection: close` to every rejection decided before the body is read, so hyper closes after the response; whether a client on Windows reads that response when it still had body bytes in flight (H5) is a B-8 case PR 9 records.

### 2.3 Store Access: `ReadCtx`, `WriteCtx`

Handlers do not hold `Store`; `AppState` keeps it private to `ctx.rs`, and handlers get one of these extractors:

```rust
pub struct ReadCtx { principal: Principal, deadline: Deadline, store: Store }
impl ReadCtx {
    /// One `Store::read` in `spawn_blocking`, waiting at most until the deadline. Inside, the token is
    /// re-verified first (`verify_in`) in the same snapshot as everything `f` reads.
    pub async fn read<R: Send + 'static>(&self, f: impl FnOnce(&ReadTx<'_>, &Authorized) -> Result<R, ApiError> + Send + 'static)
        -> Result<R, ApiError>;
}
pub struct WriteCtx { principal: Principal, deadline: Deadline, store: Store }
impl WriteCtx {
    /// Submits one job with `not_after = deadline`; inside the job the token is re-verified first in the
    /// write transaction. Waits for the ticket until `deadline + TICKET_GRACE` (2 s), see §2.8.
    pub async fn submit_and_wait<R: Send + 'static>(&self, job: impl FnOnce(&mut WriteTx<'_>, &Authorized) -> Result<R, JobError> + Send + 'static)
        -> Result<R, ApiError>;
    pub async fn body_within(&self, body: Body, max: u64) -> Result<Bytes, ApiError>;   // 408 at the deadline, 413 over max
}
pub struct Authorized { pub principal: Principal, /* the TokenRow as read in this transaction */ }
```

The two routes without a Bearer principal reach the store through `TokenService`, which uses the same deadline and mapping: `POST /api/pair` calls `issue_owner` (one job with `not_after = deadline`), and ticket redemption calls `verify_in` by token id in one read. `Authorized` is constructed only by `verify_in` inside the transaction, so a closure that has one has a token valid in its own snapshot. The step-3 lookup and the in-transaction check run the same `verify_in` (§2.4): the first rejects before the body, the second decides. `spawn_blocking` closures capture `Store` (moved in), the principal, and parsed parameters; nothing else. A `spawn_blocking` task that outlives its deadline keeps running until `f` returns; reads have no effect, so the 408 is accurate (§5).

### 2.4 Tokens, Tickets, Pair Codes

**Secret format.** 32 bytes from `getrandom::fill`, base64url without padding (43 characters), with a fixed prefix per credential: `chd_` (token), `chdt_` (ticket), `chdp_` (pair code). The prefix lets the log redaction (§2.10) and secret scanners match them; the kind is not encoded (it lives in the row). `Secret(String)` has no `Display`, and its `Debug` prints `Secret(<redacted>)`.

**Hashing and comparison.** The stored and looked-up value is `SHA-256(secret bytes)`. Lookups go by hash (`tokens.sha256` is `UNIQUE`; the ticket and pair-code maps are keyed by hash), so timing reveals at most the position of a SHA-256 output in an index, which gives nothing about any secret; the final equality between the computed hash and the found row's hash uses `subtle::ConstantTimeEq` (mvp §2.5, prior-art row 5d). T3 Code compares secrets with `timingSafeEqual` (`apps/server/src/auth/utils.ts:83-89`).

```rust
pub struct MintedToken { pub token_id: TokenId, pub secret: Secret, pub sha256: [u8; 32] }
impl TokenService {
    pub fn mint(&self) -> MintedToken;                                   // random id (v4) and secret; touches nothing
    pub fn new_token(m: &MintedToken, kind: TokenKind, thread: Option<ThreadId>, launch: Option<LaunchId>) -> NewToken;
    pub fn verify_in(tx: &ReadTx<'_>, secret: &str, now: &Timestamp) -> Result<Authorized, Unauthorized>;
    pub async fn issue_owner(&self, deadline: Instant) -> Result<Secret, ApiError>;    // one job: token_insert
    pub async fn revoke(&self, which: TokenSelector, deadline: Instant) -> Result<u64, ApiError>;  // then nudge_revocation
}
```

Issuance inside another PR's job (PR 10's reservation inserts the hook and agent token hashes in the job that appends `launch_reserved`, mvp §2.9) is `mint()` before the job, `tx.token_insert(new_token(..))` inside it, and the secret handed to the launch only after the ticket resolves `Ok`.

**`verify_in`** (the one token check; step 3 and every `ReadCtx`/`WriteCtx` closure run it): parse the format (else `Unauthorized`), hash, `token_by_hash`, constant-time equality, `revoked_at IS NULL`; for `agent`, also: the token's launch is its thread's `current_launch_id` and that launch has no `exit_reason` (mvp §2.5 "one launch" holds at use time even before PR 10 revokes); for `hook`, only the revocation check (PR 9 adds its launch and binding checks). `rescue` and `owner`: until revoked. `extension`: until revoked; its one-run lifetime is PR 20's revocation after the run.

| Credential | Lives in | Restart | Expiry enforced at | Single use decided by |
|---|---|---|---|---|
| Token (`owner`, `agent`, `rescue`, `hook`, `extension`) | SQLite `tokens` (hash only) | survives; mvp §2.9 restart revokes every open launch's tokens (PR 10) | `verify_in` in the transaction that uses it | — (multi-use) |
| WS ticket | memory `TicketStore` (hash → `{ token_id, purpose, expires_at }`) | lost; clients mint a new one (they reconnect after a restart anyway, mvp §2.4) | redemption: `now < expires_at`, then `verify_in` on the bound token | `Mutex::lock` + `HashMap::remove` |
| Pair code | memory `PairCodes` (hash → `expires_at`) | lost; the user runs `chaldea open` again | redemption: `now < expires_at` | `Mutex::lock` + `HashMap::remove` |
| Command receipt | SQLite `command_receipts` | `in_progress` → `unknown_after_crash` (PR 8/10 call `receipts_mark_unknown_after_crash`) | — | the writer's job order (§2.8) |

**Tickets** (`POST /api/ws-ticket`). Body `WsTicketRequest { purpose: "events" | "attach:<thread_id>" }`; reply `WsTicketResponse { ticket, expires_in: 30 }`. `attach:` requires the `owner` kind and the `Attach` grant (403 otherwise); the thread must exist (404 otherwise). The store holds at most 1024 tickets and 32 per token; expired entries are purged on every insert; a full store answers 503 `busy`. **Redemption** (`GET /api/ws?ticket=`, `GET /api/attach/{thread}?ticket=`): under the mutex, `remove` the entry by hash; outside it, reject if absent, expired, or of another purpose (an `attach:` ticket must name the path's thread), then `verify_in` the bound token by id (a revoked token kills its tickets: mvp §2.5 "die with their token") and for `attach` re-check `owner`. Every failure is 401 with nothing else changed. Redemption happens in the guard before the `101` response.

Restart safety: a ticket is worth 30 s and one socket; losing it only makes the client mint another with its Bearer token, which it does on every reconnect anyway. T3 Code uses signed stateless WebSocket tickets with a 5-minute expiry and a session-revocation check at use, not single use (`auth/SessionStore.ts:493`, `:1049-1064`, `:1113`, `:1138`; its media tickets are reused on purpose, `auth/EnvironmentAuth.ts:1500-1502`); Chaldea's tickets are single use (mvp §2.5), which a stateless signature cannot enforce without a spent list, so they are memory entries.

**Pair codes.** `POST /api/pair-codes` (owner Bearer, Origin absent, §8 OQ1) mints a code, keeps its hash with `expires_at = now + 60 s` (at most 8 outstanding; purge on insert; full → 503), and returns `PairCodeResponse { code, expires_in: 60 }`; `chaldea open` opens `http://127.0.0.1:<port>/#pair=<code>`. `POST /api/pair` (Origin present and matching) takes `PairRequest { code }`: under the mutex, `remove` by hash; absent or expired → 401 `unauthorized`. The code is spent before the token exists; then one job inserts a new `owner` token (`issue_owner`), and the reply is `PairResponse { token }`. If the job fails or its deadline passes, the code stays spent and the user pairs again (fails closed). A paired token whose response was lost is a valid owner token nobody holds; it is harmless and revocable. T3 Code spends a one-time link in one conditional `UPDATE … WHERE consumed_at IS NULL AND expires_at > now` (`persistence/AuthPairingLinks.ts:239-247`) and in-memory grants through an atomic `Ref.modify` (`auth/PairingGrantStore.ts:546`); `PairCodes` is the latter shape. A pair code is never persisted: it would be a secret at rest for 60 s of value.

### 2.5 Authorization: `core::auth`

```rust
pub enum TokenKind { Owner, Agent, Rescue, Hook, Extension }           // mvp §2.5 public names
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
    OwnedWorktrees,         // worktree remove: owner_thread_id ∈ self+, no flags
    ExtensionScope,         // POST /events: the token's configured scope (PR 20)
    SurfaceScopeContains,   // surface_action: the surface's scope contains the target (PR 20)
}
pub fn grant(kind: TokenKind, action: Action) -> Option<Grant>;            // the table; None = refused
pub struct ThreadGraph { /* thread → parent, from proj_threads */ }
pub fn self_plus(g: &ThreadGraph, me: ThreadId) -> BTreeSet<ThreadId>;
pub fn may_act_on_thread(g: &ThreadGraph, me: Option<ThreadId>, grant: Grant, target: ThreadId) -> bool;
```

The table is mvp §2.5's "Actions" table transcribed cell by cell (`owner`: `All` everywhere, with `SurfaceScopeContains` for `surface_action`; `agent`: `SelfPlus` for reads, send, stop, resume, close, and the rest as named; `rescue`: `All` for reads and doctor read, `PostEvents` any scope; `extension`: `ExtensionScope` for `PostEvents` only; `hook`: nothing on these actions, its only route is `/hook`). `core::auth` is pure (mvp §3 places the auth policy in `core`); `ThreadGraph` is built by the daemon from `ReadTx::threads()` in the same snapshot as the decision.

**Object checks** run on the validated body before any effect, in the same transaction as the effect (mvp §2.5 "Order"): reads in the `ReadCtx` snapshot, commands in the intent job (§2.8). For an update or delete, the command handler also checks the stored object's scope (the surface a `POST /events` replaces must already be in the caller's scope; PR 20 uses `ObjectScope::stored_surface`). **Hiding:** an object the principal cannot read answers 404 `not_found`, identical to a missing one; an object it can read but not act on answers 403 `forbidden`. One function, `deny_for(graph, principal, object) -> ApiError`, decides between them, so no handler chooses its own.

**Readable streams:** `owner`, `rescue`: every stream; `agent`: `thread:<t>` for `t ∈ self_plus`; the `host` stream is not self+ and is 404 for agents, who read the thread list and worktrees through the filtered routes instead (its events cannot be filtered without breaking the gapless `seq` a client pages by). **Per-resource filtering:** `GET /api/threads` returns `ThreadInfo` for readable threads; `GET /api/worktrees` returns all for `owner` and `rescue` and those with `owner_thread_id ∈ self_plus` for `agent`; a blob needs a readable stream and a `blob_refs` row; WebSocket subscriptions keep only readable streams. A receipt is visible only to the token that created it (§2.8).

### 2.6 WebSocket: Tickets, Hub, Socket Loop

```rust
pub struct WsConfig {
    pub heads_interval: Duration,        // 30 s (mvp §2.4); tests 200 ms
    pub send_timeout: Duration,          // 10 s per frame; past it the socket closes (slow consumer)
    pub idle_timeout: Duration,          // 75 s with no inbound frame (the browser answers our pings)
    pub max_message_bytes: usize,        // 64 KiB inbound (max_message_size and max_frame_size, H8)
    pub max_write_buffer_bytes: usize,   // 256 KiB (H8)
    pub max_subscriptions: usize,        // 1024 streams per socket
    pub max_sockets: usize,              // 128 total; 16 per token; over → 503 before the upgrade
    pub close_grace: Duration,           // 1 s to send a close frame at shutdown
}
```

**Socket loop.** One task per socket, created by `on_upgrade`, owning the socket and its own `broadcast::Receiver<Committed>` from `Store::subscribe()`, taken **before** `hello` is sent. Sequential `tokio::select!` over: an inbound frame; a commit notification; the heads tick (`interval`, `MissedTickBehavior::Delay`); the revocation nudge (`watch`); the shutdown signal; and the idle deadline. State: the subscription set and a `pending: BTreeMap<StreamId, u64>` of heads to announce, flushed one `stream_advanced` frame at a time, each send under `send_timeout`. Memory per socket is bounded by `max_subscriptions` entries whatever the commit rate; a burst on one stream coalesces into one frame with its latest head.

| Event | Effect |
|---|---|
| start | `hello { epoch, server_time }` (epoch from `Store::epoch`) |
| `subscribe { stream_ids }` | parse (unknown or malformed ids dropped; more than `max_subscriptions` → close 1008); in one read (`ReadCtx` shape, token re-verified): keep the readable ones, read their heads; **replace** the set, then send `heads` for the kept streams. Unreadable and unknown ids are dropped silently, never named |
| `Committed { heads }` | for each entry in the set, `pending[stream] = max(pending, head)`; flush |
| `Lagged(n)` | the notifications it skipped are unknown: read the heads of the whole set now and send `heads` (store §2.2: a lagging receiver resyncs from heads) |
| heads tick | one read: `verify_in` the bound token, heads of the set; send `heads`, then a `Ping` |
| revocation nudge | one read: `verify_in`; on failure close 1008 `token_revoked` |
| token invalid at any read | close 1008 `token_revoked` |
| inbound text that is not a `WsClientMessage`, binary, or over size | close 1003 / 1009 |
| `Closed` from the receiver, or the shutdown signal | close 1001 within `close_grace`, then drop |
| send timeout, read error, idle deadline | drop the socket (no close handshake wait) |

**Why a socket converges.** Notifications are published after `COMMIT` (store §2.2), the receiver exists before `hello`, and every set change is followed by a heads read: a commit before the read is in the heads; a commit after it reaches the receiver. A lost notification (lag, or a client that missed a frame) is repaired by the next heads tick within `heads_interval` and by the subscribe-time heads on reconnect, which is mvp §2.4's recovery bound and CL-13's premise. The broadcast capacity is the store's (1024); PR 7 does not change it.

**Token revocation and live sockets.** Tickets die with their token at redemption. An open socket re-verifies its token on every read it already makes (subscribe, heads tick, lag resync) and on the revocation nudge, which `TokenService::revoke` and `HttpServer::nudge_revocation` fire after a revoking job commits (PR 10 and PR 16 call it after their revoking jobs). The nudge carries no state: each socket decides by reading the database. A revocation that no nudge announces is applied within one `heads_interval`; in that window the socket can deliver only stream heads it could already read, never event content, which every `GET /api/events` re-checks.

T3 Code's WebSocket is an Effect RPC transport (`apps/server/src/ws.ts`) with server-pushed streams; it is not used as precedent for the notify-then-pull hub, which follows mvp §2.4.

### 2.7 Sync Read API and Blobs

**`GET /api/events`** (mvp §2.4). Query parameters: `stream_id` (required, `StreamId` syntax), `after_seq`, `before_seq` (decimal `u64` up to `MAX_SAFE_INTEGER`), `limit` (decimal `u32`), `epoch` (string). A missing `stream_id`, a malformed or repeated parameter, or an unknown parameter → 400 `bad_query` (before any read). Then one `ReadCtx::read`:

1. `verify_in` (inside `ReadCtx`); the stream must exist (`stream_exists`) and be readable (§2.5), else 404 `not_found` (one body for both).
2. `head(stream, after_seq.unwrap_or(0))`, then `core::sync::decide_replay(&query, head, store.epoch())`: `LimitOutOfRange` and `AfterAndBefore` → 400 `bad_query`.
3. `Forward` → `events_after`, `Backward` → `events_before` → `EventsResponse::Replay { events, head_seq, has_more, has_more_before, epoch }`; `Snapshot { reason }` → `thread_snapshot` or `host_snapshot` (agents never reach the host stream) → `EventsResponse::Snapshot { snapshot, as_of_seq, reason, epoch }`.

Head, page, snapshot, and `as_of_seq` come from that one read transaction (store §2.2, PR6-CL-18), so a commit during the request is either wholly in the reply or wholly after `head_seq`/`as_of_seq`. The byte bound uses `cum_bytes` through `StreamHead` (an O(1) read). The reply always carries the server epoch; a backward page with a mismatched epoch is still a page (`decide_replay`), and the client resyncs forward (mvp §2.4). `before_seq = 1` returns `events: []` with `has_more_before: false`; `before_seq` above the head returns the latest page (the store clamps). The 404 precedes the 400 of step 2 because `decide_replay` takes the stream head (`crates/core/src/sync.rs`, "a 404 decided before this call"). This is T3 Code's `decideThreadResume` (`apps/server/src/orchestration-v2/ThreadStream.ts:100-118`, bounds at `:18`, `:20`) with mvp's bounds and epoch.

**`GET /api/threads`** → `ThreadsResponse { epoch, threads: Vec<ThreadInfo> }` filtered (§2.5). **`GET /api/worktrees`** → `WorktreesResponse { epoch, worktrees: Vec<WorktreeChanged> }` filtered. **`GET /api/adapters`** and **`GET /api/doctor`**: route, guard, and 501 until PR 10 and PR 21.

**`GET /api/blobs/{sha256}?stream_id=`.** `sha256` must be 64 lowercase hex and `stream_id` present and valid, else 400 `bad_query`. One read: readable stream (else 404), `ReadTx::blob(stream, sha256)` (`None` → 404, the same body). Response: status 200, `Content-Type` = the store's download type if it is exactly `text/plain; charset=utf-8` or `application/json`, else `application/octet-stream` (mvp §2.5), `Content-Length`, `X-Content-Type-Options: nosniff`, `Content-Disposition: attachment`, `Cache-Control: no-store`; no `Range`, no `ETag`. The body is the stored bytes (at most 64 MiB, store §2.5) read whole in the snapshot; at most 4 blob responses are in flight (`Semaphore`; the permit moves into the response body and is released when hyper drops it), else 503 `busy`, so blob memory stays below 256 MiB. A blob response body has a 60 s whole-transfer deadline: a client that stops reading has its connection closed. A streaming reader (`rusqlite` `blob` feature) stays a later, schema-free change.

### 2.8 Commands and Receipts

**Envelope** (public, §8 OQ2): `POST /api/commands/{name}` with `CommandRequest { command_id: CommandId, args: <JSON object> }`; `name` is snake_case: `spawn`, `resume`, `send`, `ready`, `key`, `stop`, `close`, `input_resolve`, `input_cancel`, `task_set`, `mailbox_send`, `worktree_remove`, `doctor_run`, `surface_action` (mvp §2.4's list). Unknown name → 404; a known name whose PR has not merged → 501 `not_implemented`, decided before the body is read, with no receipt, so a retry after that PR merges runs normally.

**Request hash.** `SHA-256("chaldea-command-v1" ‖ 0x00 ‖ name ‖ 0x00 ‖ canonical(args))`, where `canonical` writes the parsed JSON value with object keys sorted by their UTF-8 bytes at every level, no whitespace, strings and numbers as `serde_json` writes them. It is computed explicitly rather than through `serde_json::Map` order, which a `preserve_order` feature anywhere in the build graph would change. `command_id` is the receipt key, not part of the hash.

**Dispatch seam.**

```rust
pub trait CommandHandler: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn action(&self) -> Action;                             // checked against the kind before the body (guard step 4)
    fn deadline(&self) -> Duration;                         // 5 s default; long commands state theirs (mvp §2.5)
    /// Parses and validates `args` (400 `bad_args` on failure); no store access.
    fn parse(&self, args: &serde_json::Value) -> Result<Box<dyn Any + Send>, ApiError>;
    /// Inside the intent job: object authorization against the stored state, then the intent
    /// (events, effects records), optionally the completed result in the same job (`send`).
    fn intent(&self, tx: &mut WriteTx<'_>, auth: &Authorized, cmd: &mut CommandCtx, args: &(dyn Any + Send))
        -> Result<Intent, JobError>;
    /// After the intent committed, for synchronous side-effect commands: runs the effect, bounded by
    /// `deadline()`, and returns the result `complete` stores. Default: none (the intent completed it).
    fn effect(&self, run: EffectRun) -> Option<BoxFuture<'static, CommandResult>> { None }
}
pub enum Intent { Completed(CommandResult), Pending(EffectRun) }
pub struct CommandRegistry { /* name → Box<dyn CommandHandler> or Unimplemented { owner_pr } */ }
impl CommandRegistry {
    pub fn mvp() -> CommandRegistry;                        // every mvp §2.4 command, all Unimplemented in PR 7
    pub fn register(&mut self, h: Box<dyn CommandHandler>); // PR 10, 16, 20, 21 replace their entries
}
```

**Flow of one command request.**

1. Guard: Host, Origin, Bearer, kind (the command's `action` has a grant for this kind), framing, size; `Deadline = now + handler.deadline()`.
2. `body_within(deadline)` → `CommandRequest` (400 `bad_request` for invalid JSON or envelope); `parse(args)`; request hash.
3. `submit_and_wait` with `not_after = deadline`; **the intent job** (one `BEGIN IMMEDIATE` transaction, the single deciding point):
   1. `verify_in` (revoked → `Refused(Unauthorized)`, nothing written);
   2. `tx.read().receipt(command_id)` → `check_receipt(existing, token, hash)`; anything but `New` → return that lookup, nothing written;
   3. `handler.intent(…)`: object authorization first (`deny_for` → `Refused`, nothing written), then `receipt_begin` (inserts `in_progress`) and the intent's writes; `Intent::Completed(r)` also calls `receipt_complete(r)` in this job.
4. Reply by the job's outcome:

| Job outcome | Reply | Receipt afterwards |
|---|---|---|
| `Ok(Lookup::InProgress)` | 409 `in_progress`, `retry_after` = 1 s | unchanged |
| `Ok(Lookup::Replay(r))` | `r.http_status` and `r.body` byte for byte | unchanged |
| `Ok(Lookup::Conflict)` | 409 `conflict` (the same body whatever the receipt's state) | unchanged |
| `Ok(Lookup::UnknownAfterCrash)` | 409 `unknown_after_crash` | unchanged |
| `Ok(Completed(r))` | `r` | `completed` |
| `Ok(Pending(run))` | run `effect` until the deadline; if it returns in time, a second job `receipt_complete(r)`, reply `r`; else 409 `in_progress` with `retry_after`, and the effect finishes and completes the receipt on its own | `in_progress` → `completed` |
| `Err(Expired)` | 408 `request_timeout`: the deadline passed before the job held the lock (store §2.2 step 3) | none |
| `Err(Rejected(Refused(e)))` | `e`'s status (401, 403, 404, 409 `card_closed`, …) | none |
| `Err(Busy)`, `SubmitError::QueueFull` | 503 `busy`, `retry_after` | none |
| `Err(RolledBack(DiskFull))` | 503 `disk_full` | none |
| `Err(RolledBack(_))`, `Rejected(Store(_))`, `Panicked` | 500 `internal` | none |
| `SubmitError::Closed` | 503 `shutting_down` | none |
| `SubmitError::Failed`, `Err(Failed(_))` | 503 `store_failed` | none (writer failed; PR 8 stops) |
| `Err(CommitUnknown(_))`, `Err(WriterGone)` | 503 `outcome_unknown` | absent, `completed`, or `in_progress` → `unknown_after_crash` after restart; the client retries with the same `command_id` |
| ticket not resolved by `deadline + TICKET_GRACE` | 409 `in_progress`, `retry_after` | the writer still replies to the job: it ran (receipt `in_progress` or `completed`) or expired (no receipt); either way a retry with the same `command_id` is correct |

**Why 408 is exact.** The writer runs a job only if its deadline had not passed once it held the lock (store §2.2 step 3), and `Expired` is its only "deadline prevented the commit" reply; no layer drops the handler (§2.2), so PR 7 never answers 408 for a job that may commit. The last row is the only wait PR 7 bounds beyond the writer: the writer replies to every accepted job, but a queue behind a long job could delay that reply past the deadline, and the client then gets the recovery answer mvp §2.5 names (retry with the same `command_id`), not a 408.

**Concurrent duplicates.** Two requests with one `command_id` reach the writer as two jobs; the second runs after the first committed and reads its receipt (`InProgress`, `Replay`, or `Conflict`), so only one intent is ever written; the receipts primary key is the backstop for a second process (store §2.3). A synchronous command's retry while its effect runs gets 409 `in_progress`; after completion, `Replay`.

**Receipt visibility.** `check_receipt` returns `Conflict` for another token before looking at status (`crates/core/src/command.rs`), and `Conflict` has one body, so another token learns nothing about the command. T3 Code's receipts (`persistence/Migrations/055_OrchestrationV2.ts:42-50`) key by `command_id` with no token or request hash; Chaldea adds both (mvp §2.4).

**What PR 7 completes end to end.** No mvp command: each needs its owning PR's state machine (PR 10: `spawn`, `resume`, `send`, `ready`, `key`, `stop`, `close`, `input_resolve`, `input_cancel`; PR 16: `task_set`, `mailbox_send`, `worktree_remove`; PR 20: `surface_action`; PR 21: `doctor_run`). PR 7's tests register test-only handlers (a counter intent, a completed-in-intent handler, a slow-effect handler, a refusing handler) through `CommandRegistry::register` to prove the seam.

### 2.9 Status Mapping and Error Codes

`ErrorBody { code: String, retry_after: Option<u32> }` (seconds), `application/json`, `Cache-Control: no-store`. Codes: `misdirected_request` (421), `origin_rejected` (403), `unauthorized` (401), `forbidden` (403), `not_found` (404), `method_not_allowed` (405), `request_timeout` (408), `length_required` (411), `payload_too_large` (413), `bad_request`, `bad_query`, `bad_args` (400), `in_progress`, `conflict`, `unknown_after_crash` (409), `not_implemented` (501), `busy`, `disk_full`, `shutting_down`, `store_failed`, `outcome_unknown` (503), `internal` (500). Command handlers may add refusal codes of their own PRs (e.g. `card_closed`, mvp §2.10).

Reads: `ReadError::Busy` (no free reader before the deadline) and `ReadError::Store(Busy)` → 503 `busy`; `ReadError::Closed` → 503 `shutting_down`; `ReadError::Store(_)` → 500 `internal`; a `spawn_blocking` read still running at the deadline → 408 (no effect). Writes: §2.8's table, used also by `POST /api/pair`.

### 2.10 Secrets in Logs

- **Spans:** one span per request from `trace.rs`: method, the route template (`MatchedPath`, e.g. `/api/blobs/{sha256}`; `unmatched` for the fallback), status, latency, and `token_id` once known. Never the URI, the query, header values, or bodies (H7 is why `TraceLayer`'s default span is not used).
- **Headers:** `SetSensitiveRequestHeadersLayer` marks `Authorization`, `Cookie`, and `Sec-WebSocket-Key`, so a `Debug` of the headers prints `Sensitive`.
- **Types:** `Secret`, `MintedToken`, `PairRequest`, `PairResponse`, `PairCodeResponse`, `WsTicketResponse` implement `Debug` with the secret replaced; no error type carries a secret or a body.
- **Bodies:** no layer or handler logs a request or response body; `/hook` and `/api/pair` bodies are never in a log (mvp §2.5).
- **Second line:** a `tracing` field formatter used by PR 8's subscriber replaces any `chd_`, `chdt_`, or `chdp_` secret pattern with `<redacted>` (`trace::redact_secrets`), for text that reaches a log by another path (a panic message, a third-party log line).

### 2.11 What Later PRs Plug In

| Seam | Type | Used by |
|---|---|---|
| Listener and lifecycle | `HttpServer::new`, `serve(TcpListener)`, `ServeHandle::shutdown(deadline)` | PR 8 |
| Token issuance and revocation | `TokenService::{mint, new_token, issue_owner, revoke}`, `nudge_revocation` | PR 8 (owner token file), PR 10 (launch tokens), PR 16, PR 20 |
| Commands | `CommandHandler`, `CommandRegistry::register`, `WriteCtx`, `Authorized`, `deny_for` | PR 10, 16, 20, 21 |
| Hook route | `Mount` for `POST /hook` with `Credential::Bearer(hook)`, `Origin: MustBeAbsent`, `ConnStart`, `WriteCtx` | PR 9 |
| Attach | `Mount` for `GET /api/attach/{thread}`; the guard hands the handler `AttachGrant { thread, token_id }` and the `WebSocketUpgrade` | PR 11 |
| Read routes | `Mount` for `/api/adapters`, `/api/doctor`, `/`, `/assets/*` with `ReadCtx` | PR 10, 21, 18 |
| `POST /events` | `Mount` with `Credential::Bearer(owner, rescue, extension)`, `ObjectScope::stored_surface` | PR 20 |

### 2.12 Single Deciding Points

| Operation several requests can hit at once | Decided by | Why a second caller cannot act |
|---|---|---|
| Duplicate `command_id` | the intent job: receipt lookup, authorization, `receipt_begin`, and intent in one `BEGIN IMMEDIATE` transaction on the one writer | jobs run one at a time; the second reads the first's receipt; the primary key refuses a second process's insert (store §2.3) |
| Token revocation racing a request | reads: the request's one snapshot runs `verify_in` and the data reads; writes: the job runs `verify_in` first | a revocation commits before or after the snapshot/job in the writer's order; there is no cached token state to disagree |
| Ticket redemption | `TicketStore`'s mutex: `remove` by hash | only the caller whose `remove` returned the entry has it |
| Pair-code exchange | `PairCodes`' mutex: `remove` by hash, before the token is created | as above; the token insert happens only for the remover |
| Ticket and pair-code caps | the same mutexes | counted and inserted under one lock |
| WS subscribe racing a commit notification | the socket's sequential loop; receiver taken before `hello`; heads read after each set change | a commit is either in the heads read or delivered by the receiver afterwards (§2.6) |
| Revocation racing an open socket | each socket's own read (`verify_in`) on nudge, tick, subscribe, or resync | the nudge carries no decision; the database does |
| Synchronous effect racing a retry | the receipt row (`in_progress` until the completing job) | a retry reads it in its own intent job |
| Socket and connection caps | `Semaphore`s acquired before the upgrade / before serving | permits are atomic |
| Shutdown racing a new connection | the accept loop's `select!` on the shutdown signal, then the `JoinSet` | after the signal no stream is accepted; every connection task is in the set |

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **Serving loop** | A. `axum::serve` | axum `src/serve/mod.rs:388-396` | Rejected: no timer, so no header read timeout (H1); no connection bound; graceful shutdown waits for every WebSocket indefinitely. |
| | B. Own accept loop on hyper's HTTP/1 builder with `TokioTimer` and `with_upgrades` | hyper `server/conn/http1.rs` (H2) | **Adopted.** Slow and oversized heads are hyper's (mvp §2.5 parser layer), connections are bounded, shutdown has a deadline. |
| **Protocol** | A. HTTP/1.1 and HTTP/2 (auto) | axum's default build | Rejected: a second framing to test for B-8; the clients are local HTTP/1.1. |
| | B. HTTP/1.1 only | — | **Adopted.** Adding HTTP/2 later is additive. |
| **Where checks live** | A. Per-handler extractors | — | Rejected: a new route could forget one (lesson: rest on layering). |
| | B. Global `Router::layer`s that look up the route's rules | — | Rejected: rules separated from the route registration they belong to. |
| | C. Host as the outermost layer; one `GuardLayer` per route built from its `RouteSpec`; the router built only from the table, enforced by clippy `disallowed-methods` | — | **Adopted.** |
| **Deadlines** | A. `tower-http` `TimeoutLayer` on every route | `src/timeout/service.rs:30-35` (H6) | Rejected for writes: dropping the handler after `submit` answers 408 while the job may commit, against mvp §2.5. Kept in effect for `ReadOnly` routes, where 408 is accurate. |
| | B. A `Deadline` extension; writes pass it to `submit` as `not_after` and bound their own waits (§2.8) | store §2.2 step 3 | **Adopted** for `Writes` routes. |
| **WS library** | A. axum `ws` (tokio-tungstenite 0.29 underneath) | axum `src/extract/ws.rs` | **Adopted**: upgrade handling, per-socket limits, automatic pongs (H8), one dependency tree. |
| | B. `tokio-tungstenite` directly with hand-written upgrade | — | Rejected: duplicates the handshake for no gain; kept as the test client. |
| **Token check** | A. A memory cache of valid tokens | — | Rejected: cross-call state the database does not hold; revocation would need invalidation. |
| | B. Look up by hash in the transaction that uses it | mvp §2.5 (hashes in SQLite) | **Adopted**; one indexed read per request. |
| **Ticket form** | A. Signed stateless ticket (HMAC over token id, purpose, expiry) | T3 Code `auth/SessionStore.ts:1049-1064`, `:1113` | Rejected: cannot be single use without a spent list, i.e. memory state anyway. |
| | B. Random ticket, hash in a memory map, `remove` on redemption | — | **Adopted**; restart loss is safe (§2.4). |
| | C. Tickets in SQLite | — | Rejected: a write per WebSocket connect for a 30 s credential, and a restart has no use for them. |
| **Pair code issuance** | A. A dedicated owner-only route (`POST /api/pair-codes`, Origin absent) | T3 Code pairing links (`Migrations/031_AuthAuthorizationScopes.ts:11-24`) | **Recommended**, §8 OQ1. |
| | B. A command (`POST /api/commands/open`) | — | Rejected: the receipt stores the reply body, which would persist the code. |
| | C. A ticket purpose `pair` on `POST /api/ws-ticket` | — | Rejected: mixes a browser credential exchange into the socket-ticket route and its 30 s lifetime. |
| **Notification fan-out** | A. One hub task fanning out to per-socket channels | — | Rejected: a second queue per socket that needs its own lag rule. |
| | B. Each socket subscribes to the store's broadcast and coalesces heads per stream | store §2.2 | **Adopted**; lag is the store's `Lagged`, answered with a heads read. |
| **Host stream for agents** | A. Serve it filtered | — | Rejected: filtered events break the gapless `seq` the client pages by. |
| | B. 404; agents use `/api/threads` and `/api/worktrees` | — | **Adopted**; within mvp §2.5's "self+" rule. |
| **Blob body** | A. Whole blob in memory, bounded concurrency | store §2.5 | **Adopted** for the MVP (≤ 64 MiB × 4). |
| | B. Streaming through `rusqlite`'s incremental blob I/O | store §2.5 | Deferred: holds a read snapshot for the whole transfer; schema-free to add. |
| **Command handler shape** | A. One HTTP handler per command | — | Rejected: receipt, deadline, and mapping repeated 14 times. |
| | B. A `CommandHandler` trait with `intent` inside the shared job and an optional `effect` | — | **Adopted.** |
| **Request hash** | A. Bytes as received | — | Rejected: two serializations of one request conflict. |
| | B. `serde_json::to_vec(&Value)` | — | Rejected: key order depends on a crate feature (`preserve_order`) anywhere in the graph. |
| | C. Explicit canonical form (sorted keys) with a domain prefix and the command name | — | **Adopted.** |
| **PR size** | A. One PR | mvp §4 | Rejected: large and on the longest path. |
| | B. Two PRs (server and reads; commands) | — | Rejected (§8 D1). |
| | C. Three PRs: 7a server, guard, tokens, matrices; 7b sync API, blobs, tickets, pairing, WS; 7c commands and receipts | — | **Adopted** (§8 D1). |

## 4. Same-Class Inventory

Every later PR that adds a route, a command, a token use, or a WebSocket purpose, and the seam it uses (§2.11). None needs a change to PR 7's types.

| PR | Adds | Seam | What PR 7 provides |
|---|---|---|---|
| 8 lifecycle | starts and stops the server; writes the owner token file; restart order | `HttpServer::new`, `serve`, `ServeHandle::shutdown`; `TokenService::issue_owner` | Startup order (PR 8): open store → `receipts_mark_unknown_after_crash` and launch recovery (PR 10's) → `serve`. Shutdown order: `ServeHandle::shutdown(budget)` first (no new requests), then `Store::shutdown` with the same budget. Port and lock are PR 8's. |
| 9 hooks | `POST /hook` handler; hook token use | `Mount` (spec already in `ROUTES`), `WriteCtx`, `ConnStart` | Host, `Origin` must be absent, `hook` kind only, 411/400/413/408 before the handler; `verify_in` checks revocation only; hook body never logged. B-8 cases on both OSes are PR 9's (H5). |
| 10 supervisor | `spawn`, `resume`, `send`, `ready`, `key`, `stop`, `close`, `input_resolve`, `input_cancel`; agent and hook tokens; `GET /api/adapters` | `CommandHandler`, `TokenService::{mint, new_token, revoke}`, `nudge_revocation`, `Mount` | Receipt, envelope, deadline, mapping, kind check, object-scope functions, `deny_for` |
| 11 attach | `GET /api/attach/{thread}` socket bytes | `Mount`, `AttachGrant` | ticket purpose `attach:<thread>`, owner-only at issue and redeem, single use |
| 16 agent control | `task_set`, `mailbox_send`, `worktree_remove`; agent token rules | `CommandHandler` | `SelfAndDirectChildren`, `ParentAndChildren`, `OwnedWorktrees`, `CreatedBySelf` grants |
| 18 PWA | serves `/`, `/assets/*`; pairing; ticket and events client | `Mount` | pair flow, `ws-ticket`, `/api/ws`, `/api/events`, the public bodies (§8 OQ2) |
| 20 extensions | `POST /events`, `surface_action`, extension tokens with a scope, rescue | `Mount`, `CommandHandler`, `TokenService` | kind check (owner, rescue, extension), `ExtensionScope`/`SurfaceScopeContains` grants, `ObjectScope::stored_surface`; the scope column is a v2 migration (store §2.4) |
| 21 doctor | `doctor_run`, `GET /api/doctor` | `CommandHandler`, `Mount` | owner and rescue read, owner run |
| 17 `stateright` | the Sync model | — | the receipt table and replay rules (§2.7, §2.8) as model assumptions, cited by claim id |

## 5. Resource Paths

| Resource | Created by | Bound | Ends on |
|---|---|---|---|
| Listener | PR 8 (`serve` takes it) | one | `ServeHandle::shutdown`: the accept task stops at the signal and drops it |
| Connection task | accept loop | `max_connections` (128) permits | client close; `header_read_timeout` on a slow or idle head (EOF to the client); graceful shutdown at the signal; aborted at the shutdown deadline (`JoinSet::abort_all`) |
| Request handler | hyper per request | one per connection at a time (HTTP/1.1) | response; for `ReadOnly` routes the deadline (408); for writes their bounded waits (§2.8); connection abort |
| `spawn_blocking` read | `ReadCtx::read` | the store's reader pool (4) and its `wait_until` | `f` returns (not bounded by the store, store §5 "Waits"); the request answers 408 at its deadline and the task finishes on its own; at daemon shutdown the store abandons and reports it |
| Write ticket wait | `WriteCtx::submit_and_wait` | one per request | the writer's reply, or `deadline + TICKET_GRACE` (§2.8) |
| Synchronous effect future | `CommandHandler::effect` | per command | its own completion; the request stops waiting at the deadline; the effect's owner PR bounds it |
| WebSocket socket task | `on_upgrade` | `max_sockets` (128), 16 per token | client close; send timeout (10 s); idle (75 s); token invalid (1008); `Closed` from the store receiver or the shutdown signal (1001 within `close_grace`, then drop) |
| Commit notification receiver | each socket at start | the store's broadcast (1024) | dropped with its socket; `Lagged` → heads resync; `Closed` → socket close |
| Heads tick | each socket | one `interval` | with its socket |
| Tickets | `POST /api/ws-ticket` | 1024 total, 32 per token, 30 s | redemption (`remove`), expiry purge on insert, daemon exit |
| Pair codes | `POST /api/pair-codes` | 8, 60 s | redemption, purge on insert, daemon exit |
| Blob response body | `GET /api/blobs` | 4 in flight (permit held by the body), 64 MiB each | sent; 60 s transfer deadline; connection close |

**Endings.**

| Ending | What happens |
|---|---|
| Daemon shutdown (PR 8) | `ServeHandle::shutdown(deadline)`: signal the accept loop (stop accepting), `graceful_shutdown` on every connection, sockets send 1001 and drop within `close_grace`; wait for the `JoinSet` until `deadline`; abort the rest and report counts. Aborted handlers that had submitted a job leave it to the writer (it runs or expires); their clients recover through the receipt. PR 8 then shuts the store down with the same budget. |
| Store `Closed` (shutdown began) | `submit` → 503 `shutting_down`; `read` → 503 `shutting_down`; sockets get `Closed` from their receivers and close 1001. |
| Store failed or `WriterGone` | writes → 503 `store_failed` or `outcome_unknown`; reads continue; PR 8 stops the daemon (store §5). |
| Lagged notifications | heads read for the socket's whole set (§2.6). |
| Client disconnect mid-request | hyper drops the handler future; a submitted job still runs or expires; a read finishes on its own. |
| Token revoked | requests: 401 at their next `verify_in`; sockets: close 1008 on the nudge or within one `heads_interval`; outstanding tickets fail at redemption. |

**Waits.** Head read: `header_read_timeout`. Body: `body_within` until the deadline. Read: `wait_until = deadline`, then `timeout_at(deadline)` on the `spawn_blocking` handle. Write: `not_after = deadline`; ticket wait until `deadline + TICKET_GRACE`; effect until the deadline. WebSocket: each send `send_timeout`; inbound `idle_timeout`; each read the short deadline; close `close_grace`. Ticket and pair-code mutexes are held only for a map operation. Shutdown: its deadline. The only unbounded durations are a started `spawn_blocking` read and a started writer job, which the store states (store §5); PR 7 never awaits either past its own deadline.

**Windows and macOS.** The code has no `cfg`. Loopback behaviour differs only where H5 applies (a rejection while the client still sends a body); PR 7's tests send bodies only after reading a rejection (the ordering cases) or not at all, and PR 9 records the per-OS outcome of the B-8 drain case. `TCP_NODELAY` and `SO_REUSEADDR` are left to the platform defaults except `TCP_NODELAY = true`.

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked on a running daemon (PR 8's binary once it exists; until then the test server of `tests/support/http.rs` on `127.0.0.1:<port>`). `$T` is an owner token, `$A` an agent token, `$P` the port.

| ID | Criterion (EARS) | Source | How to observe |
|---|---|---|---|
| PR7-P-1 | If a request has a wrong or missing `Host`, a wrong `Origin`, a missing or wrong token, or a token kind the route does not accept, then the daemon shall reject it with 421, 403, 401, or 403 and change nothing. | mvp P-20; §2.5 | `curl -i -H 'Host: evil:$P' http://127.0.0.1:$P/api/threads` → 421; `-H "Authorization: Bearer $T" -H 'Origin: http://evil'` → 403; no token → 401; hook token on `/api/threads` → 403; `GET /api/events?stream_id=host` with `$T` shows the same `head_seq` before and after. |
| PR7-P-2 | When a client replays a stream from `after_seq`, pages backward with `before_seq`, or sends a stale epoch, the daemon shall answer per mvp §2.4 with events, a snapshot, or 400/404. | mvp §2.4; PR 7 obligations | `curl -s -H "Authorization: Bearer $T" "http://127.0.0.1:$P/api/events?stream_id=host&after_seq=0&limit=0"` → 400; `limit=1001` → 400; `after_seq=999999` → `mode: snapshot, reason: cursor_ahead`; `epoch=x` → `reason: epoch_changed`; `after_seq=1&before_seq=5` → 400; `stream_id=thread:<unknown uuid>` → 404. |
| PR7-P-3 | When a stream advances, a subscribed WebSocket client shall receive `stream_advanced` or, at the latest, a `heads` message within 30 s. | mvp §2.4; CL-13 | `curl -X POST … /api/ws-ticket -d '{"purpose":"events"}'`, then `websocat "ws://127.0.0.1:$P/api/ws?ticket=…"`, send `{"type":"subscribe","stream_ids":["host"]}`: `hello`, then `heads`; a new thread → `stream_advanced`. |
| PR7-P-4 | When a ticket or pair code is used a second time or after its lifetime, the daemon shall refuse it. | mvp §2.5 | Second `websocat` with the same ticket → HTTP 401; ticket used after 31 s → 401; `POST /api/pair` twice with one code → second 401. |
| PR7-P-5 | When a command is retried with the same `command_id` by the same token, the daemon shall return the stored result and cause no second effect; another token shall get 409 `conflict`. | mvp CL-8; §2.4 | With a registered command (PR 10's first, or the test handler): `curl -X POST …/api/commands/<name> -d '{"command_id":"…","args":{…}}'` twice → equal status and body, one event; the same body with `$A` → 409 `conflict`. |
| PR7-P-6 | When a blob is downloaded, the daemon shall serve its bytes only through a stream that references it, with the safe type and download headers. | mvp §2.5; P-21 | `curl -i "…/api/blobs/<sha>?stream_id=thread:<t>"` → bytes equal, `X-Content-Type-Options: nosniff`, `Content-Disposition: attachment`; another stream → 404. |
| PR7-P-7 | While the daemon serves requests carrying tokens, tickets, and pair codes, its log shall contain none of them and no `/hook` or `/api/pair` body. | mvp §2.5 "Secrets" | Run requests with known secrets, `grep -c 'chd_\|chdt_\|chdp_'` on the log → 0. |

### 6.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| PR7-CL-1 | If a request's `Host` is missing, repeated, or not `127.0.0.1:<port>` / `localhost:<port>`, or its target names another authority, then the daemon shall answer 421 before routing, authentication, or any body byte, and change nothing. | mvp §2.5 common checks; P-20 | `http_layers::host_rejections` (missing, duplicate, other port, other name, `[::1]`, absolute-form target with another host; one known and one unknown path each; store dump unchanged) |
| PR7-CL-2 | When a route checks `Origin`, the daemon shall accept it only per the route's rule (absent or exactly `http://` + the received `Host`; present and matching for `/api/pair`; absent for `/hook` and `/events`), comparing with the received `Host`, and otherwise answer 403. | mvp §2.5; mvp §4 PR 7 "`Origin` vs received `Host`" | `http_matrix::origin_matrix` (every route of `ROUTES` × {absent, matching for `127.0.0.1:<port>`, matching for `localhost:<port>` sent with the other `Host`, other port, `null`, `https://` scheme, duplicate}; expectations from each spec's `OriginRule`) |
| PR7-CL-3 | If a Bearer route gets no `Authorization`, several, another scheme, a malformed token, an unknown or revoked token, or an agent token whose launch is not current or has exited, then the daemon shall answer 401 with nothing changed. | mvp §2.5; P-20 | `http_layers::bearer_rejections` (each case; `agent_token_after_launch_exit` inserts `launch_exited` without revoking) |
| PR7-CL-4 | Every route shall be reachable only through its `RouteSpec`, and for every route and every token kind (and none) the outcome shall be the table's: pass to the handler, 401, or 403. | CL-15; mvp §4 PR 7 "every route × token kind cell" | `http_matrix::route_kind_matrix` (generated from `ROUTES` × {owner, agent, rescue, hook, extension, none}; a handler stub records reaching it), `http_matrix::route_kinds_follow_the_action_table`, `http_matrix::build_router_rejects_unlisted_mounts`, `http_matrix::unknown_paths_are_404`; clippy `disallowed-methods` for direct `Router::route` calls |
| PR7-CL-5 | The action table shall equal mvp §2.5's "Actions" table cell by cell, and object scopes shall follow their definitions on any thread tree. | CL-15; mvp §2.5 | `core::auth::tests::grant_table_matches_mvp` (a literal transcription), proptest `core::auth::tests::self_plus_is_the_descendant_closure`, `…::scope_grants_on_random_trees` (each `Grant` against a brute-force definition) |
| PR7-CL-6 | When a request is rejected for Host, Origin, credential, kind, framing, or size, the daemon shall answer before reading any body byte, in the order Host → Origin → credential → kind → framing → size. | mvp §2.5 "Order", "Rejections" | `http_layers::rejections_precede_body` (head sent, body withheld until the response is read, one case per layer); `http_layers::layer_order` (requests failing several layers get the earliest layer's status) |
| PR7-CL-7 | If a POST has no `Content-Length`, has `Transfer-Encoding`, or declares more than 4 MiB, then the daemon shall answer 411, 400, or 413; a malformed `Content-Length` (`+5`) shall get hyper's 400 and an oversized head 431, without the service being called; a head not completed within `header_read_timeout` shall end the connection (the client sees EOF). | mvp §2.5 rejections; spike B-8; H2–H4 | `http_layers::framing_and_size`, `http_layers::parser_layer_cases` (`Content-Length: +5`, two different lengths, 9 KiB of headers, 65 headers; a counting stub shows no service call), `http_layers::slow_head_is_closed` (EOF within `header_read_timeout` + 1 s, no status) |
| PR7-CL-8 | If a request's body is not complete by its deadline, or a read route has not answered by it, then the daemon shall answer 408 and nothing shall be written. | mvp §2.5 deadlines | `http_layers::slow_body_is_408` (a command and `/api/pair` with a trickled body: 408, no receipt, no token, the pair code still unused because it is read from the body first), `http_reads::read_past_deadline_is_408` (a read held at a test fault point) |
| PR7-CL-9 | When `GET /api/events` is asked, the daemon shall answer per `decide_replay` from one read snapshot: 400 for `limit` 0 or above 1000, for `after_seq` with `before_seq`, and for malformed parameters; 404 for an unknown or unreadable stream with one body; `replay` with `events: []` and `head_seq: 0` for an empty stream; an empty replay at `after_seq = head_seq`; a snapshot for `after_seq` above the head, an epoch mismatch, more than 10 000 events, or more than 8 MiB; a backward page of the nearest events below `before_seq`, empty with `has_more_before: false` at `before_seq = 1`, carrying the server epoch without switching mode on a mismatch. | mvp §2.4; mvp §4 PR 7 obligations; CL-13 | `http_events::forward_cases` (table: each case above), `http_events::backward_cases`, `http_events::byte_bound` (events of 64 KiB inline payload until `cum_bytes` passes 8 MiB at fewer than 10 000 events), `http_events::event_bound` (10 001 small events), `http_events::unreadable_equals_unknown` (bodies byte-equal) |
| PR7-CL-10 | When events are appended between a client's pages, paging forward from each page's last `seq` shall return every event exactly once, and every reply's `head_seq` or `as_of_seq` shall match its events or snapshot. | CL-13; mvp §4 PR 7 "append between pages" | `http_events::append_between_pages` (a writer task appends while a client pages with `limit` 7: the union equals the stream, no duplicate), proptest `http_events::snapshot_as_of_matches_contents` (concurrent appends; `recent_events` ends at `as_of_seq`) |
| PR7-CL-11 | A thread snapshot shall carry every field a client renders when its source exists, and a host snapshot the thread list, worktrees, and host and workspace surfaces. | mvp §2.4 snapshot; mvp §4 PR 7 "snapshot contents restore every rendered field" | `http_events::snapshot_covers_every_field` (a history that sets every `ThreadSnapshot` and `HostSnapshot` field; the reply decodes with `EventsResponse` and none is empty or `null`; a field added to the type without a source fails the exhaustive destructuring in the test) |
| PR7-CL-12 | An agent token shall read only self+ streams, threads, worktrees owned by self+, and blobs referenced by those streams; the `host` stream shall be 404 for it; WebSocket subscriptions shall keep only readable streams. | CL-15; mvp §4 PR 7 "per-resource filtering" | `http_reads::agent_sees_self_plus_only` (a tree of parent, agent, child, grandchild, sibling; each route), `http_ws::subscription_drops_unreadable` |
| PR7-CL-13 | When a blob is requested, the daemon shall serve it only for a readable stream with a `blob_refs` row, with the store's type only if it is `text/plain; charset=utf-8` or `application/json` and `application/octet-stream` otherwise, and the headers `X-Content-Type-Options: nosniff`, `Content-Disposition: attachment`, `Cache-Control: no-store`. | mvp §2.5 blobs; store commitment 3 | `http_blobs::served_only_through_a_referencing_stream`, `http_blobs::type_rule` (text, JSON, `text/html`, disagreeing types), `http_blobs::bad_hash_is_400`, `http_blobs::bytes_equal` |
| PR7-CL-14 | A ticket shall be redeemable once, within 30 s, only while its token is valid, only for its purpose (and its thread for `attach:`), and an `attach:` ticket shall be issued and redeemed only for an owner token; of concurrent redemptions of one ticket exactly one shall succeed. | mvp §2.5; mvp §4 PR 7 "ticket reuse and expiry" | `http_ws::ticket_single_use`, `http_ws::ticket_expires` (injected clock at 29.9 s and 30 s), `http_ws::ticket_dies_with_token`, `http_ws::ticket_purpose_mismatch`, `http_ws::attach_ticket_owner_only`, `http_ws::concurrent_redemption_once` (32 tasks) |
| PR7-CL-15 | A pair code shall exchange once, within 60 s, only with a matching `Origin`, for one new owner token; concurrent exchanges of one code shall yield one token. | mvp §2.5 secrets | `http_pair::single_use`, `http_pair::expires`, `http_pair::origin_required`, `http_pair::concurrent_exchange_once`, `http_pair::lost_after_restart` (a new server on the same store refuses the code) |
| PR7-CL-16 | When a WebSocket opens, its first message shall be `hello` with the store epoch; each `subscribe` shall be answered with the heads of its readable streams; a commit on a subscribed stream shall produce a `stream_advanced` with a head at least the commit's; and a socket whose notifications were lost or lagged shall receive the current heads within `heads_interval`. | mvp §2.4; mvp §4 PR 7 "lost notification recovered by `heads`"; CL-13 | `http_ws::hello_then_heads`, `http_ws::commit_is_announced`, `http_ws::lost_notification_recovered_by_heads` (a fault point drops the socket's notifications; heads within the interval), `http_ws::lagged_socket_resyncs` (the loop held while more than 1024 commits land: a `heads` frame with the latest heads follows), `http_ws::subscribe_racing_commits` (proptest: interleaved subscribes and commits; the client's final heads equal the store's) |
| PR7-CL-17 | When a socket's token is revoked, the socket shall close with 1008 on the revocation nudge, and at the latest at the next heads tick; when the daemon shuts down, sockets shall close with 1001 within `close_grace`; a socket that does not read shall be dropped after `send_timeout`, and one with no inbound frame after `idle_timeout`. | mvp §2.5 "die with their token"; §5 | `http_ws::revocation_closes_socket` (with and without the nudge), `http_ws::shutdown_closes_sockets`, `http_ws::idle_socket_dropped`; send timeout: `ws::tests::slow_consumer_dropped` (unit, a fault point makes the send pending) |
| PR7-CL-18 | One `command_id` shall cause at most one intent, and a repeat by its token with the same request shall return the stored status and body byte for byte; another token or another request shall get 409 `conflict` with one body; an `in_progress` receipt 409 `in_progress` with `retry_after`; an `unknown_after_crash` receipt 409 `unknown_after_crash`. | CL-8; mvp §2.4; mvp §4 PR 7 "receipt HTTP codes" | `http_commands::concurrent_duplicates_one_intent` (32 concurrent requests, a counter handler: one event), `http_commands::replay_is_byte_equal` (non-ASCII body), `http_commands::other_token_conflicts` (the receipt in each state: bodies equal), `http_commands::other_hash_conflicts`, `http_commands::in_progress_while_effect_runs`, `http_commands::unknown_after_crash` |
| PR7-CL-19 | A command shall answer 408 only when its deadline passed before its intent job held the write lock, and then no receipt or intent shall exist; once the intent committed, the reply shall follow the receipt. | mvp §2.5 deadlines; store commitment 1 | `http_commands::expired_before_lock_is_408` (a test job holds the writer past the deadline: 408, no receipt), `http_commands::slow_effect_is_in_progress` (intent committed, effect past the deadline: 409 `in_progress`, later `Replay`), `http_commands::ticket_cap_answers_in_progress` (writer delayed past `deadline + TICKET_GRACE`: 409, and a retry later sees the true state); unit `commands::tests::store_outcomes_map_to_statuses` (every `WriteError` and `SubmitError` through the writer's fault points: `CommitUnknown` → 503 `outcome_unknown`, never 408) |
| PR7-CL-20 | If a command's token is revoked before its intent job runs, or its object check fails, then the job shall write nothing and the reply shall be 401, or 404 for an object the token cannot read and 403 for one it can read but not act on. | CL-15; mvp §2.5 "Order" | `http_commands::revoked_between_guard_and_job` (revocation job queued between the guard read and the intent job), `http_commands::object_denials` (unreadable target 404, readable but refused 403; no receipt) |
| PR7-CL-21 | Every command of mvp §2.4 shall be registered; an unregistered name shall be 404 and a registered unimplemented one 501 `not_implemented` before its body is read, with no receipt; a kind without the command's grant shall get 403 before the body. | mvp §2.4 commands; CL-15 | `http_commands::every_mvp_command_is_registered`, `http_commands::unimplemented_is_501_without_receipt`, `http_commands::kind_refused_before_body` (body withheld) |
| PR7-CL-22 | The command request hash shall be equal for requests whose `args` differ only in key order or whitespace and different for any other change, including the command name. | mvp §2.4 | `commands::tests::hash_is_canonical` (proptest over random JSON with shuffled keys), `commands::tests::hash_covers_name_and_values` |
| PR7-CL-23 | While the daemon serves requests carrying tokens, tickets, and pair codes, no log record shall contain a secret, the query string, or a `/hook` or `/api/pair` body. | mvp §2.5 secrets; mvp §4 PR 7 "secrets absent from logs" | `http_logs::secrets_absent` (a capturing subscriber at `TRACE`; requests with a token in `Authorization`, a ticket in the query, a pair code and a hook body with unique markers, including rejected ones and a handler panic; the captured text contains none), `trace::tests::redact_secrets` |
| PR7-CL-24 | When `ServeHandle::shutdown` runs, no new connection shall be accepted, sockets shall close, and it shall return by its deadline with every connection finished or aborted. | §5 | `http_server::shutdown_stops_accepting`, `http_server::shutdown_returns_by_its_deadline` (a request held in a handler past the deadline: aborted and counted) |
| PR7-CL-25 | Read routes shall map store read outcomes to 503 `busy`, 503 `shutting_down`, or 500, and never to a success. | §2.9 | unit `error::tests::read_outcomes_map` (pool exhausted, store shut down) |

**Mapping.** mvp §4 PR 7 obligations: `limit` 0 and above max → CL-9; empty, unknown, unreadable stream → CL-9; `after_seq = head_seq` and above → CL-9; append between pages → CL-10; epoch mismatch forward and backward → CL-9; byte bound → CL-9; `after_seq` + `before_seq` → CL-9; `before_seq` at 1 → CL-9; lost notification recovered by `heads` → CL-16; snapshot contents → CL-11; every route × token kind cell → CL-4 (with CL-2, CL-3); per-resource filtering → CL-12; `Origin` vs received `Host` → CL-2; ticket reuse and expiry → CL-14; secrets absent from logs → CL-23; receipt HTTP codes → CL-18, CL-19. mvp §6.2: CL-8 → PR7-CL-18, CL-19 (PR 10 adds its own concurrent duplicates); CL-13 → PR7-CL-9, CL-10, CL-11, CL-16 (with PR 17's model and PR 18's reducer); CL-15 → PR7-CL-4, CL-5, CL-12, CL-20, CL-21 (PR 20 adds the rescue and scope cases). mvp P-20 → PR7-CL-1, CL-3, CL-4, CL-6. CL-16 (B-8 per layer) stays PR 9's; PR7-CL-6 and CL-7 cover the same layers for PR 7's routes.

**Known limits** (no test, by design): the window between a revocation without a nudge and the next heads tick, during which a socket can still send stream heads (§2.6); a client on Windows that sends its body before reading a pre-body rejection may see a reset instead of the status (H5, PR 9 records it); `ReadOnly` routes rely on `Effects` being set right in `ROUTES`, which review checks and `route_kinds_follow_the_action_table` does not; the canonical hash treats `1` and `1.0` as different numbers.

## 7. Tests and Completion Commands

**Harness.** `tests/support/http.rs` starts an `HttpServer` on a `127.0.0.1:0` listener with a store in a `tempfile::TempDir` (fixed `Clock` and counter `IdSource` from the store's test support), an injectable monotonic clock for tickets and pair codes, `heads_interval` of 200 ms, and helpers to mint tokens of each kind and build thread trees through the store's `submit`. Requests use a raw HTTP/1.1 client over `tokio::net::TcpStream` (so malformed heads, withheld bodies, and EOF are observable exactly); WebSocket tests use `tokio-tungstenite`. Each test shuts the server and the store down before its directory drops. Fault points (`#[cfg(test)]` hooks: hold a read, hold a socket loop, drop a socket's notifications, make a send pending) are crate-internal, as the store's are; the tests needing them are unit tests in `src/http/tests/`.

**Matrix tests from one table.** `route_kind_matrix` and `origin_matrix` iterate `ROUTES` (the table `build_router` mounts) and derive expectations from each `RouteSpec` and `core::auth::grant`; a route added to the table is tested without editing the test, and a route mounted without a spec fails `build_router` or clippy. The action table has its own literal test against mvp §2.5 (CL-5), so the matrix test cannot pass by deriving expectations from a wrong table.

**Regression rule.** Each named test is seen failing with its behaviour reverted (the Host check removed, the ticket `remove` replaced by a `get`, the receipt lookup skipped, the 408 mapping applied to `CommitUnknown`, the agent filter dropped, the span replaced by `TraceLayer`'s default) and passing with it restored before the PR merges; the PR description lists the reverts.

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

**D1. PR 7 ships as three PRs.** **7a** (server and access): `HttpServer`, the accept loop, `HostLayer`, `GuardLayer`, `ROUTES` with every route mounted (handlers 501), `TokenService` and `verify_in`, `core::auth`, `ReadCtx`/`WriteCtx`, the status mapping, the redaction, `ServeHandle::shutdown`, and the public bodies of OQ2; claims CL-1–CL-7, CL-24, CL-25. **7b** (reads and live): `/api/events`, `/api/threads`, `/api/worktrees`, blobs, tickets, pairing, the WebSocket loop; CL-9–CL-17. **7c** (commands): envelope, hash, `CommandHandler`, registry, receipts; CL-18–CL-22. CL-8 and CL-23 grow with the routes: 7a runs their guard and header cases, 7b adds the pair, ticket, and read cases, 7c the command cases. PR 8 (lifecycle) and PR 9 (`/hook`) need only 7a, so both start once 7a merges; 7b and 7c then run in parallel with them; PR 10 needs 7c, 8, and 9, PR 17 and 18 need 7b. The longest path shrinks from 7 → 8 → 9 → 10 to 7a → 8 → 9 → 10 with 7c off it. Each part is shippable: 7a's matrix and layer tests run against 501 handlers; 7b and 7c add their routes' handlers to mounts already in the table. The two-way split was rejected because its first half (server and reads) would still hold PR 8 and PR 9 behind the sync API and WebSocket, which neither needs.

**D2. Own accept loop on hyper's HTTP/1 builder with a timer**, a 5 s header read timeout, 8 KiB heads, 64 headers, and 128 connections; HTTP/1.1 only (H1, H2; §3).

**D3. Access is decided by one Host layer and one per-route guard built from a static route table**, which is also the only way to build the router (clippy `disallowed-methods`) and the input of the matrix tests. Order: Host → (routing) → Origin → credential → kind → framing → size → deadline; no body byte before the last.

**D4. Tokens are 256-bit `chd_` secrets stored as SHA-256 and checked at use time in the transaction that uses them**, with a `subtle` final comparison; agent tokens also need their launch current and not exited. Tickets (`chdt_`, 30 s) and pair codes (`chdp_`, 60 s) are memory-only, single use by mutex `remove`; restart loses them safely.

**D5. Write routes bound their own waits and never sit under a dropping timeout**; 408 means only `WriteError::Expired`. A ticket not resolved by `deadline + 2 s` answers 409 `in_progress` (retry with the same `command_id`); `CommitUnknown` and `WriterGone` answer 503 `outcome_unknown`.

**D6. Each WebSocket subscribes to the store's broadcast itself, coalesces heads per stream, answers `Lagged` with a heads read, and re-verifies its token on every read and on a revocation nudge.** `subscribe` replaces the set; unreadable streams are dropped silently.

**D7. Agents get 404 on the `host` stream** and read threads and worktrees through filtered routes; objects a token cannot read answer 404, objects it can read but not act on 403, decided by one `deny_for`.

**D8. No mvp command is completed in PR 7**; all are registered as 501 until their PRs, with test-only handlers proving the seam.

**D9. Logs carry route templates, never URIs, queries, header values, or bodies**; secrets have redacted `Debug`, and a pattern scrub is the second line.

### Open Questions

**OQ1. A route for `chaldea open` to obtain a pair code** (changes mvp §2.5's public route table). `chaldea open` must get a code the daemon will accept, and the table has no route that mints one.

| Option | Gains | Losses | Reversibility |
|---|---|---|---|
| A. `POST /api/pair-codes`: owner Bearer, `Origin` must be absent, reply `{ code, expires_in }` | explicit; only non-browser owner clients can mint; no secret at rest | one more public route | additive; removable before PR 18 ships |
| B. Ticket purpose `pair` on `POST /api/ws-ticket` | no new route | overloads the ticket route, its 30 s lifetime, and its Origin rule (browsers could mint) | changes a public purpose set |
| C. A command `open` | uses the command seam | the receipt persists the reply body, i.e. the code | — |

Recommendation: **A**; add the row to mvp §2.5 "Routes" and `open` stays a CLI subcommand (mvp D2).

**OQ2. The public wire bodies PR 7 adds** (changes the public contract additively: new `ts-rs` types in `crates/core`, generated TypeScript and JSON Schema). mvp §2.4–§2.5 fix routes, codes, and messages but not these bodies: `CommandRequest { command_id, args }` and the snake_case command names in the URL; `ErrorBody { code, retry_after? }` with the codes of §2.9 (including 501 `not_implemented` and the 503 codes); `WsTicketRequest { purpose }` / `WsTicketResponse { ticket, expires_in }`; `PairRequest { code }` / `PairResponse { token }` / `PairCodeResponse { code, expires_in }`; `ThreadsResponse { epoch, threads }`, `WorktreesResponse { epoch, worktrees }`. Options: (A) a new `core::http` module with these types, exported with the drift test; (B) leave them untyped in the daemon and let PR 18 write TypeScript by hand. A keeps one source of truth for the PWA and the relay at the cost of fixing names now; B defers naming but splits the contract. Recommendation: **A**, landing with 7a (bodies of 7b and 7c routes included, so the PWA's PR 18 can start from them).

### Where Other Documents Would Change

Proposals, not applied in this PR.

- `docs/mvp-design.md` §2.5 "Routes": add `POST /api/pair-codes` (OQ1).
- `docs/mvp-design.md` §2.4 "Commands": the envelope and the snake_case names (OQ2); and that 409 `in_progress` is also the answer when the intent's outcome is not known by the reply bound (D5), which keeps "408 only if the deadline prevented the commit".
- `docs/mvp-design.md` §2.5 "Actions" / "Order": the hiding rule (404 for unreadable, 403 for readable but refused) and agents' 404 on the `host` stream (D7).
- `docs/mvp-design.md` §3: `core` gains `auth` (named there as "auth policy") and `http` (OQ2); `getrandom` and `subtle` are daemon dependencies (the list names `subtle` under `core`, which does not use it).
- `crates/core`: new modules `auth` and `http` only; no existing type changes.
- `docs/design/event-store.md`: none; the extension scope column is PR 20's v2 migration.
