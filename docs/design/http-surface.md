# Design: HTTP Surface (MVP PR 7)

Status: design. Scope is the PR 7 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): HTTP route and action matrices, tokens, pairing, the sync API, blobs, WebSocket notifications, and command receipts, in `crates/daemon` on `axum` + `hyper` + `tower-http` (mvp §2.5, arch §3). The contract is mvp §2.4 (replay, backward history, live notifications, commands and receipts, restart), mvp §2.5 (common checks, routes, token kinds, actions, order, secrets, rejections), the PR 7 design obligations of mvp §4, and the claims of mvp §6.2 that name PR 7 (CL-8, CL-13, CL-15). It builds on the store of `docs/design/event-store.md` (cited as "store §N"). It proposes a split into three PRs (§8 D1).

**What PR 7 decides and what it leaves.** PR 7 owns everything between the socket and a store call: connection handling, the order of checks, credentials and their lifetimes, the authorization tables and the shared object checks, the mapping of store outcomes to status codes, the read routes, the WebSocket loop, the command envelope, receipts, runner, and dispatch seam, and the one registry that owns every task outliving a request. It does not decide daemon lifecycle, port, state directory, the single-instance lock, the process panic hook, or what the daemon does on a fatal store outcome (PR 8), hook ingest and binding (PR 9), any command's effect (PR 10, 16, 20, 21), attach byte handling (PR 11), `POST /events` scope semantics and A2UI (PR 20), the doctor (PR 21), or the PWA (PR 18). For each of these it provides the seam named in §2.11 and §4.

**Commitments from PR 6 this design keeps** (store §2.2 "What callers learn", §2.5, §2.4, §5 "Waits"):

1. The store returns `SubmitError`, `WriteError`, and `ReadError` kinds and never chooses a status code; PR 7 owns the mapping (§2.9), including mvp §2.5's rule that a command answers 408 only if its deadline prevented the intent commit (§2.8).
2. A command whose intent job ended `CommitUnknown` has, after a restart, a receipt that is absent, `completed`, or `in_progress` (→ `unknown_after_crash`); PR 7 answers that request with 503 `outcome_unknown`, reports the outcome to PR 8's lifecycle sink while serving, and lets the retry after restart find out (§2.2, §2.8).
3. The blob download type for `(stream_id, sha256)` is the store's (`application/octet-stream` on disagreement); PR 7 applies mvp §2.5's serving rule and headers (§2.7).
4. The v1 schema is frozen; PR 7 needs no schema change. It moves the store's `TokenKind` enum to `core::auth` and re-exports it from the store (a Rust type move), and adds one read method, `ReadTx::blob_ref_exists` (§2.7). No stored value changes. The extension token's scope (PR 20) is a numbered migration (§4).
5. `Store::read` is synchronous: every async caller runs it in `spawn_blocking`; read closures and write jobs capture only data (ids, hashes, parsed arguments, principals, a non-blocking semaphore handle), never a network, PTY, supervisor, or channel handle (§2.3, §2.8).

Sources read for this document (paths are relative to each root):

| Source | Revision | Read for |
|---|---|---|
| This repository | `a08820b` | `docs/mvp-design.md`, `docs/architecture.md` §3–§4, `docs/design/event-store.md`, `crates/core/src/{sync,command,ids,lib,payload}.rs`, `crates/daemon/src/store/{mod,error,readers,rows,write_tx,writer,clock}.rs`, `Cargo.toml`, `Cargo.lock`, `.github/workflows/ci.yml`, `docs/spikes/pty-hooks-spike.md` (B-8, F-3c) |
| T3 Code (`pingdotgg/t3code`) | `10586f9e` | `apps/server/src/orchestration-v2/ThreadStream.ts`, `auth/{utils,PairingGrantStore,SessionStore,EnvironmentAuth}.ts`, `persistence/AuthPairingLinks.ts`, `persistence/Migrations/{031_AuthAuthorizationScopes,055_OrchestrationV2}.ts`, `ws.ts` |
| `hyper` | crate 1.12.0 | `src/server/conn/http1.rs`, `src/proto/h1/{role,conn,dispatch}.rs`, `src/headers.rs`, `src/common/time.rs`, `src/error.rs` |
| `axum` | crate 0.8.9 | `Cargo.toml` (features), `src/serve/{mod,listener}.rs`, `src/extract/ws.rs`, `src/routing/{method_routing,method_filter,route}.rs` |
| `tower-http` | crate 0.7.1 | `src/timeout/service.rs`, `src/trace/make_span.rs`, `src/sensitive_headers.rs` |
| `tokio` | crate 1.54.1 | `src/runtime/task/list.rs`, `src/sync/notify.rs`, `src/time/timeout.rs` |
| `futures-util` | crate 0.3.34 | `src/abortable.rs`, `src/future/mod.rs` |
| `tungstenite` | crate 0.29.0 | `src/handshake/client.rs` |
| `hyper-util` 0.1.21, `getrandom` 0.4.3, `subtle` 2.6.1 | crates | feature lists, `getrandom::fill` |

**Facts this design relies on.** T-facts are exercised by named tests of §7 on both runners; D-facts rest on the cited source and are not re-proven.

| # | Fact | Where | Kind |
|---|---|---|---|
| H1 | `axum::serve` builds a `hyper_util` auto connection builder per connection with no timer, so hyper's default 30 s header read timeout is dropped with a warning, and it has no connection bound. | axum `src/serve/mod.rs:388-396`; hyper `src/common/time.rs:73-85` | D |
| H2 | hyper's HTTP/1 builder has `timer`, `header_read_timeout` (panics if set without a timer), `max_header_size`, `max_headers`, `keep_alive`; `serve_connection(..).with_upgrades()` lets an upgraded connection reach axum's `WebSocketUpgrade`. The header read timeout is armed when the connection starts reading a head, before any byte arrives. A response whose `Connection` header carries `close` is encoded as the connection's last message, and hyper closes the connection after it instead of reading another request; a `101` is always the last HTTP message. With `keep_alive(false)` instead, hyper appends `close` to every response's `Connection` value and writes all values on one line, so a `101` would carry `Connection: upgrade, close`. | hyper `src/server/conn/http1.rs:201`, `:245-262`, `:276-279`, `:341`, `:356`, `:370-373`, `:429`, `:481-483`; `src/proto/h1/conn.rs:224-236`, `:710-720`, `:1100-1117`; `src/proto/h1/role.rs:404-405`, `:847-862` | T (`http_layers::slow_head_is_closed`, `http_layers::one_request_per_connection`, `http_ws::*`) |
| H3 | hyper rejects a `Content-Length` that is not all digits (`+5`), and two different values, as a header parse error answered 400 before the service is called; a head over the size or count bound is 431. | hyper `src/headers.rs:101-125`; `src/proto/h1/role.rs:295-312`, `:493-500`; `src/error.rs:513`, `:637-638` | T (`http_layers::parser_layer_cases`) |
| H4 | With `Transfer-Encoding` present, hyper drops `Content-Length` from the parsed headers and keeps `Transfer-Encoding`. | hyper `src/proto/h1/role.rs:275-293` | T (`http_layers::te_with_content_length_is_400`) |
| H5 | When a response is sent before the request body was read, hyper drains what it can read without waiting and otherwise closes the read side. On Windows a close with unread data can reset the connection (spike F-3c). | hyper `src/proto/h1/conn.rs:875-891`, `src/proto/h1/dispatch.rs:242-243`; spike `docs/spikes/pty-hooks-spike.md` §3 | D here; PR 9 records the observed outcome per OS |
| H6 | `tower-http`'s `TimeoutLayer` answers 408 and drops the inner future. | `tower-http` `src/timeout/service.rs:30-35` | D (why write routes have no dropping timeout) |
| H7 | `tower-http`'s default `MakeSpan` records the full request URI, query included. | `tower-http` `src/trace/make_span.rs:78-90` | D (why §2.10 has its own span) |
| H8 | axum's `WebSocketUpgrade` checks method, `Connection`, `Upgrade`, `Sec-WebSocket-Key`, and version, not `Origin`; tungstenite answers pings itself; `max_message_size`, `max_frame_size`, and `max_write_buffer_size` are per-socket settings. | axum `src/extract/ws.rs:189-201`, `:449-470`, `:781-790`, `:1220-1221` | D |
| H9 | `getrandom::fill` fills a buffer from the OS CSPRNG; `uuid` v4 (the store's epoch and event ids) uses the same `getrandom` 0.4.3. | `getrandom` `src/lib.rs:87`; `Cargo.lock` (`uuid` → `getrandom 0.4.3`) | D |
| H10 | hyper's connection future completes when it hands the transport to an upgrade, and axum's `on_upgrade` runs its callback in a task it spawns itself, so no task the server owns holds an upgraded socket unless the callback hands it over. | hyper `src/server/conn/http1.rs:568-576`; axum `src/extract/ws.rs:350-365` | T (`http_ws::shutdown_aborts_a_blocked_socket`) |
| H11 | axum's `MethodRouter` sends `HEAD` to a `GET` handler and empties the body of every `HEAD` response at the top level; it adds an `Allow` header only to a response that has none; `MethodFilter` implements `TryFrom<Method>`. | axum `src/routing/method_routing.rs:1157-1158`; `src/routing/route.rs:160-185`; `src/routing/method_filter.rs:88-91` | T (`http_matrix::methods_are_exact`) |
| H12 | hyper may read request body bytes into its own buffer before the service polls the body. | hyper `src/proto/h1/dispatch.rs:166`, `:247`, `:317` | D (why §2.2 states the boundary as "no application body use") |
| H13 | axum's listener ignores per-connection accept errors (`ConnectionRefused`, `ConnectionAborted`, `ConnectionReset`) and sleeps 1 s after any other accept error instead of ending. | axum `src/serve/listener.rs:140-165` | D (the accept loop follows it) |
| H14 | The store's `ShutdownReport` holds `readers_returned`, `readers_abandoned`, `writer_exited`, `jobs_run`, `jobs_expired`, and `checkpoint`; it reports no job's `CommitUnknown` or `WriterGone`, and the store's failed flag has no public accessor. | `crates/daemon/src/store/mod.rs:246-260`, `:269` | D (why §2.2 decides fatal reporting at shutdown itself) |
| H15 | The writer replies `Expired` only when it takes a job whose `not_after` has passed, or when it obtains the lock after `not_after`; both happen after the deadline, never before it. | `crates/daemon/src/store/writer.rs:338-340`, `:360-363`; store §2.2 steps 1 and 3 | D (why §2.8 waits a reply grace past the deadline) |
| H16 | `futures_util::future::AbortHandle::new_pair` returns a handle and a registration; an `Abortable` built from the registration returns `Aborted` at its next poll once the handle was aborted, including when the abort came before its first poll, and dropping it drops the inner future. On a runtime whose task list is closed, `tokio::spawn` drops the new future on the calling thread at once. `Notify::notify_one` stores a permit when no waiter is registered, which the next `notified().await` consumes. `tokio::time::timeout_at` polls the inner future before its timer, so both can be ready when the task resumes. | futures-util `src/abortable.rs:98-108`, `:132-145`; `src/future/mod.rs:117-125` (needs `alloc`, which the pinned `std` feature enables); tokio `src/runtime/task/list.rs:137-143`, `:279-282`; `src/sync/notify.rs:520-523`; `src/time/timeout.rs:210-223` | T (`registry::tests::*`, `error::tests::late_read_success_is_408`) |
| H17 | The pinned WebSocket client compares the whole `Connection` value of a `101` with `Upgrade` (ASCII case-insensitive), so `Connection: upgrade, close` fails the handshake. Chromium and Firefox accept extra tokens; WebKit compares the whole value. | tungstenite `src/handshake/client.rs:243-252` | T (`http_ws::hello_then_heads` through `tokio-tungstenite`); D for browsers |

## 1. Goal and Dependencies

**Goal.** An `http` module in `chaldea-daemon` that serves mvp §2.5's route table on a listener PR 8 hands it; decides every request in the order Host → method → Origin → credential → token kind → action → framing → size before the application polls, decodes, or uses any body byte; answers reads from one store snapshot each; issues and checks tokens, tickets, and pair codes; notifies WebSocket clients of stream heads; and runs commands through a receipt, a daemon-owned runner, and a dispatch seam that later PRs fill. Everything that outlives a request future is owned by one task registry with one closing transition. Every claim has a test on `macos-latest` and `windows-latest`.

**Files.**

```
crates/core/src/auth.rs              # Sans-IO policy: TokenKind (moved from the store), Action, Grant, MVP_COMMANDS, the action table, object-scope functions
crates/core/src/http.rs              # public wire bodies: CommandRequest, ErrorBody, WsTicket*, PairCode*, Pair*, ThreadsResponse, WorktreesResponse (§8 D13)
crates/daemon/clippy.toml            # disallows axum Router::{route, route_service, nest, nest_service, merge, fallback, fallback_service} (§2.2)
crates/daemon/src/store/rows.rs      # `pub use chaldea_core::auth::TokenKind` replaces its own enum
crates/daemon/src/store/readers.rs   # + ReadTx::blob_ref_exists (§2.7)
crates/daemon/src/http/mod.rs        # HttpServer, HttpConfig, LifecycleSink, ServeHandle, HttpShutdownReport
crates/daemon/src/http/registry.rs   # TaskRegistry: the one owner of every task that outlives a request (§2.2)
crates/daemon/src/http/listener.rs   # accept loop, ConnIo (request origin, write stall), hyper http1 builder
crates/daemon/src/http/routes.rs     # RouteSpec, ROUTES, Mount, build_router (the only Router::route call)
crates/daemon/src/http/guard.rs      # OuterLayer (span, RequestStart, Host), GuardLayer (per route: method → … → size)
crates/daemon/src/http/auth.rs       # Secret, TokenService, verify_secret_in / verify_hash_in, Principal, deny_for, ObjectScope
crates/daemon/src/http/tickets.rs    # TicketStore, PairCodes (memory, single use)
crates/daemon/src/http/ctx.rs        # ReadCtx, WriteCtx, run_write (the one submit-and-observe path), observe_write
crates/daemon/src/http/error.rs      # ApiError, codes, the store-outcome mapping, extractor-rejection mapping (§2.9)
crates/daemon/src/http/sync_api.rs   # GET /api/events, /api/threads, /api/worktrees
crates/daemon/src/http/blobs.rs      # GET /api/blobs/{sha256}
crates/daemon/src/http/ws.rs         # POST /api/ws-ticket, GET /api/ws socket loop
crates/daemon/src/http/pair.rs       # POST /api/pair, POST /api/pair-codes (§8 D12)
crates/daemon/src/http/commands.rs   # envelope, duplicate scan, request identity, CommandHandler, CommandRegistry, CommandRunner
crates/daemon/src/http/trace.rs      # request span, Secret redaction
crates/daemon/src/http/tests/        # unit tests needing fault points (§7)
crates/daemon/tests/http_*.rs        # integration tests (§7)
crates/daemon/tests/support/http.rs  # test server on 127.0.0.1:0, raw HTTP/1.1 client, WS client, capturing log subscriber
```

**Dependencies.** Root `[workspace.dependencies]`, exact pins with one comment line each, in the existing style:

```toml
# HTTP routing and extractors (mvp §2.5, arch §3); no defaults: no form, no json, no axum tracing, no http2.
axum = { version = "=0.8.9", default-features = false, features = ["http1", "tokio", "ws", "query", "matched-path"] }
# The HTTP/1 connection builder with a timer, header limits, and upgrades (H1, H2); request parsing is hyper's.
hyper = { version = "=1.12.0", default-features = false, features = ["server", "http1"] }
# TokioIo, TokioTimer, and the tower-to-hyper service adapter for the own accept loop (H1).
hyper-util = { version = "=0.1.21", default-features = false, features = ["tokio", "server", "http1", "service"] }
# Layer and ServiceBuilder for the guard.
tower = { version = "=0.5.3", default-features = false, features = ["util"] }
# Sensitive-header marking (§2.10); no TimeoutLayer, no TraceLayer, no body-limit layer (§3).
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

Workspace changes: `tokio` keeps `default-features = false`; `crates/daemon` adds the features `rt`, `net`, `time`, `macros` (`sync` stays). `serde_json` gains the `raw_value` feature for `args` (§2.8); the existing `schemars` pin already enables its `raw_value` feature. Dev-dependencies of `crates/daemon`: `tokio` with `rt-multi-thread`; `tokio-tungstenite = "=0.29.0"` (the WS test client; the version axum 0.8.9's `ws` feature already locks); `tracing-subscriber = { version = "=0.3.23", default-features = false, features = ["fmt", "std"] }` (the capturing subscriber). `sha2 =0.11.1` (already a daemon dependency) hashes secrets and command requests. The `ws` feature adds `sha1 0.10.7` (the WebSocket accept key, `digest` 0.10 alongside `sha2`'s `digest` 0.11). `axum 0.8.9` needs Rust 1.80, `hyper-util 0.1.21` 1.85; the toolchain is 1.96.0. The PR records the resolved lock versions (axum-core 0.5.6, http 1.5.0, http-body 1.1.0, matchit 0.8.4, tungstenite 0.29.0) in its description.

**Workspace and CI.** No new CI job. `cargo test --workspace --locked` runs the HTTP tests on both runners against `127.0.0.1:0` listeners and stores in the OS temp directory. The `core` dependency check gains `hyper|tower|getrandom` in its forbidden list, since `core::auth` and `core::http` are pure:

```
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum|hyper|tower|getrandom) ' <<<"$deps"
```

No test is Windows-only, so the sentinel list is unchanged. `lefthook.yml` mirrors the changed check.

## 2. Types and Functions

Names may change in review; shapes may not.

### 2.1 The Rules

1. **The platform decides framing; one guard decides access.** Request parsing, head limits, and the slow-head bound are hyper's (H2, H3). Every access decision before the handler runs is one outer layer plus one tower layer per route built from that route's `RouteSpec`; the router is built only from the route table, and a literal inventory written independently of that table equals it and tests the real router (§2.2, §7).
2. **The database decides; memory only remembers what is safe to forget.** Tokens and receipts live in SQLite and are checked at use time, by hash, inside the transaction that uses them (§2.4). Tickets and pair codes live in memory, single use under one mutex each; a restart drops them, and losing one only makes its holder ask for a new one.
3. **One task registry owns everything that outlives a request future** (§2.2): the accept loop, connections, upgraded sockets, plain write observers, and command runners. A task's entry exists before the task can run; registration fails once shutdown has closed the registry, in the same critical section in which shutdown snapshots it; work admitted before that finishes within the drain budget.
4. **One deciding point per concurrent operation** (§2.12), and **every wait has a deadline** (§5). A committed intent's effect, its receipt completion, and its fatal outcome belong to its registered runner, never to the request (§2.8).

### 2.2 Server, Registry, Routes, Guard

```rust
pub struct HttpConfig {
    pub port: u16,                          // the bound port; Host must name it
    pub short_deadline: Duration,           // 5 s: every request without its own budget, from its connection's first head byte (ConnIo)
    pub write_reply_grace: Duration,        // 100 ms: how long past the deadline a submitted write waits for the writer's reply (§2.8); reads have none
    pub header_read_timeout: Duration,      // 5 s (H2): slow or idle heads end the connection (EOF)
    pub write_stall_timeout: Duration,      // 30 s with a write pending and no byte accepted → the connection ends
    pub max_header_bytes: usize,            // 8 KiB (spike B-8); over → 431 (H3)
    pub max_body_bytes: u64,                // 4 MiB (mvp §2.5)
    pub max_connections: usize,             // 128; the accept loop waits for a permit
    pub blob_slots: usize,                  // 4 blob bodies in memory at once (§2.7)
    pub ws: WsConfig,                       // §2.6
    pub ticket_ttl: Duration,               // 30 s
    pub pair_code_ttl: Duration,            // 60 s
    pub clock: Arc<dyn MonotonicClock>,     // request origins, deadlines and their cutoff checks, tickets, pair codes; tests inject
}
/// PR 8's seam: told of every fatal store outcome `run_write` meets while serving, and of an uncompleted receipt.
pub trait LifecycleSink: Send + Sync + 'static { fn store_fatal(&self, f: StoreFatal); }
pub enum StoreFatal { CommitUnknown, WriterGone, Failed, ReceiptUncompleted { command_id: CommandId } }
pub struct HttpServer { /* Arc<AppState>: store, config, TokenService, TicketStore, PairCodes, CommandRegistry,
                          TaskRegistry, blob slots, revocation nudge, LifecycleSink */ }
impl HttpServer {
    pub fn new(cfg: HttpConfig, store: Store, registry: CommandRegistry, sink: Arc<dyn LifecycleSink>, mounts: Mounts) -> HttpServer;
    pub fn serve(&self, listener: tokio::net::TcpListener) -> ServeHandle;   // the listener is PR 8's, bound to 127.0.0.1
    pub fn tokens(&self) -> &TokenService;     // PR 8 (owner token file), PR 10 (launch tokens), PR 20
    pub fn nudge_revocation(&self);            // re-check every WebSocket's token now (§2.6)
}
impl ServeHandle {
    /// Closes the registry, stops accepting, asks every task to finish, waits until `deadline`,
    /// requests abort of the rest, and reports.
    pub async fn shutdown(self, deadline: Instant) -> HttpShutdownReport;
}
pub struct HttpShutdownReport { pub exited: KindCounts, pub abort_requested: KindCounts }  // per kind: accept, connection, socket, write, runner
```

**`TaskRegistry`** (`registry.rs`): the one owner of every task that must end at shutdown.

```rust
struct RegistryState { closing: bool, next_id: u64, tasks: HashMap<u64, (TaskKind, futures_util::future::AbortHandle)> }
pub struct TaskRegistry { state: std::sync::Mutex<RegistryState>, empty: tokio::sync::Notify }
impl TaskRegistry {
    /// Spawns `fut` as a registered task, or refuses once closing (see below for the order of steps).
    pub fn spawn(&self, kind: TaskKind, fut: impl Future<Output = ()> + Send + 'static) -> Result<(), Closing>;
}
```

`spawn` takes three steps: (1) create an `AbortHandle` / `AbortRegistration` pair (H16); (2) under the mutex, refuse if `closing`, otherwise allocate the id and insert `(kind, handle)`, then release; (3) `tokio::spawn` a wrapper whose fields are `Abortable::new(fut, registration)` and then the exit guard (holding the id); fields drop in declaration order, so the guard drops last, after everything the inner future owns, also when the wrapper is dropped before its first poll. The entry exists before the task is spawned, so it precedes the task's first action; shutdown can abort an entry even before its task is spawned, and the task then ends at its first poll. The mutex is never held while spawning, so a future dropped inline by `tokio::spawn` (a closed runtime, H16) or a task cancelled before its first poll removes its entry through the guard without contention. Dropping the guard removes the entry under the mutex; if the registry is closing and now empty, it calls `empty.notify_one()`, whose permit survives until shutdown waits (H16). The mutex is held only for a flag check and map operations, never across `.await`. Nothing that outlives a request is spawned any other way; finished tasks remove themselves, so nothing reaps while serving. Abort through the handle is cooperative: it takes effect at the task's next poll, so a task blocked outside an `.await` keeps running (§6.2 "Known limits").

**Who registers what, and when.** The accept loop is itself a registered task (kind `accept`), registered by `serve`. A connection task is registered before the accepted stream is served; a stream accepted after `closing` finds registration refused and is dropped, so no connection task is admitted after closing. An upgrade registers its socket task before the final admission read, the ticket's removal, and `101` (§2.4, §2.6). A plain write registers its observer before it submits; a command registers its runner before it submits, and the runner performs its own writes without registering again (§2.3, §2.8). A refused registration answers 503 `shutting_down` with nothing done. `closing` stops new registered work only: a connection already being served may still dispatch a read (reads are not registered tasks) before it processes the shutdown signal; such a read has no effect and ends at its own deadline.

**Shutdown** (`ServeHandle::shutdown(deadline)`): (1) under the lock, set `closing` and snapshot the entries, one transition, so no task registers after the snapshot; (2) signal every task (a `watch`): the accept loop stops and, on exit, drops the listener, connections call `graceful_shutdown`, sockets send 1001 best effort, and runners and observers continue, since admitted work finishes within the drain budget; (3) until `deadline`: under the lock, if no entry is left, stop; otherwise release the lock and await `empty.notified()` (a permit from a last exit between the check and the await is kept, H16); (4) at `deadline`, copy the remaining abort handles under the lock, release it, then `abort()` each and count it under `abort_requested`, separately from `exited`: an abort request does not prove the task has exited; the task ends at its next poll (or when the runtime drops it), and its guard then removes the entry. The listener is released before `shutdown` returns exactly when the report counts the accept task under `exited`: the listener belongs to the accept task's future, which drops before its guard removes the entry.

**Fatal outcomes of a dropped observer.** A runner or observer that is dropped before it sees its job's result does not report that result; a job the writer still holds may end `CommitUnknown` or `WriterGone` with nobody told. This is decided, not missed. Drops happen only after the drain deadline, when the daemon is already stopping; the job's effects are all or nothing in the database (store §2.2); and PR 8 keeps the single-instance lock until it observes the writer's exit (or the process exits) and reconciles receipts before serving again (commitment 2; §4 hand-off), so no in-memory report is needed. The store does not report such an outcome either (H14).

**Accept loop** (`listener.rs`). Its own loop instead of `axum::serve`, because `axum::serve` sets no timer and so no header read timeout, and has no connection bound (H1). The loop `select!`s on the shutdown signal and on acquiring a connection permit (`Semaphore(max_connections)`, so excess clients wait in the kernel backlog); holding a permit, it `select!`s on the shutdown signal and `accept`. Per-connection accept errors are ignored, any other accept error is logged and the loop sleeps 1 s (still selecting on shutdown) and continues (H13); a failing `set_nodelay` is logged and the connection is served anyway. Each stream is wrapped in `ConnIo` and served with `hyper::server::conn::http1::Builder` configured with `timer(TokioTimer)`, `header_read_timeout`, `max_header_size`, `max_headers(64)`, `keep_alive(true)`, then `serve_connection(TokioIo, TowerToHyperService(app)).with_upgrades()` (H2), in a registered task that selects on its connection and the shutdown signal (calling `graceful_shutdown`). The listener is owned by the accept loop's task, so shutdown's wait for an empty registry includes its release. HTTP/2 is not enabled. **One request per connection:** `OuterLayer` sets `Connection: close` on every final response except a successful `101`, so hyper encodes it as the connection's last message and closes the connection after it (H2); hyper's own 400 and 431 close the connection too. A successful `101` keeps exactly `Connection: Upgrade`, because the pinned client and WebKit reject any other value (H17); hyper's builder keep-alive therefore stays enabled, since disabling it would append `close` to the `101` (H2). Each request's origin is its own connection's first head byte, and pipelined or partly buffered later requests are never dispatched. The cost is one loopback TCP connect per request. Clients (the CLI, the hook emitter, PR 18's PWA) must not rely on connection reuse.

**`ConnIo`.** (a) **Request origin:** the instant, on the server's clock (`HttpConfig::clock`), of the first read on the connection that returns bytes; `OuterLayer` takes it as `RequestStart`. A connection carries one request, so this is that request's first head byte. (b) **Write stall:** a timer runs only while a write is pending and no byte has been accepted since it started; progress resets it, and no pending write disarms it (an idle upgraded socket is governed by `WsConfig::idle_timeout`). Expiry fails the write with `TimedOut`, which ends the connection and frees what hyper buffered.

**Route table** (`routes.rs`). One static table, the single production source for the router and the guard:

```rust
pub struct RouteSpec {
    pub method: Method, pub path: &'static str,        // axum path syntax, e.g. "/api/blobs/{sha256}"
    pub mount: MountKind,                              // Read | Write | Upgrade
    pub origin: OriginRule,                            // AbsentOrMatching | PresentMatching | MustBeAbsent | NotChecked (static PWA)
    pub credential: Credential,                        // None | Bearer(&'static [TokenKind]) | Ticket(TicketPurposeKind) | PairCode
    pub action: ActionRule,                            // None | Fixed(Action) | ByCommandName (through core::auth::MVP_COMMANDS)
    pub body: BodyRule,                                // NoBody | Json
    pub owner_pr: u8,                                  // the PR that implements the handler; until then 501
}
pub static ROUTES: &[RouteSpec] = &[ /* the table below */ ];
impl Mount {   // typed constructors; each panics at startup when `spec.mount` differs
    pub fn read(spec: &'static RouteSpec, h: impl ReadHandler) -> Mount;       // gets ReadCtx only; wrapped in timeout_at(deadline) → 408
    pub fn write(spec: &'static RouteSpec, h: impl WriteHandler) -> Mount;     // gets WriteCtx; never wrapped in a dropping timeout
    pub fn upgrade(spec: &'static RouteSpec, h: impl UpgradeHandler) -> Mount; // gets an admitted upgrade (§2.6)
}
pub(crate) fn build_router(mounts: &[Mount], state: AppState) -> Router;  // the only Router::route call
```

| Method, path | Mount | Origin | Credential (kinds) | Action | Handler PR |
|---|---|---|---|---|---|
| `GET /`, `GET /assets/{*path}` | Read | NotChecked | none | — | 18 |
| `POST /api/pair` | Write | PresentMatching | PairCode | — | 7 |
| `POST /api/pair-codes` (§8 D12) | Read (memory only) | MustBeAbsent | Bearer(owner) | — | 7 |
| `POST /api/ws-ticket` | Read (memory only) | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadStream` (an `attach:` purpose also `Attach`, checked on the body) | 7 |
| `GET /api/ws` | Upgrade | AbsentOrMatching | Ticket(events) | — | 7 |
| `GET /api/attach/{thread}` | Upgrade | AbsentOrMatching | Ticket(attach) | `Attach` | 11 |
| `GET /api/events` | Read | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadStream` | 7 |
| `GET /api/blobs/{sha256}` | Read | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadBlob` | 7 |
| `GET /api/threads` | Read | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadThreadList` | 7 |
| `GET /api/worktrees` | Read | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadWorktrees` | 7 |
| `GET /api/adapters` | Read | AbsentOrMatching | Bearer(owner, agent, rescue) | `ReadAdapters` | 10 |
| `GET /api/doctor` | Read | AbsentOrMatching | Bearer(owner, rescue) | `ReadDoctor` | 21 |
| `POST /api/commands/{name}` | Write | AbsentOrMatching | Bearer(owner, agent) | ByCommandName | 7 seam; each command its PR |
| `POST /hook` | Write | MustBeAbsent | Bearer(hook) | — | 9 |
| `POST /events` | Write | MustBeAbsent | Bearer(owner, rescue, extension) | `PostEvents` | 20 |

**Read routes cannot write; write routes cannot time out by dropping.** Only a spec whose `mount` is `Write` can be mounted with `Mount::write` (each `Mount` constructor checks the spec's kind), the guard dispatches on the typed endpoint and constructs `WriteCtx` only in its write arm, and it wraps only read handlers in `timeout_at(deadline)` → 408 (H6 is why write routes are not). The store handle is private to `ctx.rs`, so a handler reaches the store only through `ReadCtx` or `WriteCtx`. The two memory-only routes are read mounts: a 408 there leaves at most an unreturned ticket or code that expires unused.

**Building the router.** `build_router` registers each mount's path with `route_service`, behind `GuardLayer::new(spec)`, so every method of the path reaches the guard, which compares it with the spec's method first and answers a wrong one itself with its `Allow` (no `MethodRouter`, so axum neither routes `HEAD` to a `GET` handler nor adds its own `Allow`, H11); it adds a router fallback for unknown paths. hyper sends no body for a `HEAD` response, so the `HEAD` 405 has the headers of a 405 and an empty body. `crates/daemon/clippy.toml` lists axum's `Router::route`, `route_service`, `nest`, `nest_service`, `merge`, `fallback`, and `fallback_service` under `disallowed-methods`; `build_router` carries the one `#[allow(clippy::disallowed_methods)]`. `build_router` refuses (panics at startup; tested) a mount whose spec is not in `ROUTES` and a `ROUTES` entry with no mount. Production `Mounts::mvp()` mounts every route, with 501 handlers for routes whose PR has not merged.

**Guard order** (mvp §2.5 "Order", "Rejections"):

| Step | Where | Check | Failure |
|---|---|---|---|
| 0. Parse | hyper (H2, H3) | head within `header_read_timeout`, `max_header_size`, `max_headers`; valid framing headers | EOF for a slow head; 431; 400 (hyper's own responses, §2.9) |
| 1. Span, start, Host | `OuterLayer`, before routing | opens the request span; inserts `RequestStart` (the request origin from `ConnIo`); on the way out, sets `Connection: close` on every response except a successful `101`; exactly one `Host`, equal to `127.0.0.1:<port>` or `localhost:<port>` (`localhost` ASCII case-insensitive); a request target in absolute form must carry exactly that authority as a whole string (no userinfo, same port) | 421 `misdirected_request` |
| — | router | known path | 404 `not_found` (fallback, an `ErrorBody`) |
| 2. Method | `GuardLayer` | the method equals the spec's exactly (`HEAD` is not accepted on `GET` routes) | 405 `method_not_allowed` with `Allow: <spec method>` |
| 3. Origin | `GuardLayer` | per `OriginRule`: at most one `Origin`; `AbsentOrMatching`: absent or exactly `http://` + the received `Host` value; `PresentMatching`: present and matching; `MustBeAbsent`: absent. `Origin: null` is present and never matches | 403 `origin_rejected` |
| 4. Credential | `GuardLayer` | Bearer: parsed and verified in one read (§2.4). Ticket: the query is exactly one `ticket` parameter (a missing or repeated `ticket` → 401, any other parameter → 400 `bad_query`); the ticket is **peeked** and its bound token verified in one read, nothing removed (§2.4). PairCode: none here; the code is in the body | 401 `unauthorized` with `WWW-Authenticate: Bearer`; when the lookup itself fails, 503 / 500, or 408 at the read cutoff (§2.3, §2.9) |
| 5. Kind and action | `GuardLayer` | the verified kind is in the route's kinds; `Fixed(a)`: `grant(kind, a)` exists; `ByCommandName`: `{name}` is in `MVP_COMMANDS` (else 404 `not_found`) and `grant(kind, action_of(name))` exists | 403 `forbidden` |
| 6. Framing | `GuardLayer` | any `Transfer-Encoding` → 400 `bad_request` (checked first: hyper has removed `Content-Length` when both are present, H4); `POST`: exactly one parsed `Content-Length` (else 411 `length_required`; hyper has already collapsed equal duplicates and answered differing ones with its own 400, H4); `GET`: no `Content-Length` other than a zero length, however spelled (else 400) | as listed |
| 7. Size | `GuardLayer` | `Content-Length` ≤ `max_body_bytes` (else 413 `payload_too_large`); the body is wrapped in `http_body_util::Limited` as the second line | 413 |
| 8. Unimplemented | `GuardLayer` | the route's or command's handler PR has not merged | 501 `not_implemented` |
| 9. Deadline | `GuardLayer` | `Deadline = RequestStart + budget` is in the extensions; no handler is invoked once the deadline has passed; read mounts are wrapped in `timeout_at(deadline)`, and a result taken at or after it is discarded | 408 |

**One deadline origin.** Every route, `/hook` included, measures its budget from `RequestStart`, its connection's first head byte (`ConnIo`). A slow head therefore consumes the budget: a request whose head took 3 s has 2 s left for the lookup, the body, and the store. The step-4 lookup (`wait_until = deadline`, awaited until the same cutoff), `body_within`, and a write's `not_after` all use the one `Deadline`. `RequestStart`, every `Deadline`, and every cutoff check are read on the server's clock; timers and the store's waits run on real time, converted by the clock itself (`MonotonicClock::real`, a function of its state, never a second reading): the system clock's instants are real, so production deadlines are not converted at all, and a test clock that is `Instant` time plus an offset converts by subtracting it. An injected clock moves the cutoffs and nothing else, and a thread preempted while converting cannot move a deadline. hyper's `header_read_timeout` is a separate parser bound: hyper arms it when it starts reading a head, before any byte arrives (H2), and it is not the application deadline.

**The body boundary.** No application code polls, decodes, or uses a body byte before steps 1–8 accept the request; every rejection in them is a response with no store write, no event, no ticket or pair code consumed, and no handler call. The transport may already hold body bytes (hyper reads ahead, H12; the client may send them), so the claim is about application use, not about bytes on the wire. Every rejection decided before the body carries `Connection: close`. A Windows client that keeps sending a large body after such a rejection may see a reset instead of the status (H5); PR 7 claims the decided status only for small in-flight bodies, and PR 9 measures the large-body case with B-8.

### 2.3 Store Access: `ReadCtx`, `WriteCtx`, `run_write`

```rust
pub struct ReadCtx { principal: Principal, deadline: Deadline, store: Store }
impl ReadCtx {
    /// One `Store::read` in `spawn_blocking` with `wait_until = deadline`; `verify_hash_in` runs first in the
    /// same snapshot as everything `f` reads. The read mount's `timeout_at(deadline)` is its one cutoff.
    pub async fn read<R: Send + 'static>(
        &self,
        f: impl FnOnce(&ReadTx<'_>, &Authorized) -> Result<R, ApiError> + Send + 'static,
    ) -> Result<R, ApiError>;
}
pub struct WriteCtx { principal: Option<Principal>, deadline: Deadline, app: Arc<AppState> }
impl WriteCtx {
    pub async fn body_within(&self, body: Body, max: u64) -> Result<Bytes, ApiError>;   // 408 at the deadline, 413 over max
    /// One non-command write through `observe_write`, its result awaited until `deadline + write_reply_grace`.
    pub async fn write<R: Send + 'static>(&self, job: impl FnOnce(&mut WriteTx<'_>, Option<&Authorized>) -> Result<R, JobError> + Send + 'static)
        -> Result<R, ApiError>;     // no reply by then → 503 `outcome_unknown` (never `in_progress`: no receipt)
}
/// The one submit-and-observe path: submits, owns and awaits the ticket (the writer answers every accepted
/// job, store §5), reports a fatal outcome to `LifecycleSink`, and runs `after_ok` on success (e.g. the
/// revocation nudge). Called by an already registered task: an `observe_write` observer or a `CommandRunner`.
pub(crate) async fn run_write<R: Send + 'static>(app: &AppState, not_after: Instant, job: Job<R>, after_ok: Option<AfterOk>)
    -> Result<R, WriteFailure>;   // WriteFailure = WriteError | SubmitError
/// For plain writes (`WriteCtx::write`, token revocation): registers one observer task running `run_write`
/// and returns its result on a oneshot. Refused once closing.
pub(crate) fn observe_write<R: Send + 'static>(app: &AppState, not_after: Instant, job: Job<R>, after_ok: Option<AfterOk>)
    -> Result<oneshot::Receiver<Result<R, WriteFailure>>, Closing>;
```

**Typed failures survive the wrappers.** `Store::read`'s closure returns `Result<R, StoreError>`: a `StoreError` from the read itself or from the token lookup maps through §2.9 automatically. `ReadCtx` passes it a closure that returns `Ok(f(tx, &auth))`, and `f` returns `Result<R, ApiError>`, so `f` decides how its own store calls map (normally through §2.9) and an authorization or validation failure stays the `ApiError` `f` chose. `verify_hash_in` returns `AuthFailure::{Invalid, Store(StoreError)}`: `Invalid` (missing, revoked, malformed, agent launch not current) is 401, `Store` maps through §2.9; a failed lookup is never a 401. Write jobs return `JobError::Refused(ApiError)` for refusals and `JobError::Store` for store errors.

**Read outcomes: one cutoff.** Every read (the step-4 lookup, `ReadCtx::read`, a socket's read) is awaited until its deadline and no longer. A store outcome returned before the cutoff maps through §2.9 (`ReadError::Closed` → 503 `shutting_down`, `ReadError::Store(Busy)` → 503 `busy`); a read that has not returned by the cutoff answers 408, whether it was still waiting for a reader or running. A result is accepted only if the clock, read when the result is taken, is before the deadline; otherwise it is discarded and the request answers 408, whether it was a success or a store error. This matters because a reader can return after the cutoff and `timeout_at` polls the result before its timer, so both can be ready when the task resumes (H16); the check is the same for the step-4 lookup, `ReadCtx::read`, the final ticket admission read (§2.4), and a socket's reads (a late result drops the socket). The one exception is reader-pool exhaustion: the pool's own wait ends at the same instant (`wait_until = deadline`), so a reader pool exhausted through the cutoff answers 408 or 503 `busy` depending on which is observed first; both mean "nothing happened, retry", and no claim depends on which. The same rule holds for the other waits at the deadline: a read mount's handler is not invoked once the deadline has passed and its result taken at or after it is 408, and `body_within` answers 408 for a body completed at or after the deadline even when it was ready at once, so no write is submitted after a body missed the cutoff. A read cut off this way has no effect and finishes on its own.

`spawn_blocking` read closures capture the moved `Store`, the principal (token id and hash), parsed parameters, and for blobs an `Arc<Semaphore>` used only through `try_acquire_owned`; nothing that blocks.

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
    pub async fn issue_owner(&self, issuer: Option<[u8; 32]>, deadline: Instant) -> Result<Secret, ApiError>;   // pairing, PR 8
    pub async fn revoke(&self, which: TokenSelector, deadline: Instant) -> Result<u64, ApiError>;  // observe_write; nudges on commit
}
```

**`verify_hash_in`** (the one token check; step 4 and every read, write, and socket check run it): `token_by_hash`, constant-time equality, `revoked_at IS NULL`; for `agent`, also: the token's launch is its thread's `current_launch_id` and that launch has no `exit_reason` (mvp §2.5 "one launch" holds at use time even before PR 10 revokes); for `hook`, only revocation (PR 9 adds its launch and binding checks). `rescue` and `owner`: until revoked. `extension`: until revoked; its one-run lifetime is PR 20's revocation after the run. No `token_by_id` is needed: every later check has the hash.

**Revocation nudges come from the observer.** `TokenService::revoke` passes the nudge as `after_ok` to `observe_write`, so it fires from the task that observes the commit, also when the request that asked for the revocation has gone. PR 10 and PR 16 pass the same `after_ok` to `run_write` for their own revoking jobs.

| Credential | Lives in | Restart | Expiry enforced at | Single use decided by |
|---|---|---|---|---|
| Token (`owner`, `agent`, `rescue`, `hook`, `extension`) | SQLite `tokens` (hash only) | survives; mvp §2.9 restart revokes every open launch's tokens (PR 10) | `verify_hash_in` in the transaction that uses it | — (multi-use) |
| WS ticket | memory `TicketStore`: hash → `{ token_id, token_sha256, purpose, expires_at }` | lost; clients mint a new one (they reconnect after a restart anyway, mvp §2.4) | at the conditional removal | `Mutex` + conditional `remove` |
| Pair code | memory `PairCodes`: hash → `{ issuer_sha256, expires_at }` | lost; the user runs `chaldea open` again | at the conditional removal | `Mutex` + conditional `remove` |
| Command receipt | SQLite `command_receipts` | `in_progress` → `unknown_after_crash` (PR 8/10 call `receipts_mark_unknown_after_crash`) | — | the writer's job order (§2.8) |

**Tickets** (`POST /api/ws-ticket`; 7a registers and guards the route and has the in-memory store, the peek, and the redemption admission; its handler (minting), the caps, socket slots, and the re-verification at startup are 7b, §8 D1). Body `WsTicketRequest { purpose: "events" | "attach:<thread_id>" }`; reply `WsTicketResponse { ticket, expires_in: 30 }`. `attach:` requires the `owner` kind and the `Attach` grant (403 otherwise); the thread must exist (404 otherwise). The store holds at most 1024 tickets and 32 per token; expired entries are purged on every insert; over either cap → 503 `busy` with `retry_after` (a client mints one ticket per connect, so 32 is far above honest use).

**Redemption** (`GET /api/ws?ticket=`, `GET /api/attach/{thread}?ticket=`): every fallible check runs first, and the ticket is removed last.

1. Step 4 (credential): **peek** under the mutex (absent, expired at `now ≥ expires_at`, or of another purpose → 401; an `attach:` ticket must name the path's thread), then the **credential read**: `verify_hash_in` on the ticket's token hash (revoked → 401; the lookup failing or cut off → 503, 500, or 408, and the ticket is kept). This read keeps credential errors in their place in the order.
2. Step 5 (kind: `attach` needs an `owner` ticket), steps 6–7 (a `GET` with a body is 400).
3. The upgrade handler validates the handshake (axum's `WebSocketUpgrade` extraction; a rejection is 400 `bad_request`, and so is a `Connection` header carrying a `close` token, since the connection must stay open for the socket), takes a socket slot (`max_sockets`, 16 per token; none free → 503 `busy`), and registers the socket task (closing → 503 `shutting_down`). Until step 5 removes the ticket, the socket task waits on its admission, which ends it by refusing; its hand-over bound (the short deadline) starts only after the removal, so a spent ticket always has a task waiting for its socket.
4. **Final admission read**: `verify_hash_in` on the ticket's token hash again. This snapshot is the authorization decision: a revocation committed before it refuses the redemption with 401 and no `101`; a failed, cut-off, or late lookup (§2.3) answers as in step 1 and keeps the ticket.
5. **Conditional removal** under the mutex: remove the entry only if it is still present and `now < expires_at`, with `now` sampled immediately before the call and the lookup and removal done under the mutex with that sample; if another request removed it first or it expired since the peek → 401. On any refusal in steps 3–5 the slot and the registered task are released.
6. `101`; the socket task receives the socket (§2.6) and re-verifies the token at startup, which covers a revocation committed after the final admission read (the socket then closes with 1008).

The removal decides single use and comes after every fallible check, so a request rejected in steps 1–4 never spends a valid ticket.

Restart safety: a ticket is worth 30 s and one socket; losing it only makes the client mint another with its Bearer token. T3 Code uses signed stateless WebSocket tickets with a 5-minute expiry and a session-revocation check at use, not single use (`auth/SessionStore.ts:493`, `:1049-1064`, `:1113`, `:1138`; its media tickets are reused on purpose, `auth/EnvironmentAuth.ts:1500-1502`); Chaldea's tickets are single use (mvp §2.5), which a stateless signature cannot enforce without a spent list, so they are memory entries.

**Pair codes, bound to their issuer** (7b, §8 D1). `POST /api/pair-codes` (owner Bearer, Origin absent, §8 D12) mints a code, keeps `{ issuer_sha256: the caller's token hash, expires_at: now + 60 s }` under its hash (at most 8 outstanding; purge on insert; full → 503 `busy`), and returns `PairCodeResponse { code, expires_in: 60 }`; `chaldea open` opens `http://127.0.0.1:<port>/#pair=<code>`. `POST /api/pair` (Origin present and matching) takes `PairRequest { code }`:

1. Under the mutex, conditional `remove` by hash (present and `now < expires_at` at the removal), else 401 `unauthorized`. The code is spent from here on.
2. One write through `WriteCtx::write`: `verify_hash_in(issuer_sha256)` (issuer revoked or gone → `Refused(401)`, nothing inserted), then `token_insert` of a new `owner` token. The writer's order decides the race with a revocation of the issuer: a revocation committed first refuses the exchange; one committed after it does not revoke the token just issued (each owner token is revoked on its own).
3. The reply is `PairResponse { token }`. If the job fails, is refused, or its outcome is not known in time, the code stays spent and the reply is the error (401, or 503 `outcome_unknown`; never `in_progress`, as no receipt exists). A token whose reply was lost is a valid owner token nobody holds; it is harmless and revocable.

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
pub fn may_act_on_thread(g: &ThreadGraph, me: Option<ThreadId>, grant: Grant, target: ThreadId) -> bool;  // target: the thread acted on,
                                           // the requested parent (spawn), the recipient (mailbox), the input's creator (input cancel)
pub fn may_remove_worktree(me: Option<ThreadId>, grant: Grant, owner_thread_id: Option<ThreadId>) -> bool;
pub fn surface_in_scope(grant: Grant, configured: Option<&SurfaceScope>, surface: &SurfaceScope, workspace_of: impl Fn(ThreadId) -> Option<WorkspaceId>) -> bool;
```

The table is mvp §2.5's "Actions" table transcribed cell by cell (`owner`: `All`, with `SurfaceScopeContains` for `surface_action`; `agent`: `SelfPlus` for reads, send, stop, resume, close, and the rest as named, `All` for `ReadAdapters` (a static catalog with no host data), `OwnWorktree` for `worktree remove`; `rescue`: `All` for reads and doctor read, `PostEvents` any scope; `extension`: `ExtensionScope` for `PostEvents` only; `hook`: nothing on these actions, its only route is `/hook`). `MVP_COMMANDS` gives every command a fixed action from the start, so the guard decides kind and action for every command name in 7a while every handler is still 501. Listing worktrees (`ReadWorktrees`) stays `SelfPlus` for agents; removing one (`OwnWorktree`) needs the worktree's current owner to be the caller itself (mvp §2.5 "worktrees it owns"; §2.6 ownership passes to the parent only on `thread_closed`).

**Object checks** run on the validated body before any effect, in the same transaction as the effect (mvp §2.5 "Order"): reads in the `ReadCtx` snapshot, commands in the intent job (§2.8). For an update or delete, the handler also checks the stored object's scope through `ObjectScope::stored_surface(tx, stream, surface_id, authorized, configured)` (the surface a `POST /events` replaces must exist and already be in the caller's scope; `configured` is an extension token's scope, which PR 20 stores and passes, `None` until then; PR 20 calls it). **Hiding:** an object the principal cannot read answers 404 `not_found`, identical to a missing one; an object it can read but not act on answers 403 `forbidden`. `deny_for(graph, principal, object)` decides between them after the action grant is checked, so no handler chooses its own. A refusal returns `Refused` from the intent job before the runner's `receipt_begin`, so it leaves no receipt and starts no effect.

**Readable streams:** `owner`, `rescue`: every stream; `agent`: `thread:<t>` for `t ∈ self_plus`; the `host` stream is not self+ and is 404 for agents, who read the thread list and worktrees through the filtered routes (host events cannot be filtered without breaking the gapless `seq` a client pages by). **Per-resource filtering:** `GET /api/threads` returns readable threads (all for `owner` and `rescue`); `GET /api/worktrees` returns all, human-owned included, for `owner` and `rescue`, and those with `owner_thread_id ∈ self_plus` for `agent`; a blob needs a readable stream and a `blob_refs` row; WebSocket subscriptions keep only readable streams. A receipt is visible only to the token that created it (§2.8).

### 2.6 WebSocket: Socket Tasks and the Socket Loop

Ownership (§8 D1): 7a registers the socket task and hands it the admitted socket (§2.4 "Redemption"); the socket loop, its configuration, and its caps are 7b.

```rust
pub struct WsConfig {
    pub heads_interval: Duration,        // 30 s (mvp §2.4); tests 200 ms
    pub send_timeout: Duration,          // 10 s for one frame; past it the socket is dropped
    pub idle_timeout: Duration,          // 75 s with no inbound frame (the browser answers our pings)
    pub max_message_bytes: usize,        // 64 KiB inbound (max_message_size and max_frame_size, H8)
    pub max_write_buffer_bytes: usize,   // 256 KiB (H8)
    pub max_subscriptions: usize,        // 1024 streams per socket
    pub max_sockets: usize,              // 128 total, 16 per token
    pub close_grace: Duration,           // 1 s to send a close frame, best effort
}
```

**Socket ownership.** axum's `on_upgrade` runs its callback in a task it spawns, and hyper's connection task ends at the upgrade (H10). So the upgrade handler registers the socket's own task (holding the socket slot and a `oneshot::Receiver<WebSocket>`) before the ticket is spent (§2.4 step 3), then calls `on_upgrade(|ws| async move { let _ = tx.send(ws); })`: axum's task only hands the socket over and ends. The registered task waits for the socket until the short deadline (a failed upgrade drops the sender and ends it), then runs the loop. Every socket is therefore in the registry from before `101` to its exit, and an abort request drops it. PR 11's attach handler gets the same `UpgradeHandler` seam.

**Startup.** The task takes its `broadcast::Receiver<Committed>` first and the revocation nudge's `watch::Receiver` second (marked seen), then runs the **startup read** (`verify_hash_in`): a revocation committed after the final admission read and before this read fails it (close 1008), and one committed after it fires the nudge, which the socket already watches. Then `hello { epoch, server_time }`.

**State.** The subscription set `S`; `known: BTreeMap<StreamId, u64>` (the newest head this socket knows per stream of `S`); `announced: BTreeMap<StreamId, u64>` (the newest head sent per stream); `heads_due: bool`; at most one read and one send in flight.

**One merge for every source of heads.** Commit notifications and every snapshot read (subscribe, periodic, lag) go through one `merge(entries)`: for each entry with `stream ∈ S`, `known[stream] = max(known[stream], head)`; a snapshot read also sets `heads_due`. **Frames are built at send selection** from the current `known`, never queued: when the sink is free, if `heads_due`, send `heads` with **every** stream of `S` and its `known` head (equal to `announced` allowed), set `announced = known` for `S`, and clear `heads_due`; otherwise send `stream_advanced` for one stream with `known > announced` and set its `announced`. So no `stream_advanced` repeats or lowers an announced head, a buffered old notification merged after a newer snapshot changes nothing, and a newer snapshot is never overtaken by an older pending head.

**Loop.** One `select!` with `biased` order: shutdown signal; send deadline of the in-flight send; read deadline of the in-flight read; idle deadline; nudge; heads time (`sleep_until(next_heads_at)`, an absolute schedule advanced by `heads_interval`, so a slow iteration adds no drift); the in-flight read's completion; the in-flight send's completion; an inbound frame; a commit notification. Reads are `spawn_blocking` handles held across iterations, never awaited inline, so control deadlines fire while a read or a send is in flight. Each read gets the short deadline from its start, the same read cutoff as a request's (§2.3); a read that misses it drops the socket. One read at a time. Heads time marks a refresh due; a nudge or subscribe that arrives during a read is remembered (the latest subscribe replaces an earlier pending one). When the in-flight read ends, a due refresh starts first; it re-verifies the token, so it also serves a pending nudge; a pending subscribe starts after it. A `heads` frame is a snapshot of the latest subscription set; subscribes may coalesce (§8 D15).

| Event | Effect |
|---|---|
| `subscribe { stream_ids }` | parse (malformed ids dropped; more than `max_subscriptions` → close 1008 `too_many_subscriptions`); start a read: token re-verified, readable streams kept, their heads read. When it completes, `S` is **replaced** by the kept streams, `known` and `announced` drop streams no longer in `S`, and the heads merge. The replacement takes effect at that merge: a frame already in flight was built from the previous `S`; every frame selected after the merge names only the new `S`. Subscribe responses may coalesce: when a later subscribe replaces the set before the earlier one's `heads` frame was selected, the client gets one `heads` frame for the latest set only. Unreadable and unknown ids are dropped silently |
| `Committed { heads }` | `merge(heads)` |
| `Lagged(n)` | start a read of the heads of `S` (token re-verified); merge (store §2.2: a lagging receiver resyncs from heads) |
| heads time | mark a refresh due; it starts now if no read is in flight, else first when the in-flight read ends: token re-verified, heads of `S`; merge; a `Ping` goes with the next frame |
| nudge | start a read: token re-verified only |
| a read finds the token invalid | close 1008 `token_revoked`, best effort within `close_grace`, then drop |
| inbound text that is not a `WsClientMessage`, binary, or over size | close 1003 / 1009, best effort |
| `Closed` from the receiver, or the shutdown signal | close 1001, best effort within `close_grace`, then drop |
| send deadline, read deadline, read error, idle deadline | drop the socket |

**Bounded disconnect.** No close frame is waited for beyond `close_grace`; a send or read that misses its deadline drops the socket, and `ConnIo`'s write stall ends the connection under any frame. Close codes are best effort.

**Why a socket converges.** Notifications are published after `COMMIT` (store §2.2), and the receiver exists before the startup read; every snapshot read happens after the receiver exists, so a commit is in a snapshot or arrives as a notification afterwards. A lost notification is repaired by the next periodic `heads` (the periodic frame lists every stream of `S`) and by the subscribe-time heads on reconnect. **What the 30 s covers:** a heads refresh becomes due every `heads_interval`, starts as soon as no read is in flight, and its frame is selected once that read completes (within the read deadline) and the sink is free; delivering the frame is bounded by `send_timeout` and the write stall. A refresh due while a read is in flight waits for that read, and its frame may wait for a send in flight, so a lost notification is recovered within `heads_interval + 2 × read deadline + 2 × send_timeout` (60 s with the defaults) on a socket that stays open. A socket that cannot meet those bounds is dropped, and the client recovers by reconnecting, whose subscribe returns current heads (§8 D15).

**Revocation.** Tickets die with their token at redemption; a new socket re-verifies at startup. An open socket re-verifies on every read (subscribe, heads time, lag) and on the nudge, which fires from the task observing every revoking job that passes `after_ok` (§2.4). The nudge carries no state; each socket decides by reading the database. **Unannounced window (decided, bounded, tested):** a revocation committed without a nudge is applied at the socket's next read, at the latest `heads_interval` plus one read deadline later; during it the socket can send only head numbers of streams the token could read before the revocation, never event content, which every `GET /api/events` re-checks.

**Subscription cap.** PR 18 subscribes to the streams it renders (the host stream and the threads in view), so 1024 is far above use; a client that wants more pulls the rest on demand. A subscribe over the cap is a client bug and closes the socket rather than silently dropping streams; splitting a large set across subscribes is not supported, since each replaces the set.

T3 Code's WebSocket is an Effect RPC transport (`apps/server/src/ws.ts`) with server-pushed streams; it is not used as precedent for the notify-then-pull loop, which follows mvp §2.4.

### 2.7 Sync Read API and Blobs

Ownership (§8 D1): the events, threads, worktrees, and blobs routes are 7b; `/api/adapters` is PR 10 and `/api/doctor` PR 21. In 7a all of them are in `ROUTES` and answer 501 after the guard.

**`GET /api/events`** (mvp §2.4). Query parameters: `stream_id` (required, `StreamId` syntax), `after_seq`, `before_seq` (decimal `u64` up to `MAX_SAFE_INTEGER`), `limit` (decimal `u32`), `epoch` (string). A missing `stream_id`, a malformed or repeated parameter, or an unknown parameter → 400 `bad_query` (before any read). Then one `ReadCtx::read`:

1. `verify_hash_in` (inside `ReadCtx`); the stream must exist (`stream_exists`) and be readable (§2.5), else 404 `not_found` (one body for both).
2. `head(stream, after_seq.unwrap_or(0))`, then `core::sync::decide_replay(&query, head, store.epoch())`: `LimitOutOfRange` and `AfterAndBefore` → 400 `bad_query`.
3. `Forward` → `events_after`, `Backward` → `events_before` → `EventsResponse::Replay { stream_id, epoch, events, head_seq, has_more, has_more_before }`; `Snapshot { reason }` → `thread_snapshot` or `host_snapshot` (agents never reach the host stream) → `EventsResponse::Snapshot { stream_id, epoch, snapshot, as_of_seq, reason }`.

Head, page, snapshot, and `as_of_seq` come from that one read transaction (store §2.2, PR6-CL-18). The snapshot semantics are PR 6's: `as_of_seq` is the requested stream's head in that transaction, and the snapshot's other fields (a thread's `ThreadInfo` in the host snapshot, a worktree in a thread snapshot) come from the same database state, which can change without that stream advancing. The byte bound uses `cum_bytes` through `StreamHead`. The reply always carries the server epoch; a backward page with a mismatched epoch is still a page (`decide_replay`), and the client resyncs forward (mvp §2.4). `before_seq = 1` returns `events: []` with `has_more_before: false`; `before_seq > head_seq` selects the latest page by the store's `seq < before_seq` predicate (a separate matter from the store clamping values above `i64::MAX`). Events of an unknown kind, newer `schema_version`, or invalid payload are returned as stored (store §2.7). The 404 precedes the 400 of step 2 because `decide_replay` takes the stream head (`crates/core/src/sync.rs`, "a 404 decided before this call"). This is T3 Code's `decideThreadResume` (`apps/server/src/orchestration-v2/ThreadStream.ts:100-118`, bounds at `:18`, `:20`) with mvp's bounds and epoch.

**`GET /api/threads`** → `ThreadsResponse { epoch, threads: Vec<ThreadInfo> }` filtered (§2.5). **`GET /api/worktrees`** → `WorktreesResponse { epoch, worktrees: Vec<WorktreeChanged> }` filtered. **`GET /api/adapters`** and **`GET /api/doctor`**: route, guard, and 501 until PR 10 and PR 21.

**`GET /api/blobs/{sha256}?stream_id=`.** `sha256` must be 64 lowercase hex and `stream_id` present and valid (no other parameter), else 400 `bad_query`. One read, in this order in one snapshot:

1. readable stream, else 404;
2. `ReadTx::blob_ref_exists(stream, sha256) -> Result<bool, StoreError>`, a new store read method over `blob_refs` (no schema change); `false` → 404 with the same body, whether or not a slot is free;
3. admission: `try_acquire_owned` on `blob_slots` (non-blocking; none free → 503 `busy` with `retry_after`);
4. `ReadTx::blob(stream, sha256)` loads the bytes and the agreed download type, in the same snapshot, so it finds the reference step 2 found.

Response: 200, `Content-Type` = the store's download type if it is exactly `text/plain; charset=utf-8` or `application/json`, else `application/octet-stream` (mvp §2.5), `Content-Length`, `X-Content-Type-Options: nosniff`, `Content-Disposition: attachment`, `Cache-Control: no-store`; no `Range`, no `ETag`. The permit moves into the response body, which yields the bytes as copied 64 KiB frames from the buffer it owns, so the buffer and the permit are freed together when hyper drops the body (sent, connection ended, write stall, shutdown).

**What admission bounds.** At most `blob_slots` (4) blob bodies, each at most 64 MiB (store §2.5), are held by responses at once, plus hyper's per-connection write buffers. Admission does not bound event pages, snapshots, or lists; their size follows mvp §2.4's limits, and a client that stops reading any reply is cut off by `ConnIo`'s write stall. A streaming blob reader (`rusqlite` `blob` feature) stays a later, schema-free change.

### 2.8 Commands and Receipts

Ownership (§8 D1): 7c, except the command guard (fixed actions, 404 for an unknown name, 501 until the handler merges) and the public envelope type, which are 7a.

**Envelope** (public, §8 D13): `POST /api/commands/{name}` with `CommandRequest { command_id: CommandId, args: <JSON object> }`. The envelope is decoded with `args` kept as `Box<RawValue>`; `args` that is not an object is 400 `bad_request`. The exported schema and TypeScript type declare `args` as an object (`#[schemars(with = "serde_json::Map<String, serde_json::Value>")]`, `#[ts(type = "Record<string, unknown>")]`), since `schemars` would otherwise describe `RawValue` as any JSON value. `name` is one of `MVP_COMMANDS` (snake_case: `spawn`, `resume`, `send`, `ready`, `key`, `stop`, `close`, `input_resolve`, `input_cancel`, `task_set`, `mailbox_send`, `worktree_remove`, `doctor_run`, `surface_action`). Unknown name → 404; a kind without the command's grant → 403; a command whose PR has not merged → 501 `not_implemented`; all three decided by the guard before the body, with no receipt.

**Request identity.** Before any conversion, the raw `args` text is scanned for **duplicate object members at every depth** (a `serde` visitor that walks every map and keeps its keys); a duplicate is 400 `bad_args`, decided before the runner is registered, so no receipt exists. Then the handler's `prepare` deserializes `args` into its own type (which refuses unknown fields and numbers outside a field's type) and returns the normalized value, re-serialized to JSON. Identity = `SHA-256("chaldea-command-v1" ‖ 0x00 ‖ name ‖ 0x00 ‖ canonical(normalized))`, where `canonical` writes object keys sorted by UTF-8 bytes at every level and no whitespace. Two requests with the same meaning share one identity whatever their key order, whitespace, or number spelling (`1.0` is refused by an integer field; for a float field `1` and `1.0` parse to the same `f64`). The order is explicit rather than taken from `serde_json::Map`, which a `preserve_order` feature anywhere in the build graph would change. `command_id` is the receipt key, not part of the hash.

**Dispatch seam.**

```rust
pub trait CommandHandler: Send + Sync + 'static {
    fn name(&self) -> &'static str;                         // must be in MVP_COMMANDS; its action comes from there
    fn deadline(&self) -> Duration;                         // the request budget; 5 s default; long commands state theirs (mvp §2.5)
    /// Parses the duplicate-free `args` into the handler's type (400 `bad_args`); returns the normalized value and
    /// an intent closure built from data only.
    fn prepare(&self, args: &RawValue) -> Result<Prepared, ApiError>;
    /// Required for a handler whose intent can return `Pending`; runs on the runner after the commit.
    fn effect(&self) -> Option<&dyn CommandEffect> { None }
}
pub trait CommandEffect: Send + Sync {
    /// Resolves to the command's terminal result. The owning PR guarantees that it resolves within a bound it
    /// states, and that a result it returns for work that did not happen (a timeout, a refusal) means the work
    /// can no longer happen later. The runner adds no timeout of its own.
    fn run(&self, run: EffectRun) -> BoxFuture<'static, CommandResult>;
}
pub struct Prepared {
    pub normalized: serde_json::Value,
    /// Runs inside the intent job after the runner's `receipt_begin` returned `New`: object authorization first,
    /// then the intent's writes. Never touches receipts.
    pub intent: Box<dyn FnOnce(&mut WriteTx<'_>, &Authorized, &mut CommandCtx) -> Result<Intent, JobError> + Send>,
}
pub enum Intent { Completed(CommandResult), Pending(EffectRun) }
pub struct CommandRegistry { /* name → Box<dyn CommandHandler> or Unimplemented */ }
impl CommandRegistry {
    pub fn mvp() -> CommandRegistry;                        // every MVP_COMMANDS entry, Unimplemented in PR 7
    pub fn register(&mut self, h: Box<dyn CommandHandler>); // PR 10, 16, 20, 21 replace their entries
}
```

Command PRs never call receipt methods: the runner owns `receipt_begin` and `receipt_complete`. `Intent::Pending` from a handler without `effect()` is a programming error: the runner refuses it inside the job, which rolls back everything including the receipt row, and answers 500 `internal`. The intent closure is built by `prepare` from the parsed arguments alone; the handler object (which may hold the supervisor) is used only by `effect` on the runner. That the closure captures no blocking handle is a review rule (store §5), since Rust cannot see what a closure captured.

**`CommandRunner`.** A command's life after the guard belongs to one registered task, which performs its writes by awaiting `run_write` itself; it never registers a second task:

1. The request reads the body (`body_within`; an incomplete body by the deadline is 408 with nothing submitted), runs the duplicate scan and `prepare`, computes the identity, and registers the runner (closing → 503 `shutting_down`, nothing submitted). It receives a `watch::Receiver<Progress>` (`Submitted` → `IntentCommitted` → `Done(reply)`).
2. The runner awaits `run_write` for **the intent job** (`not_after = deadline`; one `BEGIN IMMEDIATE` transaction, the single deciding point): `verify_hash_in` (invalid → `Refused(401)`); `receipt_begin(command_id, token_id, identity)`, which looks the receipt up and inserts `in_progress` only for `New`, and anything but `New` returns that lookup with nothing written; `prepared.intent(…)` (object authorization, then the intent's writes; `Refused` rolls back the whole job, the receipt row included); for `Intent::Completed(r)`, `receipt_complete(command_id, &r)`. `run_write` owns the ticket and awaits it without the request deadline, and reports a fatal outcome to `LifecycleSink::store_fatal`.
3. On `Ok(Pending(run))` the runner publishes `IntentCommitted`, starts the effect **once** (`effect.run(run)`, awaited to its terminal result with no timeout of the runner's own; the bound is the owning PR's guarantee), then **completes the receipt**: attempts of `receipt_complete(command_id, &r)` through `run_write` with `not_after = min(now + 5 s, budget_end)`, retried on `Expired`, `Busy`, `QueueFull`, and `RolledBack(Busy)` with backoff clipped to the remainder. `budget_end` is 60 s after the first attempt and bounds the **admission** of attempts only: an admitted attempt is awaited to its reply. No token check: the effect has run, and a caller revoked meanwhile (possibly by the command itself) does not undo it. The runner publishes `Done(r)` only after that job commits.
4. If completion cannot commit (a terminal store outcome, or no attempt admitted within the budget), the runner reports `StoreFatal::ReceiptUncompleted { command_id }` (`run_write` has already reported a fatal store kind, if any); PR 8 stops the daemon, and restart turns the `in_progress` receipt into `unknown_after_crash`. The effect is never run again, and a waiting request gets 503 `outcome_unknown`.
5. Every other outcome publishes `Done(mapped reply)` at once.

`closing` stops the admission of new requests and new runners only. An admitted runner keeps running after `closing`: its effect, its completion, and its writes go through the store, which is still open until PR 8 shuts it down after `ServeHandle::shutdown` returns. A runner still running at the drain deadline gets an abort request. If no completion job was submitted, its receipt stays `in_progress` and becomes `unknown_after_crash` at restart; a completion job already submitted may still commit, and the retry after restart then replays its result (CL-20).

The request waits on the `watch` until `deadline + write_reply_grace` and replies:

| Progress at reply time | Reply |
|---|---|
| `Done(Lookup::InProgress)` | 409 `in_progress`, `retry_after` = 1 s |
| `Done(Lookup::Replay(r))` | `r.http_status` and `r.body` byte for byte |
| `Done(Lookup::Conflict)` | 409 `conflict`: one body whatever the receipt's state |
| `Done(Lookup::UnknownAfterCrash)` | 409 `unknown_after_crash` |
| `Done(Completed(r))` (in the intent job, or by the runner after the effect) | `r` |
| `Done(Expired)` | 408 `request_timeout`: this attempt's intent job did not run before its deadline (store §2.2 steps 1, 3) |
| `Done(Refused(e))` | `e` (401, 403, 404, or the command's own refusal such as 409 `card_closed`) |
| `Done(Busy)`, `QueueFull`, `RolledBack(Busy)` | 503 `busy`, `retry_after` |
| `Done(RolledBack(DiskFull))` | 503 `disk_full` |
| `Done(RolledBack(_))`, `Rejected(Store(_))`, `Panicked` | 500 `internal` |
| `Done(SubmitError::Closed)` | 503 `shutting_down` |
| `Done(SubmitError::Failed)`, `Done(Failed(_))` | 503 `store_failed` |
| `Done(CommitUnknown)`, `Done(WriterGone)`, completion failed | 503 `outcome_unknown` |
| `IntentCommitted` (effect or completion still running) | 409 `in_progress`, `retry_after`: the receipt exists and is `in_progress` |
| `Submitted` (the writer has not replied) | 503 `outcome_unknown`, `retry_after` (§8 D14) |

**Why 408 is exact.** Before submission, 408 means the request itself missed its deadline (an incomplete body): nothing was submitted. After submission, 408 is answered only when an `Expired` reply has been observed by reply time. The writer runs a job only if its deadline had not passed once it held the lock (store §2.2 step 3), and `Expired` is its only "deadline prevented the commit" reply; no layer drops a write handler (§2.2), and nothing after `IntentCommitted` maps to 408. The writer can reply `Expired` only after the deadline (H15), so a submitted write waits `write_reply_grace` past it for the reply. The grace only extends observing the reply; it never extends admission, since the job's `not_after` stays the deadline. An idle writer's `Expired` arrives within the grace; a writer still busy with other jobs gives no reply by then, and the request answers 503 `outcome_unknown` (the job may be queued, running, or committed with its result not yet observed; a retry with the same `command_id` is correct in every case). A 408 describes **this attempt** only: an earlier attempt with the same `command_id` may have its own receipt, which the retry finds.

**Concurrent duplicates.** Two requests with one `command_id` reach the writer as two jobs; the second runs after the first committed and reads its receipt (`InProgress`, `Replay`, or `Conflict`), so only one intent is ever written; the receipts primary key is the backstop for a second process (store §2.3). A synchronous command's retry while its effect runs gets 409 `in_progress`; after completion, `Replay`.

**Receipt visibility.** `check_receipt` returns `Conflict` for another token before looking at status (`crates/core/src/command.rs`), and `Conflict` has one body, so another token learns nothing about the command's state or result. It does learn that the `command_id` is taken (the same request with a fresh id would proceed); `command_id`s are random UUIDs chosen by their sender, so this reveals only that someone guessed an id in use. T3 Code's receipts (`persistence/Migrations/055_OrchestrationV2.ts:42-50`) key by `command_id` with no token or request hash; Chaldea adds both (mvp §2.4).

**What PR 7 completes end to end.** No mvp command: each needs its owning PR's state machine (PR 10: `spawn`, `resume`, `send`, `ready`, `key`, `stop`, `close`, `input_resolve`, `input_cancel`; PR 16: `task_set`, `mailbox_send`, `worktree_remove`; PR 20: `surface_action`; PR 21: `doctor_run`). PR 7's tests register test-only handlers under test-only names through a test registry and a test `CommandSpec` table: `test_counter` (an intent counter), `test_completed` (completed in the intent job), `test_slow_effect` (a `Pending` intent whose effect resolves only when the test releases its barrier), `test_refuse` (a refusing intent), and `test_worktree_remove` (an `OwnWorktree` check).

### 2.9 Status Mapping and the Error-Body Boundary

**Application decisions** (the outer layer, the guard, the router fallbacks, handlers, and the socket admission) answer `ErrorBody { code, retry_after? }` as `application/json`; `retry_after` is omitted when absent (never `null`) and is whole seconds; codes are snake_case. Codes: `misdirected_request` (421), `origin_rejected` (403), `unauthorized` (401), `forbidden` (403), `not_found` (404), `method_not_allowed` (405, with `Allow`), `request_timeout` (408), `length_required` (411), `payload_too_large` (413), `bad_request`, `bad_query`, `bad_args` (400), `in_progress`, `conflict`, `unknown_after_crash` (409), `not_implemented` (501), `busy`, `disk_full`, `shutting_down`, `store_failed`, `outcome_unknown` (503), `internal` (500). Command handlers may add refusal codes of their own PRs (e.g. `card_closed`, mvp §2.10). A `HEAD` request gets the status and headers of its response with no body, whatever the status: its 405, the 421 decided before routing, and the router fallback's 404 (H11; mvp §2.5).

**Caching.** Every response the daemon generates, success and error alike (handlers, credential and list bodies such as `GET /api/threads`, the guard's and the fallbacks' rejections, and the empty `HEAD` 405), carries `Cache-Control: no-store`: the guard sets it on every response of a route other than the PWA's, replacing a handler's value, and the outer layer adds it to every other response that has none (the fallback, Host rejections). Only the PWA's routes (`/`, `/assets/*`) keep a handler's own caching; their CSP and caching are PR 18's mount contract. hyper's own parser responses (its 400 for `Content-Length: +5`, its 431) are not the daemon's and carry no daemon header.

**Not application decisions**, and so not `ErrorBody`: hyper's own responses before the service runs (400 for a malformed head or `Content-Length`, 431 for an oversized head, EOF for a slow head; H2, H3), and a connection ended by the write stall or by shutdown. Extractor rejections are mapped explicitly, never left to axum's defaults: a `Path` rejection → 404 `not_found`, a `WebSocketUpgrade` rejection → 400 `bad_request`, a query that does not parse → 400 `bad_query`; JSON bodies are parsed by the handler (axum's `json` feature is off).

**Reads** (one cutoff, §2.3): taken before the cutoff, `ReadError::Busy` and `ReadError::Store(Busy)` → 503 `busy`, `ReadError::Closed` → 503 `shutting_down`, any other `StoreError` → 500 `internal`; not returned by the cutoff, or any other result taken at or after it → 408 (no effect). **Writes:** an incomplete body by the deadline, or a write requested at or after it → 408 before anything is submitted; after submission, §2.8's table; a write without a receipt (`POST /api/pair`, revocation) never answers `in_progress`, and answers 503 `outcome_unknown` when its result is not known by `deadline + write_reply_grace` or is taken at or after it (the observer still sees the job through).

### 2.10 Secrets in Logs

- **Span:** `OuterLayer` creates one span per request, outermost, so 421s and 404s are spanned too, with `method` (a fixed label: the standard methods by name, any extension method as `other`, since it is client text), `route = Empty`, `status`, `latency`, and `token_id = Empty`. The guard records `route` as the matched template (e.g. `/api/blobs/{sha256}`), the fallbacks and the Host rejection record `unmatched`, and step 4 records `token_id`; each request that produces a response has exactly one completion record (a request whose future is dropped after parsing, by a client disconnect or a shutdown abort, has none). Never the URI, the query, header values, or bodies (H7 is why `TraceLayer` is not used).
- **Headers:** `SetSensitiveRequestHeadersLayer` marks `Authorization`, `Cookie`, and `Sec-WebSocket-Key`, so a `Debug` of the headers prints `Sensitive`.
- **Types:** `Secret`, `MintedToken`, `PairRequest`, `PairResponse`, `PairCodeResponse`, `WsTicketResponse`, `CommandRequest` (its `args` can hold prompt text) implement `Debug` with the secret or the arguments replaced; no error type carries a secret or a body.
- **Bodies:** no layer or handler logs a request or response body; `/hook` and `/api/pair` bodies are never in a log (mvp §2.5). This is structural: arbitrary body text cannot be recognised by a pattern, so the guarantee is that no code path formats a body.
- **Scrubber:** `trace::redact_secrets` replaces `chd_`, `chdt_`, and `chdp_` secrets in a string; PR 8's log formatter applies it to every message and field.
- **Panics (hand-off to PR 8):** a task that panics ends, and Rust's panic hook writes outside `tracing`. PR 8's process panic hook writes the location and the task kind but **omits the panic payload**, since a payload can hold arbitrary body text that no pattern recognises; PR 8 verifies this on the actual stderr of a daemon whose handler panics with a body marker. PR 7's claim covers what reaches `tracing`.

### 2.11 What Later PRs Plug In

| Seam | Type | Used by |
|---|---|---|
| Listener and lifecycle | `HttpServer::new`, `serve(TcpListener)`, `ServeHandle::shutdown(deadline)`, `LifecycleSink` | PR 8 |
| Token issuance and revocation | `TokenService::{mint, new_token, issue_owner, revoke}`, `nudge_revocation`, the `after_ok` of `run_write` / `observe_write` | PR 8 (owner token file), PR 10 (launch tokens), PR 16, PR 20 |
| Commands | `CommandHandler`, `CommandEffect` (with its bounded-result obligation), `CommandRegistry::register`, `Prepared`, `Authorized`, `deny_for`, `ObjectScope` | PR 10, 16, 20, 21 |
| Hook route | `Mount::write` for `POST /hook` (spec in `ROUTES`), `WriteCtx` | PR 9 |
| Attach | `Mount::upgrade` for `GET /api/attach/{thread}`; the handler gets `AttachGrant { thread, token_id, token_sha256 }` and runs in a registered socket task | PR 11 |
| Read routes | `Mount::read` for `/api/adapters`, `/api/doctor`, `/`, `/assets/*` with `ReadCtx` | PR 10, 21, 18 |
| `POST /events` | `Mount::write`, `ObjectScope::stored_surface` | PR 20 |

### 2.12 Single Deciding Points

| Operation several requests can hit at once | Decided by | Why a second caller cannot act |
|---|---|---|
| Registration racing shutdown | `TaskRegistry`'s mutex: `closing` set in the same critical section as the snapshot; `spawn` checks it under the lock | a task registered before the transition is in the snapshot; after it, registration fails and no work starts |
| Duplicate `command_id` | the intent job: token check, receipt lookup, authorization, intent, `receipt_begin` in one `BEGIN IMMEDIATE` transaction on the one writer | jobs run one at a time; the second reads the first's receipt; the primary key refuses a second process's insert (store §2.3) |
| A committed intent's effect and completion | its registered `CommandRunner` | only the runner starts the effect, once, and completes the receipt; the request only watches |
| Any write's outcome and fatal report | the registered task awaiting `run_write` (a plain write's observer or the command's runner) | the ticket has one owner; no request future holds it |
| Token revocation racing a request | reads: the request's one snapshot runs `verify_hash_in` and the data reads; writes: the job runs it first | a revocation commits before or after the snapshot or job in the writer's order; no cached token state exists |
| Ticket redemption | validity: the final admission read; single use: `TicketStore`'s mutex, conditional `remove` after every fallible check | a revocation commits before or after the final read in the writer's order; only the caller whose removal returned the entry has it; rejected attempts peek only |
| Pair-code exchange | `PairCodes`' mutex: conditional `remove`; the issuer check and the insert in one job | as above; the writer orders the insert against a revocation of the issuer |
| Ticket and pair-code caps | the same mutexes | counted and inserted under one lock |
| WS subscribe racing a commit notification | the socket's sequential loop; one `merge` into `known`; frames built at send selection | a commit is in a later snapshot or merged from its notification; frames never lower an announced head |
| Revocation racing an open socket | each socket's own read (`verify_hash_in`) at startup, on nudge, tick, subscribe, or resync | the nudge carries no decision; the database does |
| Socket, connection, and blob caps | `Semaphore`s acquired before registering the socket, before `accept`, after `blob_ref_exists` | permits are atomic |

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **Serving loop** | A. `axum::serve` | axum `src/serve/mod.rs:388-396` | Rejected: no timer, so no header read timeout (H1), and no connection bound. |
| | B. Own accept loop on hyper's HTTP/1 builder with `TokioTimer`, `with_upgrades`, `ConnIo`, and axum's accept-error rule | hyper `server/conn/http1.rs` (H2); axum `serve/listener.rs:140-165` (H13) | **Adopted.** |
| **Protocol** | A. HTTP/1.1 and HTTP/2 (auto) | axum's default build | Rejected: a second framing to test for B-8; the clients are local HTTP/1.1. |
| | B. HTTP/1.1, with HTTP/1.0 requests under the same rules once parsed (no upgrade; hyper answers an HTTP/1.0 request carrying `Transfer-Encoding` with its own 400 before the `Host` check; §8 D2) | — | **Adopted.** Adding HTTP/2 later is additive. |
| **Task ownership** | A. A `Mutex<JoinSet>` reaped by the accept loop | — | Rejected: awaiting `join_next` under the lock blocks every spawn, and a spawn could land after the shutdown snapshot. |
| | B. A coordinator task owning a `JoinSet`, fed by a channel | — | Rejected: a send can race the coordinator's shutdown just as a spawn can; the closing decision still needs one lock. |
| | C. `TaskRegistry`: a `std::sync::Mutex` over `{ closing, id → (kind, AbortHandle) }`, never held across `.await`; insert of an `AbortHandle` under the lock before the spawn, the spawned `Abortable` task owning its exit guard; each task removes itself; `closing` set in the snapshot's critical section; the accept loop is one of its tasks | PR 6's single shutdown owner (store §5) | **Adopted.** |
| **Fatal outcome of a dropped observer** | A. Keep observers alive past the deadline | — | Rejected: the deadline is PR 8's one shutdown budget. |
| | B. Rely on a store-side report | — | Not available: `ShutdownReport` carries no job outcome (H14). |
| | C. No report: the daemon is already stopping; the outcome is all or nothing in the database; PR 8 holds the instance lock until writer exit and reconciles receipts before serving | store §2.2, commitment 2 | **Adopted.** |
| **Deadline origin** | A. Service start (head complete), with `/hook` optionally from connection start | — | Rejected: a slow head extends the budget, and `/hook` had two origins. |
| | B. The first byte of the head, for every route, from `ConnIo`, with keep-alive kept on and a pipelined request starting at the previous response's end | — | Rejected: a partly buffered head of a later request gets an origin later than its first byte, so its application work can exceed the budget. |
| | C. One request per connection through hyper's `keep_alive(false)` | hyper `http1::Builder::keep_alive` (H2) | Rejected: the `101` would carry `Connection: upgrade, close`, which the pinned client and WebKit reject (H17). |
| | D. One request per connection through `Connection: close` on every final response except a successful `101`; origin is the connection's first head byte | hyper `role.rs:847-862` (H2) | **Adopted**: no second parser; costs one loopback connect per request. |
| **Where checks live** | A. Per-handler extractors | — | Rejected: a new route could forget one. |
| | B. Global `Router::layer`s that look up the route's rules | — | Rejected: rules separated from their route. |
| | C. An outer layer (span, start, Host) and one `GuardLayer` per route from its `RouteSpec`; the router built only from the table; an independent inventory equals it and tests the real router | — | **Adopted.** |
| **Write timeout** | A. `TimeoutLayer` on every route, writes marked by a flag | H6 | Rejected: a wrong flag answers 408 while a job commits. |
| | B. The mount kind in the spec and typed constructors: only `Mount::read` gets a dropping timeout; only `Mount::write` can obtain `WriteCtx` | store §2.2 step 3 | **Adopted.** |
| **Write observation** | A. A dispatcher for plain writes and a separate runner path for commands | — | Rejected: two places to get ownership and fatal reporting right. |
| | B. One async `run_write`; a plain write wraps it in one registered observer, and the registered runner awaits it directly | — | **Adopted**: the runner never needs a second registration, so `closing` cannot strand an admitted command. |
| **Receipt calls** | A. Each command's intent closure calls `receipt_begin` / `receipt_complete` | — | Rejected: 14 places to get it right. |
| | B. The runner calls both; intents never touch receipts | — | **Adopted.** |
| **Effect bound** | A. The runner times the effect out and stores a timeout result | — | Rejected: the effect's work can still happen after the stored result says it did not. |
| | B. The owning PR's `CommandEffect` returns a bounded terminal result and guarantees that a negative result means the work cannot happen later | mvp §2.4 "Completion" | **Adopted.** |
| **Reply before the outcome is known** | A. 409 `in_progress` | mvp §2.4 (for an existing receipt) | Rejected: a receipt may never exist. |
| | B. Wait for the writer's reply however long | store §5 "Waits" | Rejected: no request deadline. |
| | C. 503 `outcome_unknown` with `retry_after` after `write_reply_grace` | — | **Adopted**, §8 D14. |
| **Socket ownership** | A. axum's `on_upgrade` task runs the loop | axum `ws.rs:350-365` (H10) | Rejected: shutdown cannot see or abort it. |
| | B. A registered task, created before `101`, receives the socket from the `on_upgrade` callback | — | **Adopted.** |
| **Notification fan-out** | A. One hub task fanning out to per-socket channels | — | Rejected: a second queue per socket with its own lag rule. |
| | B. Each socket subscribes to the store's broadcast itself | store §2.2 | **Adopted.** |
| **Socket frames** | A. Queue `stream_advanced` and `heads` frames as events arrive | — | Rejected: a queued older head can overtake a newer snapshot. |
| | B. Merge every source into `known`; build each frame at send selection | — | **Adopted.** |
| **WS library** | A. axum `ws` (tokio-tungstenite 0.29 underneath) | axum `src/extract/ws.rs` | **Adopted**: upgrade handling, per-socket limits, automatic pongs (H8). |
| | B. `tokio-tungstenite` directly with a hand-written upgrade | — | Rejected: duplicates the handshake; kept as the test client. |
| **Token check** | A. A memory cache of valid tokens | — | Rejected: cross-call state the database does not hold. |
| | B. Look up by hash in the transaction that uses it; principals and tickets carry the hash | mvp §2.5 | **Adopted**; no plaintext kept past its request. |
| **Ticket form** | A. Signed stateless ticket | T3 Code `auth/SessionStore.ts:1049-1064`, `:1113` | Rejected: cannot be single use without a spent list. |
| | B. Random ticket in a memory map; peek and token check at the credential step, conditional `remove` after every fallible check | — | **Adopted**; a rejected upgrade does not spend it. |
| | C. Tickets in SQLite | — | Rejected: a write per WebSocket connect for a 30 s credential. |
| **Pair-code authority** | A. Expiry only | — | Rejected: a code minted by a since-revoked owner would still confer owner access. |
| | B. Bound to the issuer's token hash, checked in the token-insert job | — | **Adopted** (with §8 D12). |
| **Pair code issuance** | A. A dedicated owner-only route (`POST /api/pair-codes`, Origin absent) | T3 Code pairing links (`Migrations/031_AuthAuthorizationScopes.ts:11-24`) | **Adopted**, §8 D12. |
| | B. A command `open` | — | Rejected: the receipt stores the reply body, i.e. the code. |
| | C. A ticket purpose `pair` | — | Rejected: browsers could mint, and the ticket route's lifetime and Origin rule differ. |
| **Host stream for agents** | A. Serve it filtered | — | Rejected: filtered events break the gapless `seq`. |
| | B. 404; agents use `/api/threads` and `/api/worktrees` | — | **Adopted.** |
| **Reply memory** | A. Admission for every reply over 1 MiB | — | Rejected: snapshots and lists cannot be measured before loading, and a missing blob reference could answer 503 before 404. |
| | B. Admission for blobs only, after `blob_ref_exists` in the same snapshot; the write stall for every reply | store §2.5 | **Adopted.** |
| **Command handler shape** | A. One HTTP handler per command | — | Rejected: receipt, deadline, runner, and mapping repeated 14 times. |
| | B. `CommandHandler` with a data-only intent closure and an optional bounded `CommandEffect` | — | **Adopted.** |
| **Request identity** | A. Bytes as received | — | Rejected: two serializations of one request conflict. |
| | B. Canonical form of a parsed `serde_json::Value` | — | Rejected: duplicate keys collapse before they can be refused. |
| | C. Duplicate scan on the raw text, then the canonical form of the handler's typed value | — | **Adopted.** |
| **PR size** | A. One PR | mvp §4 | Rejected: large and on the longest path. |
| | B. Two PRs (server and reads; commands) | — | Rejected (§8 D1). |
| | C. Three PRs: 7a, 7b, 7c | — | **Adopted** (§8 D1). |

## 4. Same-Class Inventory

Every later PR that adds a route, a command, a token use, or a WebSocket purpose, and the seam it uses (§2.11). None needs a change to PR 7's types.

| PR | Adds | Seam | What PR 7 provides |
|---|---|---|---|
| 8 lifecycle | starts and stops the server; owner token file; fatal store outcomes; panic hook | `HttpServer::new`, `serve`, `ServeHandle::shutdown`, `LifecycleSink`, `TokenService::issue_owner`, `trace::redact_secrets` | Startup order (PR 8): open store → `receipts_mark_unknown_after_crash` and launch recovery (PR 10's) → `serve`. Shutdown order: `ServeHandle::shutdown(budget)` first, then `Store::shutdown` with the same budget. PR 8 stops the daemon on `store_fatal` (including `ReceiptUncompleted`); keeps the single-instance lock until it observes the writer's exit (or the process exits) and reconciles receipts before serving again (§2.2); and installs a panic hook that omits the payload, verified on stderr with a body marker (§2.10). |
| 9 hooks | `POST /hook` handler; hook token use | `Mount::write` (spec already in `ROUTES`), `WriteCtx` | Host, `Origin` must be absent, `hook` kind only, framing, size, and the deadline from the connection's first head byte before the handler (one request per connection, so every hook delivery is its own connection); `verify_hash_in` checks revocation only; hook bodies never logged. B-8 cases on both OSes, including H5's large-body case, are PR 9's. |
| 10 supervisor | `spawn`, `resume`, `send`, `ready`, `key`, `stop`, `close`, `input_resolve`, `input_cancel`; agent and hook tokens; `GET /api/adapters` | `CommandHandler`, `CommandEffect` (a bounded terminal result; a negative result means the work cannot happen later), `TokenService::{mint, new_token, revoke}`, the `after_ok` nudge, `Mount::read` | Guard (kind and action per command name), receipt, identity, runner, mapping, object-scope functions, `deny_for` |
| 11 attach | `GET /api/attach/{thread}` socket bytes | `Mount::upgrade`, `AttachGrant` | ticket purpose `attach:<thread>`, owner-only at issue and redeem, single use, a registered socket task |
| 16 agent control | `task_set`, `mailbox_send`, `worktree_remove` | `CommandHandler` | `SelfAndDirectChildren`, `ParentAndChildren`, `OwnWorktree`, `CreatedBySelf` grants |
| 17 `stateright` | the Sync model | — | the replay rules (§2.7) from 7b and the receipt and runner rules (§2.8) from 7c, cited by claim id |
| 18 PWA | `/`, `/assets/*`; pairing; ticket and events client | `Mount::read` | pair flow, `ws-ticket`, `/api/ws`, `/api/events`, the public bodies (§8 D13), the subscription rule (§2.6). Requirements on PR 18: a reducer that treats every `heads` frame as a snapshot of the latest subscribed set (§8 D15), and no reliance on connection reuse (§2.2) |
| 20 extensions | `POST /events`, `surface_action`, extension tokens with a scope, rescue | `Mount::write`, `CommandHandler`, `TokenService` | kind check, `ExtensionScope`/`SurfaceScopeContains` grants, `ObjectScope::stored_surface`; the scope column is a v2 migration (store §2.4) |
| 21 doctor | `doctor_run`, `GET /api/doctor` | `CommandHandler`, `Mount::read` | owner and rescue read, owner run |

## 5. Resource Paths

| Resource | Created by | Bound | Ends on |
|---|---|---|---|
| Listener | PR 8 (`serve` takes it) | one | `ServeHandle::shutdown`: the accept loop stops at the signal and drops it |
| Connection task | accept loop, registered before serving | `max_connections` (128) permits, acquired before `accept`; one request each (`Connection: close`) | its response sent (hyper closes it); client close; `header_read_timeout` (EOF); write stall; graceful shutdown at the signal; abort requested at the shutdown deadline |
| Request handler | hyper per request | one per connection | response; read mounts at their deadline (408); write mounts at their deadline before submission, at `deadline + write_reply_grace` after it; connection end |
| `spawn_blocking` read | `ReadCtx::read`, the guard's lookup, socket reads | the store's reader pool (4) and `wait_until` | `f` returns (not bounded by the store, store §5); the waiter stops at the deadline; at daemon shutdown the store abandons and reports it |
| Plain write observer | `observe_write`, registered before it submits | one per plain write | the writer's reply (store §5: every accepted job is answered); abort requested at the shutdown deadline |
| Command runner | the command request, registered before it submits | one per command | its last write committed or failed; abort requested at the shutdown deadline (an `in_progress` receipt becomes `unknown_after_crash` at restart) |
| Effect future | `CommandRunner` | one per synchronous command | its terminal result, within the bound its owning PR guarantees; dropped with the runner at the shutdown deadline (the receipt stays `in_progress` → `unknown_after_crash` at restart) |
| Receipt completion | `CommandRunner` | attempts of ≤ 5 s admitted within 60 s; an admitted attempt is awaited to its reply | committed; no attempt admitted within the budget, or a terminal outcome → `ReceiptUncompleted` to `LifecycleSink` |
| WebSocket socket task | upgrade handler, registered before the ticket is spent and before `101` | `max_sockets` (128), 16 per token | client close; send deadline (10 s); idle (75 s); token invalid (1008); `Closed` from the receiver or the shutdown signal (1001, best effort within `close_grace`); abort requested at the shutdown deadline |
| Commit notification receiver | each socket at start | the store's broadcast (1024) | dropped with its socket; `Lagged` → heads resync; `Closed` → close |
| Tickets | `POST /api/ws-ticket` | 1024 total, 32 per token, 30 s | redemption, expiry purge on insert, daemon exit |
| Pair codes | `POST /api/pair-codes` | 8, 60 s | redemption, purge on insert, daemon exit |
| Blob body and permit | `GET /api/blobs` | `blob_slots` (4), ≤ 64 MiB each | body dropped by hyper (sent, connection ended, write stall, shutdown) |

**Endings.**

| Ending | What happens |
|---|---|
| Daemon shutdown (PR 8) | `ServeHandle::shutdown(deadline)`: the registry closes and is snapshotted in one transition; the accept loop stops and drops the listener; connections get `graceful_shutdown`; sockets send 1001 best effort; admitted runners and observers continue; the registry is awaited until `deadline`; the rest get an abort request and are counted under `abort_requested`. A dropped observer's or runner's job is still answered by the writer, and nobody reports its outcome (§2.2 "Fatal outcomes of a dropped observer"); if its intent committed, restart reconciles the receipt. PR 8 then shuts the store down with the same budget. |
| Registration refused (closing) | 503 `shutting_down`; nothing submitted, no ticket spent, no socket admitted. |
| Store `Closed` | `submit` → 503 `shutting_down`; `read` → 503 `shutting_down`; sockets get `Closed` and close 1001. |
| Store failed, `CommitUnknown`, `WriterGone` while serving | the task awaiting `run_write` reports to `LifecycleSink`; the reply (if anyone waits) is 503 `store_failed` / `outcome_unknown`; reads continue; PR 8 stops the daemon. |
| Receipt completion fails | `ReceiptUncompleted` to `LifecycleSink`; the effect is not rerun; restart makes the receipt `unknown_after_crash`. |
| Lagged notifications | a heads read for the socket's set (§2.6). |
| Client disconnect mid-request | hyper drops the handler future; a submitted write and any effect continue on their registered task; a read finishes on its own. |
| Client stops reading | the write stall ends the connection after 30 s; a socket's send deadline drops it after 10 s. |
| Token revoked | requests: 401 at their next `verify_hash_in`; sockets: close 1008 on the nudge or within `heads_interval` plus a read deadline; outstanding tickets fail at redemption; outstanding pair codes of a revoked issuer fail at exchange. |

**Waits.** Head parsing: hyper's `header_read_timeout`, armed when hyper starts reading a head. Credential lookup, body, read, write admission: the request `Deadline` from the request origin; reads end at it; a submitted write's reply is awaited until `deadline + write_reply_grace`. A ticket wait inside `run_write`: the writer's reply guarantee. Completion: attempt admission within 60 s, each attempt's `not_after` within 5 s. Effect: the bound its owning PR guarantees. WebSocket: each send `send_timeout`; inbound `idle_timeout`; reads the short deadline; close `close_grace`. Writes to a client: `write_stall_timeout` while a write is pending. Registry, ticket, and pair-code mutexes: held only for a flag check, a spawn, or a map operation. Shutdown: its deadline. Unbounded by PR 7: a started `spawn_blocking` read and a started writer job (store §5); PR 7 never makes a request wait on them past its deadline (plus `write_reply_grace` for a submitted write), and shutdown requests abort of the tasks that await them.

**Windows and macOS.** The code has no `cfg`. Loopback behaviour differs only where H5 applies (a rejection while the client still sends a large body); PR 7's tests send small in-flight bodies (which hyper holds in its buffer) or none, and PR 9 measures the large-body case.

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked on a running daemon (PR 8's binary once it exists; until then the test server of `tests/support/http.rs` on `127.0.0.1:<port>`). `$T` is an owner token, `$A` an agent token, `$P` the port.

| ID | Criterion (EARS) | Source | How to observe |
|---|---|---|---|
| PR7-P-1 | If a request has a wrong or missing `Host`, a wrong method, a wrong `Origin`, a missing or wrong token, or a token kind or command the caller may not use, then the daemon shall reject it with 421, 405, 403, 401, or 403 and change nothing. | mvp P-20; §2.5 | `curl -i -H 'Host: evil:$P' http://127.0.0.1:$P/api/threads` → 421; `-X DELETE` → 405; `-H "Authorization: Bearer $T" -H 'Origin: http://evil'` → 403; no token → 401; hook token on `/api/threads` → 403; `$A` on `/api/commands/key` → 403; `GET /api/events?stream_id=host` with `$T` shows the same `head_seq` before and after. |
| PR7-P-2 | When a client replays a stream from `after_seq`, pages backward with `before_seq`, or sends a stale epoch, the daemon shall answer per mvp §2.4 with events, a snapshot, or 400/404. | mvp §2.4; PR 7 obligations | `curl -s -H "Authorization: Bearer $T" "…/api/events?stream_id=host&after_seq=0&limit=0"` → 400; `limit=1001` → 400; `after_seq=999999` → `snapshot`, `cursor_ahead`; `epoch=x` → `epoch_changed`; `after_seq=1&before_seq=5` → 400; unknown thread → 404. |
| PR7-P-3 | While a WebSocket client stays connected and subscribed, the daemon shall send it `stream_advanced` or a `heads` frame with the new head when a stream advances, and a heads refresh shall become due every 30 s, each frame a snapshot of the latest subscribed set; a lost notification shall be recovered within 30 s plus the read and send bounds (60 s with the defaults), or the socket shall be dropped so the client reconnects and gets current heads. | mvp §2.4; CL-13 | `curl -X POST …/api/ws-ticket -d '{"purpose":"events"}'`, then `websocat "ws://127.0.0.1:$P/api/ws?ticket=…"`, send `{"type":"subscribe","stream_ids":["host"]}`: `hello`, `heads`; a new thread → `stream_advanced` or `heads` with the new head; with no commit, another `heads` about 30 s later. |
| PR7-P-4 | When a ticket or pair code is used a second time or after its lifetime, or a pair code's issuer was revoked, the daemon shall refuse it. | mvp §2.5 | Second `websocat` with the same ticket → 401; after 31 s → 401; `POST /api/pair` twice with one code → second 401. |
| PR7-P-5 | When a command is retried with the same `command_id` by the same token, the daemon shall return the stored result and cause no second effect; another token shall get 409 `conflict`. | mvp CL-8; §2.4 | With a registered command (PR 10's first, or the test handler): `curl -X POST …/api/commands/<name> -d '{"command_id":"…","args":{…}}'` twice → equal status and body, one event; with `$A` → 409 `conflict`. |
| PR7-P-6 | When a blob is downloaded, the daemon shall serve its bytes only through a stream that references it, with the safe type and download headers. | mvp §2.5; P-21 | `curl -i "…/api/blobs/<sha>?stream_id=thread:<t>"` → bytes equal, `nosniff`, `attachment`; another stream → 404. |
| PR7-P-7 | While the daemon serves requests carrying tokens, tickets, and pair codes, its request log shall contain the request records and none of the secrets, queries, or `/hook` and `/api/pair` bodies. | mvp §2.5 "Secrets" | Run requests with known secrets; the log has one line per request the daemon produced a response for, with its route template; `grep -c 'chd_\|chdt_\|chdp_'` → 0. |

### 6.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| PR7-CL-1 | If a request's `Host` is missing, repeated, or not `127.0.0.1:<port>` / `localhost:<port>`, or its absolute-form target carries any other authority string (other host, other port, userinfo), then the daemon shall answer 421 before routing, authentication, or any body use, and change nothing. | mvp §2.5; P-20 | `http_layers::host_rejections` (each case on a known and an unknown path, an absolute-form authority other than the `Host` included; store dump unchanged) |
| PR7-CL-2 | When a route checks `Origin`, the daemon shall accept it only per the route's rule (absent or exactly `http://` + the received `Host`; present and matching for `/api/pair`; absent for `/api/pair-codes`, `/hook`, and `/events`), and otherwise answer 403. | mvp §2.5; mvp §4 PR 7 "`Origin` vs received `Host`" | `http_matrix::origin_matrix` (every inventory route × {absent, matching for `127.0.0.1:<port>`, matching for `localhost:<port>` sent with the other `Host`, other port, `null`, `https://`, duplicate}; expectations from the literal inventory) |
| PR7-CL-3 | If a Bearer route gets no `Authorization`, several, another scheme, extra whitespace or characters, a malformed token, an unknown or revoked token, or an agent token whose launch is not current or has exited, then the daemon shall answer 401 with nothing changed; a token lookup that fails or is cut off shall answer 503, 500, or 408, never 401. | mvp §2.5; P-20 | `http_layers::bearer_rejections` (each case; `agent_token_after_launch_exit` inserts `launch_exited` without revoking; an agent token naming another thread's live current launch is refused only by the current-launch comparison); unit `http::tests::reads::lookup_failure_is_not_unauthorized` (reader pool exhausted through the cutoff: 503 or 408; store closed: 503; never 401) |
| PR7-CL-4 | The real router shall serve exactly the routes of the literal inventory (mvp §2.5), each under exactly its method and mount kind, and answer `HEAD` and every other method with 405 and `Allow` before authentication; for every route, every token kind (and none), and every command name, the outcome shall be the inventory's: handler reached, 401, 403, 404, or 501. | CL-15; mvp §4 PR 7 "every route × token kind cell" | `http_matrix::inventory_equals_routes` (the literal inventory in `tests/http_matrix.rs` equals `ROUTES` as a set of method, path, mount, Origin rule, and credential: an extra or missing route fails), `http_matrix::inventory_matches_router` (every inventory row probed against the production router), `http_matrix::methods_are_exact` (`HEAD` on every `GET` route: 405, `Allow`, `Cache-Control: no-store`, empty body; `PUT`, `DELETE` on every route: 405 with `ErrorBody` and `Allow`; all before auth), `http_matrix::route_kind_matrix` (the inventory's Bearer rows × {owner, agent, rescue, hook, extension, none}), `http_matrix::command_matrix` (every `MVP_COMMANDS` name × kinds: agent on `key`, `ready`, `input_resolve`, `doctor_run`, `surface_action` → 403; granted commands → 501 in PR 7; an unknown name → 404), `http_matrix::credential_paths` (ticket routes with a Bearer header of each kind and no ticket → 401; a malformed or repeated ticket → 401, another query parameter → 400 `bad_query`); 7b adds a ticket minted per kind and `/api/pair` with a Bearer header and no code → 401, `http_matrix::build_router_rejects_inconsistent_mounts` |
| PR7-CL-5 | The action table and `MVP_COMMANDS` shall equal mvp §2.5's "Actions" table and §2.4's command list cell by cell, and object scopes shall follow their definitions on any thread tree; an agent may remove only a worktree whose current owner is itself. | CL-15; mvp §2.5, §2.6 | `core::auth::tests::grant_table_matches_mvp`, `…::mvp_commands_match`, proptest `…::self_plus_is_the_descendant_closure`, `…::scope_grants_on_random_trees`, `…::own_worktree_only` (A-owned allowed; B-owned with B a child of A refused; transferred to A after B's close allowed) |
| PR7-CL-6 | When a request is rejected at the Host, method, Origin, credential, kind, action, framing, or size step, the daemon shall answer without the application polling or using the body, in that order. | mvp §2.5 "Order", "Rejections" | unit `http::tests::layers::rejections_precede_body` (head sent, body withheld until the response is read, one case per step; the stub's poll-counting request body records 0 polls), unit `http::tests::layers::rejections_with_body_in_flight` (head and a small body sent together; the decided status arrives; 0 polls), `http_layers::layer_order` (requests failing several steps get the earliest step's status) |
| PR7-CL-7 | Every connection shall serve one request: every final response other than a successful `101` shall carry `Connection: close` and the connection shall close after it, a second request on the same connection (sent with the first or after its response) shall never be dispatched, and a successful `101` shall carry exactly `Connection: Upgrade` (except a `101` racing shutdown, §6.2 "Known limits"). If a POST has `Transfer-Encoding` (with or without `Content-Length`), has no `Content-Length`, or declares more than 4 MiB, or a GET has a nonzero `Content-Length`, then the daemon shall answer 400, 411, 413, or 400; a malformed `Content-Length` shall get hyper's 400 and an oversized head 431 without the service being called; a head not completed within `header_read_timeout` shall end the connection with EOF (except a head completed as that timer expires, §6.2 "Known limits"). | mvp §2.5 rejections; spike B-8; H2–H4 | `http_layers::one_request_per_connection` (two requests in one write, and a second request after the first response: only the first is answered, the dispatch counter is 1 when the first request is accepted and 0 when it is rejected, then EOF, or a reset or abort with the unread second request; error and success responses alike), unit `http::tests::upgrade::upgrade_keeps_connection_upgrade` (the `101` header is exactly `Connection: Upgrade`, and the pinned `tokio-tungstenite` client receives `hello`), unit `http::tests::upgrade::upgrade_asking_close_is_400` (`Connection` with a `close` token: 400 before the ticket is removed; the same ticket redeems afterwards), `http_layers::te_with_content_length_is_400`, `http_layers::framing_and_size`, `http_layers::get_with_body_is_400` (`Content-Length: 0` and `00` reach the handler), `http_layers::http10_gets_the_same_rules`, unit `http::tests::upgrade::http10_upgrade_is_400`, unit `http::tests::upgrade::non_utf8_connection_close_is_400`, unit `http::tests::upgrade::hand_over_bound_starts_after_the_ticket_is_removed` (admission held between the final read and the ticket removal until the socket task has parked: no hand-over bound has started; after the release the `101` delivers `hello`, and the recorded order is the removal, then the bound), `http_layers::parser_layer_cases` (`Content-Length: +5`, two different lengths, 9 KiB of headers, 65 headers; counting stub), `http_layers::slow_head_is_closed` |
| PR7-CL-8 | The request deadline shall start at the connection's first head byte, for every route, and bound the head, the credential lookup, the body, and the write admission together; a read mount past it shall answer 408, and no handler shall be invoked once it has passed; a write whose body is incomplete at the deadline, or completed only at or after it, shall answer 408 with nothing submitted; a submitted write shall answer 408 only when an `Expired` reply was observed by `deadline + write_reply_grace`, and 503 `outcome_unknown` when the writer had not replied by then or its reply was taken at or after then; the deadline shall be read on the server's clock, so a clock moved before the request takes nothing from its budget. | mvp §2.5 deadlines | `http_layers::deadline_from_first_head_byte` (with the test server's 2 s budget, a head trickled over a nominal 1.4 s, then an incomplete body: 408 at 2 s from the first head byte), unit `http::tests::layers::slow_head_then_slow_lookup` (a head trickled over a nominal 1.4 s, then the lookup held at a fault point: 408 between 2 s − 50 ms and 2 s + 900 ms from the first byte), `http_layers::read_past_deadline_is_408`, `http_layers::plain_write_expired_is_408` and `http_layers::plain_write_unreplied_is_outcome_unknown` (the plain-write cases), `http_layers::ready_read_past_deadline_is_408` (a zero budget and a handler that would answer at once: 408, the handler not invoked), `http_layers::skewed_clock_keeps_the_budget` (the server's clock moved forward by more than the budget before an owner request: it reaches the handler), unit `http::tests::writes::body_after_the_cutoff_submits_nothing` (a complete body taken after the deadline: 408, zero store submissions), unit `http::tests::writes::reply_after_the_grace_is_outcome_unknown` (the waiter held at a gate while its job waits behind the writer past `deadline + write_reply_grace` and the writer then replies `Expired`; released, it polls with the reply and the timer both ready: 503 `outcome_unknown`, not 408), `http_layers::late_handler_result_is_408` (a read handler that moves the clock past the deadline and answers at once: 408), `http_layers::skewed_clock_keeps_the_timers` (the clock an hour ahead: a read mount that never answers is still cut off at its budget), unit `http::tests::reads::conversion_reads_no_clock` (a clock preempted for 50 ms while read: the converted deadline is unchanged), unit `http::tests::reads::late_body_is_408`; 7c: `http_commands::expired_observed_is_408` (a test job occupies the writer until just past the deadline: the intent job is taken after `not_after`, `Expired` arrives within the grace → 408, no receipt, no effect), `http_commands::writer_held_is_outcome_unknown` (the writer occupied well past `deadline + write_reply_grace`: 503 `outcome_unknown`, never 408), `http_matrix::build_router_rejects_inconsistent_mounts` (a write route mounted with `Mount::read` panics) |
| PR7-CL-9 | When `GET /api/events` is asked, the daemon shall answer per `decide_replay` from one read snapshot, with exact events and flags: 400 for `limit` 0 or 1001, `after_seq` with `before_seq`, and malformed, repeated, or unknown parameters; 404 with one body for an unknown or unreadable stream; `replay`, `events: []`, `head_seq: 0`, both flags false for an empty stream; an empty replay at `after_seq = head_seq`; a replay at 10 000 remaining events and a snapshot at 10 001; a replay at exactly 8 MiB of remaining payload and a snapshot one byte above; a snapshot for `after_seq` above the head and for an epoch mismatch; backward pages of the nearest events with exact flags, empty at `before_seq = 1`, the latest page for `before_seq` above the head, carrying the server epoch without switching mode on a mismatch; and stored rows of an unknown kind or newer `schema_version` returned unchanged. | mvp §2.4; mvp §4 PR 7 obligations; CL-13 | **7b:** `http_events::forward_cases`, `http_events::backward_cases` (each asserts `stream_id`, exact event ids, `head_seq`, `has_more`, `has_more_before`), `http_events::event_bound_edges`, `http_events::byte_bound_edges` (payload sizes chosen so the remaining `cum_bytes` is exactly 8 MiB and 8 MiB + 1), `http_events::unreadable_equals_unknown`, `http_events::unknown_kind_passes_unchanged` (rows inserted raw through the store's test helper; bytes equal) |
| PR7-CL-10 | When events are appended while a client pages forward from 0 toward a finite catch-up target (appends stop at a known head) within the replay bounds, the union of its replay pages shall equal the stream's events up to that head, each once. | CL-13; mvp §4 PR 7 "append between pages" | **7b:** `http_events::append_between_pages` (a writer appends N events while the client pages with `limit` 7; total stays below both bounds; the union equals the stream's ids in order) |
| PR7-CL-11 | A snapshot reply shall equal, field by field, the values computed independently from the database state at the cut it was read from, with `as_of_seq` equal to the requested stream's head at that cut; a thread snapshot shall carry the latest `vt_snapshot_taken` exactly while an attention cause is active or a card is open; a host snapshot shall list the threads, worktrees, and host- and workspace-scoped surfaces of that cut. | mvp §2.4 snapshot; mvp §4 PR 7 "snapshot contents restore every rendered field" | **7b:** `http_events::snapshot_equals_model_at_cut` (the test fixes the cut by holding the writer at a fault point after a known sequence of jobs across the host and thread streams; its own model computes every field from the jobs committed before the cut; exhaustive destructuring, so a new field fails to compile), `http_events::as_of_is_the_stream_head` (asserted separately from the fields), `http_events::same_cursor_changed_projection` (a `worktree_changed` on the host stream between two thread snapshots: equal `as_of_seq`, different `worktree`, each equal to its cut's model), `http_events::vt_snapshot_branches` (attention active, card open, both cleared), `http_events::host_snapshot_surfaces` (host and workspace scopes, a removed surface absent) |
| PR7-CL-12 | An agent token shall read only self+ streams, threads, worktrees owned by self+, and blobs referenced by those streams, and get 404 on the `host` stream; owner and rescue tokens shall read every stream, thread, and worktree, human-owned included; WebSocket subscriptions shall keep only readable streams. | CL-15; mvp §4 PR 7 "per-resource filtering" | **7b:** `http_reads::agent_sees_self_plus_only` (parent, agent, child, grandchild, sibling; each route), `http_reads::rescue_and_owner_see_all`, `http_ws::subscription_drops_unreadable` |
| PR7-CL-13 | When a blob is requested, the daemon shall serve it only for a readable stream with a `blob_refs` row, answering 404 for a missing reference even when every blob slot is taken, with the store's type only if it is `text/plain; charset=utf-8` or `application/json` and `application/octet-stream` otherwise, and the headers `X-Content-Type-Options: nosniff`, `Content-Disposition: attachment`, `Cache-Control: no-store`. | mvp §2.5 blobs; store commitment 3 | **7b:** `http_blobs::served_only_through_a_referencing_stream`, `http_blobs::missing_ref_is_404_with_slots_full`, `http_blobs::type_rule` (text, JSON, `text/html`, disagreeing types), `http_blobs::bad_query_is_400`, `http_blobs::bytes_equal` |
| PR7-CL-14 | A ticket shall be redeemable once, before its expiry sampled immediately before the removal, only while its token is valid at the final admission read, only for its purpose (and its thread for `attach:`), and an `attach:` ticket only for an owner token; a request rejected for its credential or final admission lookup, handshake, framing, or a full socket cap shall not spend a valid ticket; of concurrent valid redemptions exactly one shall succeed; a ticket query other than exactly one `ticket` shall be refused. | mvp §2.5; mvp §4 PR 7 "ticket reuse and expiry" | **7b:** `http_ws::ticket_single_use`, `http_ws::ticket_expiry_boundary` (injected clock at 29.999 s and 30 s, sampled immediately before the removal), `http_ws::ticket_dies_with_token`, unit `ws::tests::revoked_before_final_admission` (revocation committed between the credential read and the final admission read: 401, no `101`, ticket not spent), unit `ws::tests::revoked_between_admission_and_startup` (revocation committed after the final admission read: `101`, then close 1008), `http_ws::ticket_purpose_mismatch`, `http_ws::attach_ticket_owner_only` (through the test-only `test_attach` upgrade mount, which runs the shared admission and holds the socket), `http_ws::rejected_upgrade_keeps_ticket` (the credential lookup and the final admission lookup each failing with the reader pool exhausted, a missing `Upgrade` header, a GET body, sockets full; each followed by a successful redemption of the same ticket), `http_ws::concurrent_redemption_once` (16 valid requests on one token: exactly one 101, the rest 401), `http_ws::ticket_query_rules` |
| PR7-CL-15 | A pair code shall exchange once, before its expiry, only with a matching `Origin`, for one new owner token, and only while its issuing token is valid in the token-insert transaction; concurrent exchanges of one code shall yield one token. | mvp §2.5 secrets; §8 D12 | **7b:** `http_pair::single_use`, `http_pair::expires`, `http_pair::origin_required`, `http_pair::concurrent_exchange_once`, `http_pair::issuer_revoked_before_exchange` (401, no token), `http_pair::issuer_revoked_after_exchange` (revocation job queued after the insert job: the new token stays valid), `http_pair::lost_after_restart` |
| PR7-CL-16 | When a WebSocket opens, its token shall be verified before `hello`, and `hello` shall carry the store epoch; after each `subscribe` the client shall get a `heads` frame for the latest subscribed set (subscribes replaced before their frame was selected coalesce); a heads refresh shall become due every `heads_interval` and start as soon as no read is in flight, before any pending subscribe, and its frame shall list every subscribed stream with its current head; a commit on a subscribed stream shall produce a `stream_advanced` or a `heads` with a head at least the commit's; no `stream_advanced` shall name an unsubscribed stream or a head not newer than one already announced; a socket whose notifications were lost or lagged shall receive the current heads with the next periodic frame, within `heads_interval + 2 × read deadline + 2 × send_timeout`, or be dropped. | mvp §2.4; mvp §4 PR 7 "lost notification recovered by `heads`"; CL-13 | **7b:** `http_ws::hello_then_heads`, `http_ws::commit_is_announced`, `http_ws::periodic_heads_list_every_stream`, `http_ws::lost_notification_recovered_by_heads` (fault point drops notifications), unit `ws::tests::lag_with_buffered_old_notifications` (old notifications merged after the resync change nothing), unit `ws::tests::periodic_snapshot_overtakes_pending` (head 11 pending, the tick reads 12: no `stream_advanced(11)` after `heads(12)`), unit `ws::tests::resubscribe_takes_effect_at_merge`, unit `ws::tests::subscribes_coalesce` (two subscribes merged while a send is held: one `heads` frame, for the second set), unit `ws::tests::tick_during_read_refreshes_next` (heads time fires while a subscribe read is held and another subscribe is pending: when the read ends, the refresh read starts next and its `heads` frame follows), proptest `http_ws::subscribe_racing_commits` (interleaved subscribes and commits; announced heads per stream never decrease, and the client's final heads equal the store's) |
| PR7-CL-17 | When a socket's token is revoked, the socket shall close at startup, on the nudge, or at the latest `heads_interval` plus a read deadline later, even while a read or a send is in flight; a socket whose read or send misses its deadline, or with no inbound frame for `idle_timeout`, shall be dropped; at shutdown, sockets shall close (1001, best effort) and any socket still running at the deadline shall get an abort request and be counted. | mvp §2.5 "die with their token"; §5 | **7b:** `http_ws::revocation_closes_socket` (with the nudge, and without it: closed within the bound; a fresh commit in the window is announced at most as a head number), `http_ws::revoke_after_waiter_left_still_nudges` (the revoking request's client disconnects before the job commits; the socket still closes on the nudge), unit `ws::tests::control_deadlines_fire_during_a_pending_send_and_read`, unit `ws::tests::stalled_read_drops_the_socket` (a socket read held past its deadline at a fault point: the socket is dropped), `http_ws::shutdown_aborts_a_blocked_socket` (send held pending; shutdown budget shorter than `send_timeout`; counted under `abort_requested`), `http_ws::idle_socket_dropped` |
| PR7-CL-18 | One `command_id` shall cause at most one intent, and a repeat by its token with the same request identity shall return the stored status and body byte for byte; another token or another identity shall get 409 `conflict` with one body; an `in_progress` receipt 409 `in_progress` with `retry_after`; an `unknown_after_crash` receipt 409 `unknown_after_crash`. | CL-8; mvp §2.4; mvp §4 PR 7 "receipt HTTP codes" | **7c:** `http_commands::concurrent_duplicates_one_intent` (32 concurrent requests, `test_counter`: one event), `http_commands::replay_is_byte_equal`, `http_commands::other_token_conflicts` (receipt in each state: bodies equal), `http_commands::other_identity_conflicts`, `http_commands::in_progress_while_effect_runs`, `http_commands::unknown_after_crash` |
| PR7-CL-19 | Once an intent commits, its effect shall start exactly once and its receipt shall be completed by the runner, independently of the request: after a client disconnect before the outcome, after the reply time, under writer contention, after the caller's token was revoked during the effect, and after shutdown closed the registry within the drain budget; the receipt shall stay `in_progress` until the effect reports its result; an attempt admitted at the end of the completion budget shall still be awaited and commit; a completion that cannot commit shall reach `LifecycleSink` as `ReceiptUncompleted` without rerunning the effect; no reply after the intent commit shall be 408; a stored result shall be returned only after its completion committed. | CL-8; mvp §2.4, §2.5; store commitment 1 | **7c:** `http_commands::disconnect_before_outcome` (client closes while the intent job is queued; the effect runs once; a retry replays), `http_commands::late_successful_intent` (writer delayed past the reply time: 503 `outcome_unknown`; the intent then commits and the effect runs; a retry replays), `http_commands::completion_under_contention` (a test job saturates the queue during completion; completion retries and commits), `http_commands::revoked_during_effect` (the effect revokes the caller; the receipt completes), `http_commands::effect_holds_receipt_in_progress` (`test_slow_effect` held well past the request deadline: retries get 409 `in_progress`; after release the receipt completes and a retry replays), unit `commands::tests::effect_released_after_closing_completes` (shutdown starts while `test_slow_effect` is held; the effect is released before the drain deadline: the receipt completes, shutdown counts the runner under `exited`, and after a restart a retry replays the result), unit `commands::tests::last_admitted_attempt_is_awaited` (an attempt admitted just before the budget ends and held in the writer past it: it commits and `Done` follows), unit `commands::tests::completion_failure_is_fatal` (the writer fails during completion: `Failed` and `ReceiptUncompleted` reported, effect counter 1, the waiting request gets 503 `outcome_unknown`) |
| PR7-CL-20 | Every fatal store outcome `run_write` meets while serving (`CommitUnknown`, `WriterGone`, `Failed`) shall reach `LifecycleSink` whether or not a request still waits; a runner or observer dropped before it sees its job's result shall not report it, and after a restart the receipt shall be in the state the committed or uncommitted job left; work arriving after the registry closed shall be refused with nothing submitted. | store commitment 2; mvp §4 PR 8 | 7a (plain writes): unit `http::tests::errors::write_outcomes_map_to_statuses`, unit `http::tests::errors::fatal_outcomes_reach_the_sink` (a commit whose outcome is unknown reaches the sink whether or not a request waits; later submissions are reported as `Failed`); **7c:** unit `commands::tests::late_commit_unknown_reaches_the_sink` (waiter dropped; the store's fault point returns `SQLITE_IOERR` after the real `COMMIT`), unit `commands::tests::store_outcomes_map_to_statuses` (every `WriteError` and `SubmitError` through the writer's fault points), unit `registry::tests::shutdown_between_register_and_submit` (a runner registered just before closing is in the snapshot, submits, and completes; a request arriving just after gets 503 `shutting_down`, and nothing is submitted), unit `registry::tests::commit_fault_after_the_drain_deadline` (`test_slow_effect`'s intent job held in the writer; the runner's abort requested at the drain deadline; the fault point then returns `CommitUnknown` after the real `COMMIT`: no sink call, the store shuts down, and after reopening and `receipts_mark_unknown_after_crash` the receipt is exactly `unknown_after_crash`), `http_commands::retry_after_restart_outcomes` (the three receipt states set up through the store, then a new server: New, `Replay`, 409 `unknown_after_crash`) |
| PR7-CL-21 | If a command's token is revoked before its intent job runs, or its object check fails, or its `args` hold a duplicate member at any depth, then nothing shall be written, no effect started, and the reply shall be 401, 404 for an object the token cannot read, 403 for one it can read but not act on, or 400 `bad_args`. | CL-15; mvp §2.5 "Order"; mvp §2.4 | **7c:** `http_commands::revoked_between_guard_and_job`, `http_commands::object_denials` (unreadable target 404, readable but refused 403, `test_worktree_remove` on a child's worktree 403; no receipt, effect counter 0), `http_commands::duplicate_member_is_400_without_receipt` (top-level and nested duplicates over HTTP) |
| PR7-CL-22 | Every command of mvp §2.4 shall have a fixed action from 7a on; an unknown name shall be 404, a kind without the command's grant 403, and a registered unimplemented command 501, all before the body and with no receipt; `args` that is not an object shall be 400. | mvp §2.4 commands; CL-15 | unit `core::auth::tests::mvp_commands_match`, `http_matrix::command_matrix` (every name × kinds: an unknown name 404, a kind without the grant 403, a granted command 501), unit `http::tests::layers::rejections_precede_body` (the action and unknown-command cases: 0 body polls), `http_contract::command_request_args_must_be_an_object` (the exported schema); 7c: `http_commands::unimplemented_is_501_without_receipt`, `http_commands::args_must_be_an_object` (the decoded request) |
| PR7-CL-23 | The command request identity shall be equal for requests whose `args` mean the same normalized value whatever their key order, whitespace, or number spelling, and different for any other change, including the command name. | mvp §2.4 | **7c:** `commands::tests::identity_is_canonical` (proptest over the test handler's argument type with shuffled keys and number spellings), `commands::tests::identity_covers_name_and_values` |
| PR7-CL-24 | While the daemon serves requests carrying tokens, tickets, and pair codes, the `tracing` records shall include one record per request the daemon produced a response for, with its route template (or `unmatched`) and status, and none shall contain a secret, a query string, or a `/hook` or `/api/pair` body; the scrubber shall replace every secret pattern it is given. | mvp §2.5 secrets; mvp §4 PR 7 "secrets absent from logs" | `http_logs::secrets_absent` (a capturing subscriber at `TRACE`; positive control first: records for `/api/threads`, `/api/ws`, `/api/pair`, `/hook`, and `unmatched` exist with their status; then requests with a token in `Authorization`, a ticket in the query, a pair code, and a hook body with unique markers, rejected ones included; no marker appears; an extension method shaped like a token is 405 and not recorded; exactly one record per request), `http_logs::span_is_outermost` (a 421 and a 404 have records, both `route=unmatched`, one each), unit `http::trace::tests::redact_secrets_replaces_every_pattern` |
| PR7-CL-25 | The credential stored for every token shall be its SHA-256 digest, and no plaintext token, ticket, or pair code shall appear in the database or its WAL. | mvp §2.5 | `http_tokens::stored_credential_is_the_digest` (for each issued token, `tokens.sha256` equals SHA-256 of the secret), `http_tokens::no_plaintext_at_rest` (issue a token of each kind and one through `issue_owner`; scan the `-wal` bytes while the store is open and before any checkpoint, then the `.db` after shutdown: no token found, the digest found as a positive control); 7b adds a pairing and minted tickets to the scan |
| PR7-CL-26 | When a resource cap is reached, the daemon shall answer as stated: tickets (1024, 32 per token), pair codes (8), sockets (128, 16 per token), and blob slots (4) → 503 `busy` with `retry_after`; subscriptions over 1024, inbound frames over 64 KiB, and binary or invalid frames → the socket is closed, with close code 1008, 1009, or 1003 sent best effort and the disconnect bounded by `close_grace`; and the next request after a slot frees shall succeed. | §2.4, §2.6, §2.7, §5 | **7b:** `http_caps::each_cap_at_its_boundary` (the last allowed and the first refused for each; the close code asserted when the client reads it, the disconnect always), `http_caps::blob_slot_frees_after_a_stalled_client` (all four slots taken, one by a real client that never reads: its connection ends at the write stall, and the next download acquires the slot), `http_server::shutdown_with_connection_permits_exhausted` |
| PR7-CL-27 | `ObjectScope::stored_surface` shall allow an update or delete only when the stored surface is in the caller's scope, and refuse when it is not or does not exist. | mvp §2.5 "Order"; CL-15 | unit `http::tests::auth::stored_surface_scope` (thread, workspace, host scopes against owner, rescue, extension principals) |
| PR7-CL-28 | When `ServeHandle::shutdown` runs, the registry shall refuse new registrations from the moment it snapshots its entries, so no connection task is admitted after closing; when the report counts the accept task under `exited`, the listener shall have been released before shutdown returns; it shall return by its deadline with every registered task counted as `exited` or `abort_requested`; a task's entry shall exist before the task is spawned, and a task aborted before or during its first poll, or dropped by `tokio::spawn` itself, shall leave the registry; transient accept errors shall not end the listener. | §2.2, §5 | `http_server::shutdown_stops_accepting` (a counting `GET /`: no dispatch after shutdown, whatever holds the port), `http_server::listener_released_with_no_connections` (shutdown with no active connection: the accept task counted under `exited`, and binding the same port afterwards succeeds), `http_server::shutdown_returns_by_its_deadline` (one idle connection and one request held in its handler: returns by the deadline within 500 ms, the idle connection under `exited`, the held one under `abort_requested`), unit `http::tests::upgrade::shutdown_aborts_a_held_socket` (a socket that ignores shutdown: counted under `abort_requested`), unit `http::tests::registry::entry_precedes_first_action` (the task's first action observes its own entry), unit `http::tests::registry::abort_before_spawn_ends_at_first_poll` (shutdown aborts an entry between its insert and its spawn: the task ends at its first poll without running `fut`, and the entry is removed), unit `http::tests::registry::inline_drop_removes_entry` (the spawned future dropped without being polled, on the registering thread: the entry is removed and nothing blocks), unit `http::tests::registry::last_exit_between_check_and_wait` (the last task exits after shutdown's emptiness check and before its wait: shutdown returns at once, not at the deadline), unit `http::tests::registry::finished_tasks_leave` (the registry is empty after a burst of short tasks), unit `http::tests::listener::accept_error_continues` |
| PR7-CL-29 | Every read shall have one cutoff at its deadline: a store outcome returned before it shall map to 503 `busy` (`ReadError::Busy`, `ReadError::Store(Busy)`), 503 `shutting_down` (`ReadError::Closed`), or 500; a read not returned by it shall answer 408; a reader pool exhausted through the cutoff shall answer 503 `busy` or 408, never a success; any other result, a success or a store error, taken at or after the deadline shall be discarded and answered 408. | §2.3, §2.9 | unit `http::tests::reads::read_outcomes_map` (`Store(Busy)` injected before the cutoff: 503 `busy`; store shut down: 503 `shutting_down`; a read held running past the cutoff: 408; pool exhausted through the cutoff: 503 or 408), unit `http::tests::reads::late_read_success_is_408` (the injected clock moved past the deadline after the read returned a success: 408), unit `http::tests::reads::late_admission_read_keeps_the_ticket` (the same for the final ticket admission read: 408, and the ticket redeems afterwards), unit `http::tests::reads::late_read_error_is_408` (the waiter held at a gate until a `DiskFull` read has finished and the real cutoff has passed; released, it polls with the result and the timer both ready: 408, not 500) |

**Mapping.** mvp §4 PR 7 obligations: `limit` 0 and above max → CL-9; empty, unknown, unreadable stream → CL-9; `after_seq = head_seq` and above → CL-9; append between pages → CL-10; epoch mismatch forward and backward → CL-9; byte bound → CL-9; `after_seq` + `before_seq` → CL-9; `before_seq` at 1 → CL-9; lost notification recovered by `heads` → CL-16; snapshot contents → CL-11; every route × token kind cell → CL-4 (with CL-2, CL-3, CL-22); per-resource filtering → CL-12; `Origin` vs received `Host` → CL-2; ticket reuse and expiry → CL-14; secrets absent from logs → CL-24, CL-25; receipt HTTP codes → CL-18, CL-19, CL-20. mvp §6.2: CL-8 → PR7-CL-18, CL-19, CL-20 (PR 10 adds its own concurrent duplicates); CL-13 → PR7-CL-9, CL-10, CL-11, CL-16 (with PR 17's model and PR 18's reducer); CL-15 → PR7-CL-4, CL-5, CL-12, CL-21, CL-22, CL-27 (PR 20 adds the rescue and scope cases). mvp P-20 → PR7-CL-1, CL-3, CL-4, CL-6. CL-16 (B-8 per layer) stays PR 9's; PR7-CL-6 and CL-7 cover the same steps for PR 7's routes.

**Bounded windows** (decided and tested): the unannounced revocation window (§2.6; CL-17's no-nudge case), the reply before the writer has answered (§2.8; CL-8, CL-19).

**Known limits** (no test, by design):

- **Large in-flight bodies on Windows** (H5): a client that keeps sending a large body after a pre-body rejection may see a reset instead of the decided status; PR 7 claims the decided status only for small in-flight bodies, and PR 9 measures the large-body case with B-8.
- **Intent closures' captures** are a review rule (store §5): Rust cannot see what a closure captured.
- **Panic output** goes through PR 8's panic hook, outside PR 7's `tracing` claim (§2.10).
- **Effect bounds** belong to the owning PR (§2.8): PR 7 cannot test a real effect's bound or that a negative result means the work cannot happen later; its test effect proves only that the runner waits for the result and keeps the receipt `in_progress` meanwhile (CL-19).
- **An abort request does not prove exit**: a task blocked outside an `.await` keeps running after its abort request; the report says `abort_requested`, not `exited`.
- **A head completed as hyper's header timer expires**: hyper parses a head that is complete when it polls the connection, before its `header_read_timeout` timer, so such a request is dispatched instead of closed (no EOF). hyper's timer starts before the first byte and the application deadline at it, so the request may still have budget left (an idle connection whose head arrives in one burst near the timer's end) and gets a normal response, a `101` included; only when the application deadline has also passed does it answer 408 at the first deadline check it reaches (no handler runs).
- **A `101` racing shutdown**: a `101` registered just before `graceful_shutdown` disables keep-alive, and encoded after it, goes out as `Connection: upgrade, close` (H2), which the pinned client and WebKit refuse (H17). The daemon is stopping and tickets live only in memory, so the spent ticket loses nothing a restart would not; the client reconnects after the restart as it would anyway.

## 7. Tests and Completion Commands

**Harness.** `tests/support/http.rs` starts an `HttpServer` on a `127.0.0.1:0` listener with a store in a `tempfile::TempDir` (fixed `Clock` and counter `IdSource` from the store's test support), an injectable monotonic clock for tickets and pair codes, `heads_interval` of 200 ms, a recording `LifecycleSink`, the test-only `test_attach` upgrade mount and the test command registry (§2.8), and helpers to mint tokens of each kind and build thread trees through the store's `submit`. Requests use a raw HTTP/1.1 client over `tokio::net::TcpStream`, so trickled or malformed heads, withheld or in-flight bodies, non-reading clients, and EOF are observable exactly; guard tests mount a stub whose request body counts its polls. WebSocket tests use `tokio-tungstenite`. Each test shuts the server and the store down before its directory drops. Fault points (`#[cfg(test)]` hooks: hold the guard's lookup, hold a read, hold a socket loop, its read, or its send, drop a socket's notifications, inject an accept error, pause between registration and submit) are crate-internal, as the store's are; tests needing them are unit tests in `src/http/tests/`, and they also use the store's writer fault points. Concurrency cases use barriers or fault points, never sleeps, for ordering.

**Matrix tests against an independent inventory.** `tests/http_matrix.rs` holds a literal inventory written from mvp §2.5 (method, path, mount kind, Origin rule, credential kinds, action) and `MVP_COMMANDS`' names written out again. One test asserts set equality with `ROUTES`; the others probe the production router built by `Mounts::mvp()` for behaviour, so adding, removing, or changing a route on the production side alone fails. The action table has its own literal test (CL-5). `ROUTES` stays the production source; the inventory is the test's.

**Contract fixtures.** `core::http`'s types join the drift test of `packages/contracts`. `tests/http_contract.rs` captures one HTTP response per `ErrorBody` code, per credential body, and per list body from the running server and checks that each decodes into its exported type, validates against its generated JSON Schema, and carries `Cache-Control: no-store`; a `CommandRequest` whose `args` is an array fails validation against the generated schema; it also captures hyper's own 400 and 431, which carry neither an `ErrorBody` nor `Cache-Control`, and a `HEAD` 405, which carries `Cache-Control: no-store` and no body (the boundary of §2.9).

**Regression rule.** Each named test is seen failing with its behaviour reverted (the Host check removed, the ticket removal moved before the final admission read, the receipt lookup skipped, the runner replaced by awaiting in the request, completion re-verifying the token, a write route mounted as read, frames queued instead of built at send selection, the span replaced by `TraceLayer`'s default, `OwnWorktree` widened to self+, the registry's closing check removed, the registry insert moved after the spawn, `Connection: close` removed from ordinary responses, `close` added to the `101`, the late-result check removed, the refresh priority removed, the runner registering a second task for its completion, the final admission read removed, the duplicate scan removed) and passing with it restored before the PR merges; the PR description lists the reverts. hyper-owned parsing (H3) is checked by integration assertions against the pinned version, not by reverts.

Completion commands:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum|hyper|tower|getrandom) ' <<<"$deps"
deps="$(cargo tree -p chaldea-pty-runtime -e normal --prefix none)" && ! grep -Eq '^tokio ' <<<"$deps"
```

plus the `workspace` job green on `macos-latest` and `windows-latest`, the contract drift test regenerated for `core::http`'s types (§8 D13), and the `pty-hooks` job unchanged.

## 8. Decisions

**D1. PR 7 ships as three PRs.** **7a** (server and access): `HttpServer`, `TaskRegistry`, the accept loop and `ConnIo`, `OuterLayer`, `GuardLayer`, `ROUTES` and `Mounts::mvp()` with every route mounted (handlers 501), `MVP_COMMANDS` and `core::auth`, `TokenService` and `verify_*_in`, `ReadCtx`/`WriteCtx`/`run_write`/`observe_write`, `LifecycleSink`, the status mapping, the span and redaction, `ServeHandle::shutdown`, the public bodies of D13; claims CL-1–CL-7, CL-27–CL-29, the Bearer and command-name cells of CL-4, and the guard, read, and route cases of CL-8, CL-22, CL-24, and CL-25; their remaining cases (commands, `args`, pairing, tickets) run in the part that adds those handlers. **7b** (reads and live): `/api/events`, `/api/threads`, `/api/worktrees`, blobs and `ReadTx::blob_ref_exists`, tickets, pairing, the WebSocket loop; CL-9–CL-17, CL-26, and the ticket and pair-code cells of CL-4. **7c** (commands): the duplicate scan, identity, `CommandHandler`, registry, `CommandRunner`, receipts; CL-18–CL-21, CL-23. PR 8 and PR 9 are siblings that each depend on PR 7 (mvp §4); both need only 7a and start once it merges, while 7b and 7c proceed in parallel with them. PR 10 depends on 8 and 9 and also needs 7c; PR 17 consumes the receipt rules and needs 7c (and 7b's replay rules); PR 18 needs 7b. 7c is on the longest path only if it takes longer than PR 8 and PR 9. The two-way split was rejected because its first half would hold PR 8 and PR 9 behind the sync API and WebSocket, which neither needs.

**D2. Own accept loop on hyper's HTTP/1 builder with a timer**, a 5 s header read timeout, 8 KiB heads, 64 headers, 128 connections, a 30 s write stall armed only while a write is pending, and axum's accept-error rule; HTTP/1.1, with HTTP/1.0 requests under the same rules once parsed (no upgrade; hyper answers an HTTP/1.0 request carrying `Transfer-Encoding` with its own 400 before the `Host` check); one request per connection (`Connection: close` on every final response except a successful `101`, which keeps exactly `Connection: Upgrade` except for one racing shutdown, §6.2 "Known limits").

**D3. Access is decided by an outer layer and one per-route guard built from a static route table**, which is the only production way to build the router; an independent literal inventory equals it and tests the real router. Order: Host → (routing) → method → Origin → credential → kind and action → framing (Transfer-Encoding first) → size → unimplemented → deadline; no application body use before the end. The mount kind in each spec, enforced by typed constructors, decides which routes may write and which may time out by dropping. One deadline origin for every route: the connection's first head byte, since each connection carries one request.

**D4. Tokens are 256-bit `chd_` secrets stored as SHA-256 and checked by hash at use time in the transaction that uses them**, with a `subtle` final comparison; after a request, only the id and hash travel. Agent tokens also need their launch current and not exited. Tickets (`chdt_`, 30 s) are peeked and token-checked at the credential step and spent by a conditional removal after every fallible check; pair codes (`chdp_`, 60 s) are bound to their issuer; both are memory-only, and a restart loses them safely.

**D5. One task registry owns everything that outlives a request**, the accept loop included, with one closing transition shared with shutdown; a task's entry exists before its first action; `closing` stops admission only, and admitted work finishes within the drain budget; shutdown reports `exited` and `abort_requested` separately. Every write goes through one async `run_write` that owns its ticket and reports fatal outcomes while serving; a plain write wraps it in one registered observer, and a runner awaits it directly. A runner or observer dropped before it sees its result does not report it, because the daemon is already stopping and PR 8 reconciles receipts before serving again.

**D6. A committed intent is owned by its registered `CommandRunner`.** The runner calls `receipt_begin` before the intent and `receipt_complete` itself, runs the effect once and waits for the terminal result its owning PR guarantees, completes the receipt within a 60 s attempt-admission budget with no token check, and reports a completion that cannot commit as fatal. 408 means a deadline missed before submission, or an `Expired` observed by `deadline + write_reply_grace` for this attempt; `IntentCommitted` is 409 `in_progress`; no writer reply by then is 503 `outcome_unknown` (D14).

**D7. Each WebSocket is a registered task from before `101`**; it verifies its token at startup and on every read and nudge; every source of heads merges into `known`, and frames are built at send selection, so no `stream_advanced` lowers an announced head and no frame selected after a subscribe's merge names a stream outside the new set; subscribe responses may coalesce; every read and send has a deadline whose expiry drops the socket, and control deadlines run independently of in-flight reads and sends; a heads refresh becomes due every `heads_interval` and starts first when the in-flight read ends, and its frame is a snapshot of the latest subscribed set (§8 D15). The unannounced revocation window is bounded and tested.

**D8. Agents get 404 on the `host` stream** and read threads and worktrees through filtered routes; objects a token cannot read answer 404, objects it can read but not act on 403, decided by one `deny_for`. Agents remove only worktrees they currently own.

**D9. No mvp command is completed in PR 7**; all are registered with fixed actions and 501 handlers until their PRs, with test-only handlers proving the seam.

**D10. Logs carry route templates, never URIs, queries, header values, or bodies**; secrets have redacted `Debug`, a pattern scrubber is the second line, and PR 8's panic hook omits panic payloads.

**D11. Blob bodies are admitted after their reference is known**, four at a time; every reply is cut off by the connection's write stall. Other replies are bounded by mvp §2.4's page and replay limits, not by admission.

**D12. `chaldea open` gets its pair code from `POST /api/pair-codes`** (owner): the route takes an owner Bearer token, requires `Origin` to be absent, and replies `{ code, expires_in: 60 }`; a code is bound to its issuing token: the exchange refuses it when that token is revoked before the token-insert job commits (§2.4). Only a non-browser owner client can mint a code, no code is stored at rest, and a revoked owner cannot confer access. `open` stays a CLI subcommand (mvp D2); a command `open` would persist the code in its receipt, and a ticket purpose `pair` would let browsers mint.

**D13. The public wire bodies are `core::http` types** (owner): `CommandRequest { command_id, args }` (`args` exported as an object) with snake_case command names; `ErrorBody { code, retry_after? }` (omitted when absent, whole seconds) with §2.9's codes; `WsTicketRequest` / `WsTicketResponse`, `PairCodeResponse`, `PairRequest` / `PairResponse`, `ThreadsResponse`, `WorktreesResponse`. hyper's own responses and the empty body of a `HEAD` 405 carry no `ErrorBody`, and every response the daemon generates (not hyper's parser responses) carries `Cache-Control: no-store` (§2.9). They join the contract drift test and are checked against captured responses (§7), so the PWA and the Phase 2 relay share one source of truth.

**D14. A command whose writer has not replied by `deadline + write_reply_grace` answers 503 `outcome_unknown` with `retry_after`** (owner), the same code as a known `CommitUnknown` / `WriterGone` and an uncompleted receipt (§2.8). The job may be queued, running, or committed with its result unobserved, and a retry with the same `command_id` is correct in every case; 409 `in_progress` would claim a receipt that may never exist.

**D15. Live notifications answer the latest subscription** (owner): each `heads` frame is a snapshot of the latest subscribed set, and subscribes may coalesce; a refresh becomes due every 30 s; a lost notification is recovered within `heads_interval + 2 × read deadline + 2 × send_timeout` (60 s with the defaults), or the socket is dropped and the reconnect's subscribe returns current heads (§2.6). This is what one read at a time with bounded reads and sends guarantees; convergence holds because announced heads never decrease and every frame is a snapshot. PR 18's reducer treats every `heads` frame as a snapshot of the latest subscribed set.

**Where the contract lives.** mvp §2.4 and §2.5 state the public contract of D12–D15, one request per connection, the deadline origin, the read cutoff, and the write reply grace; mvp §4 states the PR 8, PR 9, and PR 18 obligations this design hands off; event-store §2.4, §2.5, and §2.7 state the `TokenKind` re-export and `ReadTx::blob_ref_exists`.
