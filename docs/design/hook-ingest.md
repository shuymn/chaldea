# Design: Hook Ingest (MVP PR 9)

Status: design. Scope is the PR 9 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): `POST /hook`, binding, `raw_unknown`, `chaldea hook-emit`, and the B-8 cases (arch §5 open item **Windows hook receiver**). The contract implemented is mvp §2.1 (hook idempotency keys, `raw_unknown`), §2.5 (the `/hook` route, its rejections and deadline), §2.9 (early hooks, binding, buffering), the PR 9 and PR 9 / 10 design obligations of mvp §4, and the claims CL-10, CL-16, CL-17, CL-23 of mvp §6.2. It builds on the store of `docs/design/event-store.md` (cited as "store §N"), the HTTP surface of `docs/design/http-surface.md` (cited as "http §N"), and implements the `buffer*` and `bind` actions that `specs/thread_input.qnt` assumes (`docs/design/formal-specs.md`, cited as "formal §N").

**What PR 9 decides and what it leaves.** PR 9 owns everything between the `/hook` guard and the store: the request headers, the hook body budget, intake order and intake facts, the process-ancestry walk, delivery deduplication, binding, session selection, buffering and promotion, `raw_unknown` reasons, the attribution of descendant processes to a launch, and the emitter (`chaldea hook-emit`). It does not decide what a bound hook *means* (which events a `UserPromptSubmit`, `Stop`, or `PermissionRequest` produces, readiness evidence, ack attribution): that is a seam (`HookAdapter`, §2.6) filled by PR 10 (supervisor facts) and PR 13 / 14 (payload decoding). It does not launch CLIs, inject hooks, mint hook tokens, run timers, or recover at restart (PR 10, 13, 14); it names the functions those PRs call (§2.9, §4).

**Commitments from PR 6 and PR 7 this design keeps.**

1. One store job per delivery: deduplication on `(launch_id, emit_id)`, the routing decision, the events, binding, and promotion run in one `BEGIN IMMEDIATE` transaction (store §2.3 "Hook deliveries"). The store keeps buffered data opaque; PR 9 owns its format (§2.7).
2. The reply is sent only after the job's ticket resolved `Ok`, so a buffered hook is durable before 200 (store §2.9, D5).
3. `/hook` is a `Mount::write` route (http §2.2 route table, `crates/daemon/src/http/routes.rs:264-270`): Host, `Origin` absent, `hook` token, framing, and size are decided by PR 7's guard before the handler, and the deadline runs from the connection's first head byte (http §2.2 "One deadline origin"). PR 9 adds no check before the guard and changes none of its order.
4. Store outcomes map through PR 7's write mapping (`crates/daemon/src/http/error.rs:223-244`); PR 9 adds no status code.
5. `core` is unchanged: no public type, payload, kind, or reason is added.

Sources read for this document:

| Source | Revision | Read for |
|---|---|---|
| This repository | `3df7c70` | `docs/mvp-design.md`, `docs/architecture.md` §5, `docs/prior-art.md` rows 5a, 5b, 6a–6c, 9, `docs/design/{event-store,http-surface,formal-specs,pty-runtime,pty-unix,pty-windows}.md`, `specs/thread_input.qnt` (`bind`, `buffer*`), `crates/daemon/src/http/{routes,guard,ctx,auth,error}.rs`, `crates/daemon/src/store/{rows,write_tx,readers,schema,annex}.rs`, `crates/daemon/tests/http_layers.rs`, `crates/core/src/{state,payload,ids}.rs`, `crates/pty-runtime/src/{lib,backend}.rs`, `unix/{table,sys}.rs`, `windows/sys.rs`, `crates/testkit/src/fake_cli/`, `Cargo.toml`, `.github/workflows/ci.yml` |
| Spike `spikes/pty-hooks` | `3df7c70` | `src/emitter.rs`, `src/http_client.rs`, `src/receiver.rs` (binding, B-8), `src/fake_cli.rs` (`echo --nested`), `src/os/{mod,unix,windows}.rs` (ancestry), `src/scenario/{fake,real}.rs` (CLI names), `docs/spikes/pty-hooks-spike.md` B-8, B-9, B-11, F-3c, §4.3 |
| `hyper` | crate 1.12.0 | `src/proto/h1/role.rs`, `src/proto/h1/conn.rs` |
| `libc` | crate 0.2.191 | `src/unix/bsd/apple/mod.rs` (`proc_bsdinfo`) |
| `windows-sys` | crate 0.61.2 | `Cargo.toml` features (`Win32_System_Diagnostics_ToolHelp`) |

**Facts this design relies on.** T-facts are exercised by named tests of §7 on both runners; D-facts rest on the cited source; A-facts are assumptions not verified in source for this document.

| # | Fact | Where | Kind |
|---|---|---|---|
| K1 | hyper answers a `Content-Length` that is not all digits (`+5`) and two different `Content-Length` values with its own 400 before the service; it keeps one of two equal `Content-Length` headers and drops the second; with `Transfer-Encoding` present it removes `Content-Length` (either order); an HTTP/1.0 request with `Transfer-Encoding` is its own 400. | hyper `src/proto/h1/role.rs:275-312` | T (`http_layers::parser_layer_cases`, `te_with_content_length_is_400`; PR 9's `hook_b8::*`) |
| K2 | After a response sent before the body was read, hyper reads what is already available once and otherwise closes the read side (http H5); on Windows a close with unread data can reset the connection. | hyper `src/proto/h1/conn.rs:875-891`; spike §3 B-8 | T: PR 9 records the observation per OS (`hook_b8::body_in_flight_observed`) |
| K3 | `WriteCtx::write` verifies the token inside the job, submits with `not_after = deadline`, awaits the result until `deadline + write_reply_grace`, and maps failures; `verify_hash_in` checks only revocation for a `hook` token. | `crates/daemon/src/http/ctx.rs:206-229`, `:358-374`; `auth.rs:152-180`; `error.rs:223-244` | D |
| K4 | `hook_delivery_record` returns `Replay` for an existing `(launch_id, emit_id)` and changes nothing; `receipt_ordinal` is the caller's, unique per launch; promotion is only `buffered → appended \| raw_unknown`, enforced by trigger. | store §2.3, §2.4; `write_tx.rs:884`, `:939`; `readers.rs:1232` | D (PR 6 tests) |
| K5 | `reduce(SessionBound)` refuses unless `process_state` is `running` or `stopping`, and removes both startup attention causes. | `crates/core/src/state.rs:274`, `:298-305` | D |
| K6 | The macOS process lookup reads `proc_bsdinfo` (pid, ppid, pgid, start in µs); `proc_bsdinfo` also has `pbi_name[32]` and `pbi_comm[16]`. The Windows backend has `open_process` and `creation_time` but no parent lookup; `windows-sys 0.61.2` has the `Win32_System_Diagnostics_ToolHelp` feature, which the spike's Windows ancestry used. | `crates/pty-runtime/src/unix/table.rs:28-48`, `unix/sys.rs:189-225`; libc `apple/mod.rs:682-683`; `windows/sys.rs:485`, `:497`; windows-sys `Cargo.toml:200`; spike `src/os/windows.rs:27-28`, `:126-128` | D; T (`process_native::*`) |
| K7 | `ProcessIdentity.start_time` is decimal µs since the Unix epoch on macOS and the creation `FILETIME` in decimal 100 ns units on Windows; equal `(pid, start_time)` identifies a process, not its provenance. | `docs/design/pty-unix.md` §2.1 ("Identity format"); `docs/design/pty-windows.md` §2.1, §2.5 | D |
| K8 | The spike emitter exits 0 from a watchdog thread at 3 s whatever the main thread blocks on, caps stdin at 4 MiB + 1, never writes stdout, accepts only `http://127.0.0.1:<port>/hook` or `http://localhost:<port>/hook`, and connects to `127.0.0.1` whatever the `Host`. | spike `src/emitter.rs:34-103`, `src/http_client.rs:28-50`, `:62-95` | D; T again in PR 9 (`hook_emit::*`) |
| K9 | The spike bound Codex and FakeCli launches by the nearest ancestor of the emitter (skipping the emitter) whose normalized name is a CLI name (the agent name and the launched binary's file stem), equal to the launched pid; later hooks re-checked it; a nested FakeCli failed it (B-11). Its chain came from the emitter's own header. | spike `src/receiver.rs:854-861`, `:873-1003`; `src/scenario/real.rs:285-291` | D |
| K10 | `bind` appends `session_bound`, then applies the buffered hooks in intake order with the facts of their intake; `buffer*` records hooks of an unbound live launch without feeding a reducer. | `specs/thread_input.qnt:885-993`; formal §1, §3 hole d | D |
| K11 | Codex runs hook commands concurrently, so `UserPromptSubmit` can arrive before `SessionStart`. | mvp §2.9; arch §5 | D (from the mvp) |
| A1 | Claude Code starts a new session id on `/clear` and fires `SessionStart` for it; Codex may do the same on `/new`. | not read for this document | A (PR 12 fixtures) |
| A2 | On Windows, a loopback connect to a port with no listener can take about 2 s to be refused. | not measured | A (the emitter's connect bound covers either way) |

## 1. Goal and Dependencies

**Goal.** A `hook` module in `chaldea-daemon` that accepts a CLI hook on `POST /hook`, decides its routing (appended to the thread, buffered while its launch is unbound, or kept as `raw_unknown`) in one store job, binds a launch only through mvp §2.9's rule, promotes buffered hooks in receipt order at binding, records which processes the launch's hook chains proved to be its descendants, and replies only after the decision is durable; and `chaldea hook-emit`, which posts one hook payload with one `emit_id`, retries within its budget, and always exits 0 within 3 s with nothing on stdout. Every B-8 case is a table-driven test per layer on `macos-latest` and `windows-latest`.

**PR split** (§3 "PR size"): **9a** — `pty_runtime::process` (public lookup and ancestry walk), `crates/cli` with `chaldea hook-emit`, FakeCli hook emission; **9b** — the daemon's `hook` module, store migration v2, the `/hook` mount, B-8, the end-to-end tests. 9b depends on 9a.

**Files.**

```
crates/pty-runtime/src/process.rs        # public: ProcessEntry, Lookup, lookup(pid), Ancestry, ancestry(pid, max); the walk over a Table trait (9a)
crates/pty-runtime/src/unix/table.rs     # + the image name from pbi_name / pbi_comm (9a)
crates/pty-runtime/src/unix/sys.rs       # BsdInfo gains `name` (same proc_pidinfo call) (9a)
crates/pty-runtime/src/windows/process.rs# Toolhelp snapshot table + creation time per pid (9a)
crates/pty-runtime/src/windows/sys.rs    # CreateToolhelp32Snapshot, Process32FirstW / NextW wrappers (9a)
crates/pty-runtime/tests/process_native.rs
crates/cli/Cargo.toml, src/main.rs       # binary `chaldea`; `hook-emit` dispatched first (created by 9a unless PR 8 created it)
crates/cli/src/hook_emit/{mod,env,client}.rs   # the emitter: env, stdin, request, retry, watchdog (9a)
crates/cli/tests/hook_emit.rs            # emitter against stub listeners (9a)
crates/cli/tests/hook_e2e.rs             # daemon + FakeCli + emitter, real processes (9b)
crates/testkit/src/fake_cli/echo.rs      # + --emitter, --session-id, --nested, --nested-child (lift of spike echo, 9a)
crates/testkit/src/fake_cli/hooks.rs     # new mode `hooks`: a scripted hook sequence, optionally concurrent (9a)
crates/daemon/src/hook/mod.rs            # HookIngest, HookConfig, HookSeams, mount() (9b)
crates/daemon/src/hook/request.rs        # X-Chaldea-* header parsing (9b)
crates/daemon/src/hook/payload.rs        # decode(): the pure hook-head decoder, the fuzz target of PR 22 (9b)
crates/daemon/src/hook/ancestry.rs       # Chain, eligibility, descendants to attribute (9b)
crates/daemon/src/hook/intake.rs         # IntakeCounter (9b)
crates/daemon/src/hook/route.rs          # the delivery job: dedup, decide, bind, buffer, promote (9b)
crates/daemon/src/hook/buffer.rs         # buffer bounds, expiry, resolve_unbindable (PR 10 calls it) (9b)
crates/daemon/src/hook/adapter.rs        # HookAdapter, MvpHooks (the default until PR 10 / 13 / 14) (9b)
crates/daemon/src/http/ctx.rs            # + WriteCtx::read, WriteCtx::commits (9b)
crates/daemon/src/http/auth.rs           # verify_hash_in: the hook token's launch checks (9b)
crates/daemon/src/store/schema.rs        # migration v2: launch_descendants, events_session_bound_once (9b)
crates/daemon/src/store/{write_tx,readers}.rs   # descendants_record, descendants (9b)
crates/daemon/tests/hook_b8.rs, hook_ingest.rs, support/hook.rs   # (9b)
```

**Dependencies.** No new crate and no new version. Changes:

```toml
# Cargo.toml [workspace.dependencies] windows-sys features: one line added
  "Win32_System_Diagnostics_ToolHelp", # CreateToolhelp32Snapshot, Process32FirstW / NextW (the parent pid of the ancestry walk)
```

- `crates/daemon`: `chaldea-pty-runtime = { path = "../pty-runtime" }` (normal; PR 10 needs it anyway). Dev: unchanged.
- `crates/cli` (new unless PR 8 created it): `uuid.workspace = true` with `features = ["v4"]` (the `emit_id`; `uuid =1.29.1` already uses `getrandom 0.4.3`). Nothing else on the `hook-emit` path: std networking only (mvp §3, prior-art row 5b's hand-written alternative, needed for the separate connect bound). Dev: `chaldea-daemon`, `chaldea-core`, `chaldea-pty-runtime`, `tokio` (`rt-multi-thread`, `net`, `time`, `macros`), `tempfile`, `serde_json`, `sha2`.
- `crates/testkit`: unchanged dependencies; FakeCli runs the emitter as a child process given by `--emitter <path>`, as in the spike.

**Workspace and CI.** `crates/cli` joins `members` if absent. No new job. `cargo test --workspace --locked` runs everything on both runners. The Windows sentinel step lists `chaldea-pty-runtime --test process_native ancestry_of_a_child_reaches_this_process` (the Toolhelp path compiles only on Windows). The `core` dependency check is unchanged. `lefthook.yml` mirrors the sentinel change only.

**Binary locations in tests.** `crates/cli/tests` gets the emitter from `env!("CARGO_BIN_EXE_chaldea")`. FakeCli is resolved as `chaldea-fake-cli[.exe]` in the directory above the test executable's `deps/`; under `cargo test --workspace` testkit's binary is built before any test runs, and the helper fails with "build with cargo test --workspace" when it is missing, never skips.

## 2. Types and Functions

Names may change in review; shapes may not.

### 2.1 The Rules

1. **One job decides.** Deduplication, routing, binding, buffering, promotion, buffer bounds, and descendant records are decided inside the delivery's store job, from what the database holds at that moment (§2.10). Everything before the job (header checks, body, ancestry walk, pid wait) gathers inputs and writes nothing.
2. **The only memory is the intake counter.** The per-launch receipt counter (§2.4) is the one cross-call value outside SQLite, permitted by store §2.3 because a launch never outlives a daemon run (mvp §2.9 "Daemon restart" ends every open launch and revokes its tokens). Every other value is read in the job.
3. **Hook meaning is the adapter's.** PR 9 interprets a payload only as far as binding and session selection need: `hook_event_name`, `session_id`, and for `SessionStart` `transcript_path` (§2.5). Every other event of a selected session goes through `HookAdapter::interpret` (§2.6).
4. **Unbound intake changes no state.** A buffered hook appends no event, feeds no reducer, and changes no projection (formal §3 hole d). Only hooks of the bound launch and its session reach `interpret`, at intake or at promotion, with the facts captured at their intake.
5. **Every wait has a deadline** (§5 "Waits"), and every ending of a request is listed (§5 "Endings").

### 2.2 Request Shape

The emitter's request (and the only shape the handler accepts past the guard):

```
POST /hook HTTP/1.1
Host: 127.0.0.1:<port>                 (or localhost:<port>, as in CHALDEA_HOOK_URL)
Connection: close
Authorization: Bearer chd_<43>          (CHALDEA_HOOK_TOKEN)
Content-Type: application/json
Content-Length: <n>                     (0 ≤ n ≤ 4 MiB)
X-Chaldea-Emit-Id: <uuid>               (public, mvp §2.1; required)
X-Chaldea-Emitter-Pid: <decimal pid>    (internal; required for ancestry, else the chain is empty)
X-Chaldea-Thread-Id: <uuid>             (internal; CHALDEA_THREAD_ID)
X-Chaldea-Agent: <[a-z0-9_-]{1,32}>     (internal; the emitter's argument, recorded only)
X-Chaldea-Emitter-Version: <semver>     (internal; recorded only)
X-Chaldea-Body-Truncated: 1             (internal; present only when stdin exceeded 4 MiB)
```

No `Origin` (403 otherwise, guard step 3). Header parsing (`request.rs`) runs after the guard and before the body: `X-Chaldea-Emit-Id` must occur once and parse as a UUID in its hyphenated form, else 400 `bad_request`; each other `X-Chaldea-*` header may occur at most once (a second occurrence is 400 `bad_request`); a pid that is not a decimal `u32` in `1..` is treated as absent; an agent or version outside its form is recorded as `unknown`. Unknown `X-Chaldea-*` headers are ignored (a newer emitter against an older daemon). The internal headers are a protocol between two copies of the same binary (§8 D6).

**Body cap.** 4 MiB (mvp §2.5): over it the guard answers 413 before the body. The emitter never sends more: a longer stdin is sent as its first 4 MiB with `X-Chaldea-Body-Truncated: 1` and is kept as `raw_unknown` (`unparseable`), never interpreted (§8 D5).

**Body budget.** Before reading the body the handler takes `ceil(Content-Length / 1 KiB)` permits from one `Semaphore` of `body_budget / 1 KiB` permits (32 MiB), with `try_acquire_many_owned`; none free → 503 `busy` with `retry_after: 1`, nothing read. The permit is held until the job's result is taken (the job owns a copy of the bytes; the permit bounds the request's own copy). This caps hook bodies in memory at 32 MiB, against 128 connections × 4 MiB without it.

**Reply.** Every recorded delivery answers `200` with an empty body (`Content-Length: 0`), the guard's `Cache-Control: no-store`, and PR 7's `Connection: close`; the stored first reply is `{ http_status: 200, body: "" }`. The disposition is observable in the thread stream and in `hook_deliveries`, not in the reply (§8 D7). Every other status is a rejection or an unknown outcome, decided before or by the job, and stores nothing (§2.11).

### 2.3 Handler Flow

```rust
async fn handle(ingest: Arc<HookIngest>, ctx: WriteCtx, req: Request<Body>) -> Result<Response, ApiError>
```

| Step | What | Bound | Failure |
|---|---|---|---|
| 0 | PR 7 guard: Host → method → `Origin` absent → Bearer → kind `hook` → framing → size → deadline (http §2.2) | parser and request deadline | §2.11 rows P and G |
| 1 | Parse `X-Chaldea-*` headers (§2.2) | — | 400 `bad_request` |
| 2 | Take body permits (§2.2) | non-blocking | 503 `busy` |
| 3 | `ctx.body_within(body, 4 MiB)` | the deadline | 408, 413, 400 |
| 4 | **Pre-read** (`WriteCtx::read`, one snapshot): `verify_hash_in` (with PR 9's hook checks, §2.8) → `Authorized { thread, launch }`; the launch row; `hook_delivery(launch, emit_id)`. An existing delivery is answered with its stored first reply now. | the deadline | 401; 503 `busy` / `shutting_down`; 500; 408 at the cutoff |
| 5 | **Intake**: under the `IntakeCounter` mutex, `ordinal = next(launch)` and `facts = seams.intake.facts(launch)` (§2.4) | non-blocking | — |
| 6 | **Ancestry walk** of the emitter pid in `spawn_blocking` (§2.5), awaited until the deadline | `max_chain` lookups; the deadline | not finished by the deadline: 408 |
| 7 | **Pid wait**, only when the launch has no recorded pid and has not exited: until `launch_started` commits or `min(intake + pid_wait, deadline)`, woken by the store's commit notifications for the thread's stream (`WriteCtx::commits`) and re-reading the launch row (a `Lagged` notification also re-reads) | 1 s, inside the deadline | none: the job decides `unbound` |
| 8 | **Delivery job** through `ctx.write` (`not_after = deadline`; §2.7) | the deadline + 100 ms reply grace | §2.11 row J |
| 9 | Reply with the delivery's first reply | — | — |

Steps 1–7 write nothing. A request whose client disconnects before step 8 is dropped by hyper with no effect beyond a consumed ordinal (gaps are allowed, store §2.3). After step 8 submits, the job runs to its end on PR 7's registered observer (http §2.3).

### 2.4 Intake: Ordinal and Facts

```rust
pub(crate) struct IntakeCounter { next: std::sync::Mutex<HashMap<LaunchId, u64>> }   // first ordinal 1
pub trait IntakeProbe: Send + Sync + 'static {
    /// The facts PR 10 needs at a hook's intake (the PTY writer's position, dispatched writes), or None
    /// (no session yet). Non-blocking: it runs under the counter's mutex.
    fn facts(&self, launch: LaunchId) -> Option<IntakeFacts>;
}
#[derive(Clone, Serialize, Deserialize)] pub struct IntakeFacts(pub serde_json::Value);   // opaque to PR 9
```

**Intake** is the instant step 5 runs: the request is complete, authenticated, and its launch known. The ordinal and the facts are taken in one critical section, so receipt order and the order of intake facts agree. The map keeps one entry per launch for the daemon run, never removed: a late hook of an exited launch must not restart at 1 (`UNIQUE (launch_id, receipt_ordinal)`, store §2.4), and a run holds few launches. A retry gets a new ordinal; a request that records nothing leaves a gap.

**What intake order means.** Buffered deliveries are promoted in receipt order (mvp §2.9). Deliveries appended directly while bound appear in the stream in commit order; their intake facts, not their stream order, are what ack attribution uses (mvp §2.3 "Ack"). A first attempt that expired before its job ran records nothing, so its retry's ordinal is the delivery's.

Default probe (`HookSeams::mvp()`): returns `None`. PR 10 supplies the real one.

### 2.5 Ancestry and Binding Rules

**Process lookup** (`pty_runtime::process`, public; PR3 D7 "the PR that first builds `extra` adds the public lookup"):

```rust
pub struct ProcessEntry { pub identity: ProcessIdentity, pub ppid: u32, pub name: String }
pub enum Lookup { Live(ProcessEntry), Gone, Unreadable }
pub fn lookup(pid: u32) -> Lookup;
pub struct Ancestry { pub links: Vec<ProcessEntry>, pub end: AncestryEnd }   // links[0] is `pid` itself
pub enum AncestryEnd { Root, Limit, Gone(u32), Unreadable(u32), ParentNewer(u32) }
pub fn ancestry(pid: u32, max: usize) -> Ancestry;
```

- `identity` uses the backend's identity format (K7), produced by the same functions as `launch_started`'s identity, so `(pid, start_time)` compares equal with it. `identity.group` is display only (macOS pgid; empty on Windows); comparisons use `(pid, start_time)` only, never `ProcessIdentity`'s derived equality.
- `name`: the image name, ASCII-lowercased, a trailing `.exe` removed. macOS: `pbi_name`, else `pbi_comm` (K6; the same `proc_pidinfo` call). Windows: `szExeFile` of the Toolhelp entry.
- Parent pid: macOS `pbi_ppid` (reparented to launchd when the parent exits, so never stale). Windows: `th32ParentProcessID` from one `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS)` per walk; it stays at the creator's pid after that creator exits, so a later process can hold it.
- **The walk** follows `ppid` from `pid` and accepts a parent only when `parent.start_time ≤ child.start_time` (both parsed as `u64`); otherwise it ends with `ParentNewer`, so a reused parent pid never extends a chain. This is the one place start times are ordered rather than compared for equality; on both OSes a parent is created before its child. It ends with `Root` at pid 0 or a self-parent, `Limit` past `max` links, `Gone` / `Unreadable` when a lookup fails (an `OpenProcess` refused for another user's process on Windows is `Unreadable`).
- The walk logic is platform-independent over a `Table` trait (`lookup` by pid) and unit-tested on every OS with a synthetic table; each OS supplies the table. `unsafe` stays in `unix::sys` and `windows::sys` (`docs/design/pty-unix.md` §2.7).

**Eligibility** (`hook/ancestry.rs`, pure; decided in the job against the launch row). Let `L` be the launch's recorded `(pid, start_time)` from `launch_started`. A chain is *eligible* for `L` when:

1. some link `k ≥ 1` has `(pid, start_time)` equal to `L`; and
2. the CLI names are `{ agent name ("claude" / "codex"), links[k].name }`; and
3. the nearest link `j ≥ 1` whose name is in the CLI names is `k`.

A nested CLI inheriting the environment puts its own process (same image) between the emitter and `L`, so rule 3 fails (B-11; K9). An empty chain (no pid header, walk failed at link 0) is never eligible. The chain is walked at intake while the emitter is still waiting for its reply, so its parent links are intact.

**Binding rule per agent** (`HookAdapter::rule`, §2.6; mvp §2.7 "Session binding", §2.9):

| Agent | `SessionStart` of an unbound launch binds when | A later hook of the bound launch is selected when | Expected session |
|---|---|---|---|
| Claude | `session_id` equals the expected session | `session_id` equals the bound session | `HookAdapter::expected_session` (§2.6) |
| Codex | the chain is eligible | `session_id` equals the bound session **and** the chain is eligible | — (the first eligible `SessionStart` names it) |

Both rows also require `X-Chaldea-Thread-Id` equal to the token's thread. Claude binds by its predetermined `--session-id` (mvp §2.9; spike §4.3); its ancestry is walked and used for descendants, not required for selection (§8 D3).

### 2.6 The Adapter Seam

```rust
pub trait HookAdapter: Send + Sync + 'static {
    fn rule(&self, agent: Agent) -> BindingRule;                                      // SessionId | Ancestry
    /// Claude's predetermined session id for this launch, from stored facts only.
    fn expected_session(&self, tx: &ReadTx<'_>, launch: &LaunchRow) -> Result<Option<String>, StoreError>;
    /// Events for one selected hook of the bound launch, decided inside the delivery or binding job.
    fn interpret(&self, tx: &ReadTx<'_>, hook: &AcceptedHook<'_>) -> Result<Interpretation, StoreError>;
}
pub enum Interpretation { Events(Vec<HookEvent>), Raw(RawUnknownReason) }       // Events non-empty
pub struct HookEvent { pub event: TypedEvent, pub name: &'static str, pub annex: Option<EventAnnex>, pub blobs: Vec<BlobInput> }
pub struct AcceptedHook<'a> {
    pub thread: ThreadId, pub launch: &'a LaunchRow, pub emit_id: Uuid, pub receipt_ordinal: u64,
    pub event_name: &'a str, pub session_id: &'a str, pub json: &'a serde_json::Value, pub body: &'a [u8],
    pub intake: Option<&'a IntakeFacts>, pub promoted: bool,       // true: applied from the buffer at binding
}
pub struct HookSeams { pub adapter: Arc<dyn HookAdapter>, pub intake: Arc<dyn IntakeProbe>, pub procs: Arc<dyn ProcessProbe> }
pub trait ProcessProbe: Send + Sync + 'static { fn chain(&self, emitter_pid: u32, max: usize) -> Ancestry; }  // blocking
```

- `interpret` and `expected_session` run on the writer thread inside the job: data only, non-blocking, no network, PTY, supervisor, or channel handle (http commitment 5). They read through the job's `ReadTx`, which sees the job's earlier appends (a promoted hook sees the events of the hooks promoted before it). Effects on the supervisor travel through the store's commit notifications.
- PR 9 builds the envelope for each `HookEvent`: stream `thread:<thread>`, `thread_id`, `launch_id`, `command_id: null`, source `{ kind: hook, agent: launch.agent, cli_version: launch.cli_version, transcript: null }`, `occurred_at: None`. Keys: the first event of a delivery `hook:<emit_id>` (mvp §2.1); every further one `derived:hook:<emit_id>:<name>` (mvp §2.1 "Derived event"). A `name` repeated within one delivery is a `KeyConflict`, which rolls the job back (500): an adapter bug, caught by its tests.
- `Interpretation::Raw(r)` stores the hook as `raw_unknown` with reason `r`.
- **`MvpHooks`** (the default in `HookSeams::mvp()` until PR 10 / 13 / 14 replace it): `rule` as in §2.5; `expected_session` = the thread's bound `session_id` when the thread has one (a resume passes `--resume <it>`), else the launch id in lowercase hyphenated form (a fresh launch passes `--session-id <launch_id>`); `interpret` = `Raw(UnknownEvent)` for every hook (no decoder exists before PR 13 / 14). The obligation this puts on PR 10 / 13 is in §4.

### 2.7 The Delivery Job

`route.rs` builds one closure capturing only data: the parsed headers, the body bytes, the chain, the ordinal, the intake facts, `Arc<dyn HookAdapter>`, and `HookConfig`. It runs inside `ctx.write`, after `verify_hash_in` (§2.8):

1. **Dedup.** `tx.read().hook_delivery(launch, emit_id)`: present → return its first reply; nothing else runs.
2. **Expire.** `buffer::expire(tx, launch, cfg)` (§2.9) turns a buffer whose oldest delivery is older than `buffer_ttl` into `raw_unknown` (`unbound`).
3. **Descendants.** When the chain contains `L`'s identity (rule 1 of §2.5, whatever the session and the other rules), `tx.descendants_record(launch, links[1..k])` records the links strictly between the emitter and `L` (§2.12).
4. **Decide** the routing by the table below, first matching row.
5. **Record**: `tx.hook_delivery_record(NewHookDelivery { launch, emit_id, receipt_ordinal, first_reply: 200 "", routing })`.
6. **Promote** (binding only): `tx.read().buffered_deliveries(launch)` in receipt order; each one is decided by the promotion table and passed to `hook_delivery_promote` in that order.
7. Return the first reply.

`D` is `payload::decode(body)`; `L` the launch row; `T` the thread row.

| # | Condition | Routing |
|---|---|---|
| 1 | `X-Chaldea-Body-Truncated` present, or `D` is not JSON (invalid JSON or invalid UTF-8) | `raw_unknown` `unparseable` |
| 2 | `D` is JSON but not an object with a string `hook_event_name` (1–128 bytes) and a string `session_id` (1–256 bytes); or a `SessionStart` whose `transcript_path` is present and neither a string nor `null` | `raw_unknown` `schema_mismatch` |
| 3 | `L` is not `T.current_launch_id` | `raw_unknown` `unbound` |
| 4 | `X-Chaldea-Thread-Id` absent or not the token's thread | `raw_unknown` `foreign_session` |
| 5 | `L` bound to session `S`: `D.session_id ≠ S` (a conflicting `SessionStart` included, §8 D2), or the rule is `Ancestry` and the chain is not eligible | `raw_unknown` `foreign_session` |
| 6 | `L` bound: otherwise | `interpret` → `appended` (its events) or `raw_unknown` (its reason) |
| 7 | `L` unbound and exited (`exit_reason` set), or without a recorded pid | `raw_unknown` `unbound` |
| 8 | `L` unbound, `SessionStart`, the rule's condition (§2.5) holds, and `reduce(T.state, SessionBound)` accepts | **bind**: `appended` with `session_bound { launch_id, session_id, transcript_path }`, one `status_changed` per `StatusChanged` the reducer returned (names `status:<i>`), then the events of `interpret` for this `SessionStart` (if it returns `Raw`, the binding still stands and its events are only the first two groups); then step 6 |
| 9 | `L` unbound, `SessionStart`, the rule's condition holds, `reduce` refuses | `raw_unknown` `unbound` |
| 10 | `L` unbound, `SessionStart`, otherwise | `raw_unknown` `foreign_session` |
| 11 | `L` unbound, any other event, selected for the launch (Claude: `session_id` equals the expected session; Codex: the chain is eligible) | `buffered` if the bounds hold after adding it (§2.9), else **overflow**: every buffered delivery of `L` is promoted to `raw_unknown` `unbound` in receipt order, and this one is recorded as `raw_unknown` `unbound` |
| 12 | `L` unbound, any other event, otherwise | `raw_unknown` `foreign_session` |

**Promotion table** (step 6, each buffered delivery `B` read back with its meta, §2.9):

| Condition | Promotion |
|---|---|
| `B.session_id ≠` the bound session, or its thread header is not the token's thread, or (rule `Ancestry`) its stored chain is not eligible for `L` — the ancestry evaluated at receipt, checked again against `L` here (mvp §2.9) | `RawUnknown` `foreign_session` |
| otherwise | `interpret` with `promoted: true` and `B`'s stored intake facts → `Appended(events)` or `RawUnknown(reason)` |

**`raw_unknown` of a hook**: stream `thread:<thread>` (the token names the thread, so a hook is never "unbound to a thread" in mvp §2.1's stream rule), `launch_id` = the token's launch, key `hook:<emit_id>`, payload `{ source: hook, reason, agent: launch.agent, cli_version: launch.cli_version, content_type: "application/json" when rule 1 did not apply else "application/octet-stream", raw: prepare_content(body) }`. The raw bytes are the request body exactly (`Content` inline UTF-8, base64 for invalid UTF-8, a blob above 64 KiB; store §2.5).

**Why the order of rows is safe.** Rows 1–2 keep anything PR 9 cannot read as a hook, before any session logic. Row 3 keeps a superseded launch's late hooks out of the current launch's state. Rows 5 and 10–12 are session selection: a hook of another session never reaches `interpret` (the Quint model takes in only hooks of the selected session, formal §1). Row 7 is "a launch that can no longer bind". Row 8 is the only place a launch binds, and the store trigger `events_session_bound_once` (§2.12) refuses a second binding even if a bug skipped row 5.

### 2.8 Token Check

`verify_hash_in` (http §2.4) gains, for kind `hook`: the token has both `thread_id` and `launch_id`, the launch exists, and its thread is the token's thread; otherwise `Invalid` (401). The launch need not be current, running, or bound: late hooks are kept (rows 3, 7), and revocation (PR 10 at restart, §2.11 close) ends a token. The guard's step-4 lookup, the pre-read, and the job each run it, so a revocation commits before or after each in the writer's order.

### 2.9 Buffer

A launch's buffer is its deliveries with routing `buffered`; nothing else holds it.

```rust
pub struct HookConfig {
    pub pid_wait: Duration,          // 1 s: early hooks wait this long, inside the deadline, for launch_started
    pub max_chain: usize,            // 16 links walked from the emitter
    pub buffer_max_count: usize,     // 32 buffered deliveries per launch
    pub buffer_max_bytes: u64,       // 8 MiB of buffered bodies per launch
    pub buffer_ttl: Duration,        // 30 s from the oldest buffered delivery's received_at
    pub body_budget: u64,            // 32 MiB of hook bodies held by requests at once
    pub max_descendants: usize,      // 1024 attributed identities per launch
}
pub fn expire(tx: &mut WriteTx<'_>, launch: LaunchId, cfg: &HookConfig) -> Result<usize, StoreError>;
pub fn resolve_unbindable(tx: &mut WriteTx<'_>, launch: LaunchId) -> Result<usize, StoreError>;
```

- **Meta** (`BufferedHook.meta`, JSON, version 1): `{ "v": 1, "thread_header", "agent_header", "emitter_version", "chain": [{ "pid", "start_time", "name" }], "chain_end", "intake": <IntakeFacts or null> }`; `BufferedHook.body` is the request body. A meta that does not parse at promotion (only a bug or a foreign writer can cause it) promotes to `raw_unknown` `schema_mismatch` with the body as raw.
- **Bounds**, checked in the job that would add a delivery: the count after adding ≤ 32 and the summed body bytes after adding ≤ 8 MiB; otherwise overflow (row 11).
- **Time.** `expire` compares the oldest delivery's `received_at` with the job's `recorded_at` (both the store clock, RFC 3339 ms) and, past `buffer_ttl`, promotes every buffered delivery of the launch to `raw_unknown` `unbound` in receipt order. It runs at the start of every delivery job of that launch (step 2) and in PR 10's timer tick. A wall-clock step can expire a buffer early or late; either way it ends as `raw_unknown`, never as a wrong binding.
- **Launch that can no longer bind.** `resolve_unbindable` promotes every buffered delivery of the launch to `raw_unknown` `unbound`. PR 10 calls it in the job that appends `launch_exited`, and at restart for each of `buffered_launches()` before serving (mvp §2.9; formal `restart`). Row 7 covers a hook arriving after that.
- After an overflow or expiry the buffer is empty and later unbound hooks buffer again; if the launch then binds, only those are promoted. The earlier ones stay `raw_unknown`, visible, never applied out of order.

### 2.10 Single Deciding Points

| Operation several requests can hit at once | Decided by | Why a second caller cannot act |
|---|---|---|
| Duplicate `(launch_id, emit_id)` (a retry racing its first attempt) | the delivery job's step 1 + `hook_delivery_record`, in one `BEGIN IMMEDIATE` transaction on the one writer | jobs run one at a time, so the second reads the first's row and replays its reply; `PRIMARY KEY (launch_id, emit_id)` refuses a second process's insert. The pre-read's replay is a shortcut, never the decision |
| Two eligible `SessionStart`s of one launch (Codex runs hooks concurrently) | the job reading `L.session_id IS NULL` and appending `session_bound` | the second job reads the binding the first committed and takes row 5; trigger `events_session_bound_once` refuses a second `session_bound` for the launch |
| The first receipt ordinal | `IntakeCounter`'s mutex: read and increment in one critical section | each caller gets a distinct value; `UNIQUE (launch_id, receipt_ordinal)` is the backstop |
| Intake facts against the ordinal | the same critical section | facts and ordinals are taken in one order |
| Buffer bounds and expiry | the job reading `buffered_deliveries(launch)` | the count and bytes include every committed delivery; a concurrent delivery is a later job |
| Promotion against overflow, expiry, `resolve_unbindable` | whichever job runs first | the trigger allows `buffered → terminal` once (store §2.4); the later job reads no buffered rows |
| Binding against the launch's exit | the job: `reduce(SessionBound)` and row 7 read the projection that `launch_exited` updates in its own job | the writer orders them; a binding after exit is refused by row 7 or by the reducer |
| Descendant records | the job, `INSERT … ON CONFLICT DO NOTHING` on `PRIMARY KEY (launch_id, pid, start_time)`, cap counted in the same transaction | concurrent chains of one launch insert each identity once |
| Revocation against a hook | `verify_hash_in` inside the job | committed before or after in the writer's order |
| Body budget | `Semaphore::try_acquire_many_owned` | permits are atomic |

### 2.11 Status and Disposition Table

What the emitter observes per outcome, the layer that decides it, and whether the emitter retries (§2.13). Every row except J-ok stores nothing: no delivery, no event, no descendant, no projection change.

| Row | Outcome | Layer | Client observes | Emitter |
|---|---|---|---|---|
| P1 | `Content-Length: +5`; two different `Content-Length`; HTTP/1.0 with `Transfer-Encoding` | hyper | 400, no `ErrorBody` | final |
| P2 | head > 8 KiB or > 64 headers | hyper | 431 | final |
| P3 | head not complete within `header_read_timeout` (5 s) | hyper | EOF with no bytes | retry |
| P4 | two equal `Content-Length` headers | hyper keeps one (K1) | as a single header: the request proceeds | — |
| G1 | `Host` missing, repeated, or not `127.0.0.1:<port>` / `localhost:<port>` | guard step 1 | 421 `misdirected_request` | final |
| G2 | method other than `POST` / path other than `/hook` | guard 2 / router | 405 with `Allow: POST` / 404 | final |
| G3 | `Origin` present | guard 3 | 403 `origin_rejected` | final |
| G4 | `Authorization` missing, repeated, not `Bearer`, malformed, unknown, revoked; a `hook` token failing §2.8 | guard 4 | 401 `unauthorized` | final |
| G5 | a valid token of another kind | guard 5 | 403 `forbidden` | final |
| G6 | any `Transfer-Encoding` (with or without `Content-Length`) | guard 6 | 400 `bad_request` | final |
| G7 | no `Content-Length` | guard 6 | 411 `length_required` | final |
| G8 | `Content-Length` > 4 MiB | guard 7 | 413 `payload_too_large` | final |
| G9 | deadline passed before the handler | guard 9 | 408 | retry |
| H1 | `X-Chaldea-Emit-Id` missing, repeated, or not a UUID; another `X-Chaldea-*` header repeated | handler 1 | 400 `bad_request` | final |
| H2 | body budget exhausted | handler 2 | 503 `busy` | retry |
| H3 | body not complete by the deadline (held or slow) | handler 3 | 408, or a reset when body bytes are still in flight (K2) | retry |
| H4 | body ended early (client closed) | handler 3 | — (client gone) | — |
| H5 | pre-read: token invalid now / reader busy / store closed / other store error / cut off | handler 4 | 401 / 503 `busy` / 503 `shutting_down` / 500 / 408 | final / retry / retry / final / retry |
| H6 | walk not finished by the deadline | handler 6 | 408 | retry |
| J-ok | the job recorded a delivery, or replayed one | job | 200, empty body | final |
| J1 | `Expired` (the deadline passed before the job held the lock) | job | 408 | retry |
| J2 | `Busy`, `SubmitError::QueueFull` | job | 503 `busy` | retry |
| J3 | registry closing / `SubmitError::Closed` | observer / submit | 503 `shutting_down` | retry |
| J4 | `Rejected(401)` (revoked between steps) | job | 401 | final |
| J5 | `RolledBack(DiskFull)` | job | 503 `disk_full` | retry |
| J6 | `RolledBack(other)`, `Panicked`, `Rejected(Store)` | job | 500 `internal` | final |
| J7 | `CommitUnknown`, `WriterGone` (reported to `LifecycleSink`), or no reply by `deadline + 100 ms` | job | 503 `outcome_unknown` | retry (same `emit_id`: dedup replays a delivery that did commit) |
| J8 | `Failed`, `SubmitError::Failed` (reported) | job | 503 `store_failed` | retry |
| — | client disconnect after submission | observer | — | — |

A rejection decided before the body (rows G1–G8, H1, H2) is sent before the handler reads any body byte (http §2.2 "The body boundary"): a client that withholds its body until it reads the response gets the status (the ordering cases of §7). With body bytes in flight, a client may observe a reset instead of the status (K2); B-8 records which (§7.1).

### 2.12 Descendant Attribution (PR 9 / 10)

A matching creation time identifies a process, not its provenance (`docs/design/pty-windows.md` §2.5), so `extra` must contain only processes proven to descend from the launch. PR 9 proves it at the one moment it can: the walk at intake follows parent links upward from a live emitter to `L`, each link checked by `ParentNewer` (§2.5). The links strictly between the emitter and `L` are then descendants of `L` at that instant, recorded with their identities. The emitter itself is not recorded (it exits within 3 s). A chain that does not reach `L` records nothing.

**Migration v2** (the next free `MIGRATIONS` index at merge; PR 7c or PR 8 may take one first):

```sql
CREATE TABLE launch_descendants (
  launch_id   TEXT NOT NULL,
  pid         INTEGER NOT NULL CHECK (pid > 0),
  start_time  TEXT NOT NULL CHECK (length(start_time) > 0),
  grp         TEXT NOT NULL, name TEXT NOT NULL,
  recorded_at TEXT NOT NULL,
  PRIMARY KEY (launch_id, pid, start_time)
) STRICT, WITHOUT ROWID;
-- update and delete refused by trigger (RAISE(ROLLBACK)), as for hook_deliveries
CREATE TRIGGER events_session_bound_once BEFORE INSERT ON events WHEN NEW.kind = 'session_bound' BEGIN
  SELECT RAISE(ROLLBACK, 'events_session_bound_once: a second binding or a binding of another launch')
  WHERE EXISTS (SELECT 1 FROM events WHERE stream_id = NEW.stream_id AND kind = 'session_bound' AND launch_id IS NEW.launch_id)
     OR NEW.launch_id IS NOT (SELECT launch_id FROM events WHERE stream_id = NEW.stream_id AND kind = 'launch_reserved'
                              ORDER BY seq DESC LIMIT 1);
END;
```

```rust
impl WriteTx<'_> { pub fn descendants_record(&mut self, l: LaunchId, ids: &[ProcessEntry], cap: usize) -> Result<u64, StoreError>; }
impl ReadTx<'_>  { pub fn descendants(&self, l: LaunchId) -> Result<Vec<ProcessIdentity>, StoreError>; }
```

`descendants_record` inserts each identity not yet present while the launch has fewer than `cap` rows (counted in the transaction); beyond the cap it inserts nothing and returns the number inserted. It is not a projection (like `hook_deliveries`, store §2.1 "Projections"); rows are kept forever (store D9). PR 10 passes `descendants(launch)` as the full `extra` list on every `kill_tree` (mvp §4 PR 10). The trigger keeps `session_bound` once per launch and only for the stream's current launch (the latest `launch_reserved`), reading `events` only, so it holds while projections are dropped.

### 2.13 The Emitter: `chaldea hook-emit <agent>`

**Dispatch.** `main` compares `argv[1]` with `hook-emit` before any runtime, logging, or configuration initialization and calls `hook_emit::main(argv)`, which never returns (mvp §3 "Single binary"). The command form is fixed: `<absolute path to chaldea> hook-emit <agent>`, nothing else, because Codex hashes the command string into its hook trust (arch §5; §8 D1).

**Environment** (mvp §2.5 "Token kinds"): `CHALDEA_HOOK_URL` (exactly `http://127.0.0.1:<port>/hook` or `http://localhost:<port>/hook`, port 1–65535 with no leading zero; the spike's `parse_local_url` rules, K8), `CHALDEA_HOOK_TOKEN` (`chd_` + 43 base64url characters), `CHALDEA_THREAD_ID` (sent as `X-Chaldea-Thread-Id` when set). A missing or malformed URL or token: exit 0 at once without reading stdin. No other variable is read; no log file (§8 D8).

**Run.**

1. Install a panic hook that calls `std::process::exit(0)` without printing; start a watchdog thread that calls `exit(0)` at `start + 3 s`.
2. Read stdin on a thread, up to 4 MiB + 1 bytes, until the budget: EOF in time → the body; more than 4 MiB → its first 4 MiB and `X-Chaldea-Body-Truncated: 1`; still open at the budget, or a read error → exit 0 with no request.
3. `emit_id = Uuid::new_v4()` once.
4. Up to **3 attempts**, each only if at least 300 ms of the budget remain; before attempt 2 sleep 100 ms, before attempt 3 sleep 250 ms. An attempt: `TcpStream::connect_timeout(127.0.0.1:<port>, min(1 s, remaining − 50 ms))`, write the head and body with write timeouts from the remaining budget, read until the first CRLF (at most 1 KiB) with read timeouts from the remaining budget, parse `HTTP/1.x <3 digits>`.
5. **Retry on**: a connect error or timeout; a write or read error, a reset, or EOF before a complete status line (the request may or may not have been recorded; the same `emit_id` makes a retry safe); status 408 or any 503. **Stop on** any other status.
6. `exit(0)`.

Nothing is written to stdout or stderr on any path (Claude injects hook stdout into context, and exit 2 would block the prompt; spike §4.3). `CL-23` holds by construction: the watchdog bounds every path to 3 s, and exit code 0 is the only one used, panics included.

**Windows.** The same console-subsystem `chaldea.exe` (prior-art row 9; spike C-5 `go`: no window for hook launches); it inherits the CLI's console and creates none. A connect to a closed loopback port can take about 2 s to be refused on Windows (A2); the 1 s connect bound keeps that inside the budget. `exit` from the watchdog ends a main thread blocked in `connect`, `send`, or `recv`. No `cfg` in the emitter.

### 2.14 FakeCli Additions (9a)

- `echo --emitter <path> [--agent <claude|codex>] [--session-id <id>] [--nested]`: at start, runs the emitter once (`<path> hook-emit <agent>`, agent default `codex`) with `{ "hook_event_name": "SessionStart", "session_id": <id or a random UUID>, "transcript_path": null }` on stdin, and per input line a `UserPromptSubmit` with the line as `prompt`. `--nested` starts a nested FakeCli (`echo --nested-child`, the same binary, inheriting the environment) that emits a `SessionStart` with another session id before and after the parent's (spike `src/fake_cli.rs:244-269`, B-11).
- `hooks --emitter <path> [--agent <claude|codex>] --script <json array of payloads> [--concurrent]`: runs each payload through its own emitter process, in order (waiting for each to exit) or all at once, and reports each emitter's exit code, elapsed time, and stdout length on the report stream.

FakeCli stays free of workspace dependencies other than `core` (mvp §3); it starts the emitter with `std::process::Command`.

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **Where routing is decided** | A. In the handler with in-memory launch state | spike `receiver.rs` (a `Mutex` over bound session and events) | Rejected: cross-call state outside the database; two concurrent `SessionStart`s need one decider. |
| | B. One store job per delivery reading the projections (§2.7) | store §2.3 "Hook deliveries" | **Adopted.** |
| **Ancestry source** | A. The chain the emitter reports in a header | spike `X-Chaldea-Emitter-Ancestry` (K9) | Rejected: the daemon would trust a chain it never observed, and `extra` needs provenance the daemon proved (§2.12). |
| | B. The daemon walks from the emitter's pid at intake | — | **Adopted**; the pid header is the only emitter-reported input, and a wrong pid yields an ineligible chain. |
| | C. The peer pid of the TCP connection | Windows `GetExtendedTcpTable` | Rejected: macOS has no TCP peer-pid query (`LOCAL_PEERPID` is for Unix sockets); one mechanism per OS would differ. |
| **Walk location** | A. In `chaldea-daemon` | — | Rejected: `unsafe` is forbidden there, and OS code belongs in `pty-runtime` (AGENTS.md). |
| | B. Public `pty_runtime::process` | PR3 D7 | **Adopted.** |
| **Windows parent pid** | A. One Toolhelp snapshot per walk | spike `src/os/windows.rs:126-128` | **Adopted.** |
| | B. `NtQueryInformationProcess(ProcessBasicInformation)` | — | Rejected: an `ntdll` call and an open handle per ancestor; same staleness. |
| **Pid reuse in a chain** | A. Trust `ppid` | — | Rejected: a stale Windows `ppid` can name a later process. |
| | B. Accept a parent only if created no later than its child | — | **Adopted.** |
| **Receipt ordinal** | A. Assigned in the record job (commit order) | — | Rejected: a slow walk or pid wait reorders it (store §3 "Hook order"). |
| | B. Per-launch counter at intake under a mutex | store §2.3 | **Adopted.** |
| | C. Counter keyed by token | — | Rejected: correctness would rest on one hook token per launch. |
| **Intake facts** | A. Recomputed at promotion | — | Rejected: the model applies buffered hooks with the facts of their intake (K10); later writes would change them. |
| | B. PR 10's probe at intake, stored opaque in the buffered meta | — | **Adopted.** |
| **Hook meaning** | A. PR 9 decodes every kind | — | Rejected: adapter knowledge (PR 13 / 14) and supervisor facts (PR 10) are not PR 9's. |
| | B. `HookAdapter` seam; PR 9 decodes only the binding fields | — | **Adopted.** |
| **Claude's expected session** | A. An annex on `launch_reserved` | store annex | Rejected for now: a store change PR 9 cannot fill, since PR 10 writes `launch_reserved`. |
| | B. Derived from stored facts through `HookAdapter::expected_session` | — | **Adopted**; PR 10 may replace the default with a stored value. |
| | C. Claude binds like Codex (ancestry only) | — | Rejected: mvp §2.9 binds Claude by `--session-id`, the verified rule (spike §4.3). |
| **Buffer expiry** | A. A daemon timer task per launch | — | Rejected: a task holding state the database holds. |
| | B. `expire` in each delivery job and in PR 10's tick; `resolve_unbindable` at exit and restart | — | **Adopted.** |
| **Buffer overflow** | A. Refuse the new hook (503) | — | Rejected: the emitter would retry into the same overflow and lose it. |
| | B. The whole buffer and the new hook become `raw_unknown` `unbound` | mvp §2.9 | **Adopted.** |
| **Conflicting `SessionStart`** | A. Rebind the launch | — | Not in the MVP (§8 D2). |
| | B. `raw_unknown` `foreign_session`, binding unchanged | spike M3 (ambiguity never re-binds) | **Adopted.** |
| **Emitter HTTP client** | A. `minreq` | prior-art row 5b | Rejected: one timeout for connect and read. |
| | B. Hand-written std POST | spike `http_client.rs` | **Adopted.** |
| **Emitter retries** | A. None | spike `emitter.rs` | Rejected: a busy writer or a reset loses the hook. |
| | B. Up to 3 attempts, one `emit_id`, retry on transport errors, 408, and 503 only | — | **Adopted.** |
| | C. Retry every non-200 | — | Rejected: 4xx are final; retrying them only spends the budget. |
| **Stdin over 4 MiB** | A. Drop it (no request) | spike B-9 | Rejected: a hook silently dropped (arch §1). |
| | B. Send the first 4 MiB marked truncated; keep it as `raw_unknown` `unparseable` | — | **Adopted** (§8 D5). |
| **Large in-flight body on rejection (H5)** | A. Drain the body after an early rejection (lingering close) | spike F-3c | Rejected: a change to PR 7's connection handling for one route whose only client retries and needs no status. |
| | B. Record the observation per OS; the emitter treats a reset as retryable | — | **Adopted.** |
| **Descendant storage** | A. In memory | — | Rejected: lost at restart, when PR 10 needs it most. |
| | B. An annex on the hook's event | — | Rejected: buffered and foreign deliveries have no event, yet their chains prove descendants. |
| | C. A non-projection table written in the delivery job | `hook_deliveries` | **Adopted.** |
| **Hook body memory** | A. Only the 4 MiB per-request cap | — | Rejected: 128 connections × 4 MiB. |
| | B. A 32 MiB byte budget, 503 `busy` when full | http blob slots | **Adopted.** |
| **Stream of a hook `raw_unknown`** | A. `host` when the launch is unbound | — | Rejected: the token names the thread, and the host stream is unreadable to agents. |
| | B. Always the token's thread | — | **Adopted.** |
| **Emitter crate** | A. A module of `crates/cli` | mvp §3 | **Adopted**; 9a creates the crate if PR 8 has not. |
| | B. A separate crate | — | Rejected: one binary either way; no isolation gained. |
| **PR size** | A. One PR | mvp §4 | Rejected: large, and the emitter is testable alone. |
| | B. 9a (process lookup, emitter, FakeCli) and 9b (ingest, store v2, B-8, e2e) | http D1 | **Adopted.** |

## 4. Same-Class Inventory

Every later PR that consumes hook facts or adds hook kinds, and the seam it uses. None changes PR 9's types.

| PR | Consumes or adds | Seam | What PR 9 provides; what that PR must do |
|---|---|---|---|
| 10 supervisor | launch reservation and tokens; `extra`; readiness, gate, ack; restart; timers | `IntakeProbe`, `HookAdapter` (wraps the adapters' decoders with supervisor facts), `buffer::expire`, `buffer::resolve_unbindable`, `ReadTx::descendants`, `launch_started` | Mints one hook token per launch with `thread_id` and `launch_id` (§2.8) and sets `CHALDEA_HOOK_URL`, `CHALDEA_HOOK_TOKEN`, `CHALDEA_THREAD_ID`. For Claude passes `--session-id <launch_id>` on a thread without a bound session and `--resume <thread session>` otherwise, or replaces `expected_session` with a stored value. Records `launch_started` with the backend's identity (the pid wait ends on it). Calls `resolve_unbindable` in the `launch_exited` job and at restart for every `buffered_launches()` launch before serving, and `expire` on its tick. Passes `descendants(launch)` on every `kill_tree`. Consumes: `session_bound` (and its `status_changed` clearing startup causes), each selected hook through `interpret` with intake facts, `raw_unknown` (no state effect). |
| 12 fixtures | real payloads per CLI version | `AcceptedHook.body` / `json` in `interpret`; `raw_unknown.raw` | A capture launch wraps the adapter to copy each accepted body; foreign and unparseable bodies are `raw_unknown` events with the exact bytes. |
| 13 Claude | decoders: `UserPromptSubmit`, `Stop`, `SessionEnd`, `PermissionRequest`, `PreToolUse`, `PostToolUse`, `Notification`; `SessionStart` readiness | `HookAdapter::interpret` for `Agent::Claude` | `SessionStart` reaches `interpret` once after `session_bound` (row 8) and again for every later same-session `SessionStart` (row 6). Verifies Claude's ancestry on real hosts before any switch to rule `Ancestry` (§8 D3), and `--resume` keeping the session id (A1; §2.6). |
| 14 Codex | decoders including `Interrupt`; command string and stable emitter path | `HookAdapter::interpret` for `Agent::Codex`; §2.13 command form | Registers `<stable path> hook-emit codex`; the path must not change across upgrades, or trust renews (arch §5). Launches the native `codex` process itself, so that the CLI process running hook commands is the launched pid (a wrapper process in between fails eligibility). |
| 20 extensions | `attention` on raise and clear; rescue reads `raw_unknown` | store commit notifications; thread streams | The binding's `status_changed` events are ordinary events; `raw_unknown` of hooks is on the thread stream. |
| 21 doctor | hook round trip through `hook-emit` | the public route and env | The emitter exits 0 silently by design, so the doctor observes the daemon side (a delivery recorded), not the emitter's exit. |
| 22 fuzz | the hook payload decoder | `hook::payload::decode(&[u8]) -> Decoded` (pure, no I/O) | A fuzz target that never panics and classifies every input as one of `Hook`, `Unparseable`, `SchemaMismatch`. |

## 5. Resource Paths

| Resource | Created by | Bound | Ends on |
|---|---|---|---|
| Connection | PR 7 accept loop | 128, one request each | PR 7 (http §5) |
| Request body in memory | handler step 3 | 4 MiB each; 32 MiB of permits across requests | the reply; the request dropped; permits released with it |
| Walk (`spawn_blocking`) | handler step 6 | 16 lookups; Windows one snapshot | returns on its own; the handler stops waiting at the deadline (408) |
| Pid wait | handler step 7 | `min(intake + 1 s, deadline)` | `launch_started` observed; the bound; the request dropped |
| Commit receiver | handler step 7 | the store's broadcast | dropped with the wait |
| Delivery job and its observer | handler step 8 through `ctx.write` | one per request | the writer's reply (store §5); PR 7's shutdown |
| `IntakeCounter` entry | first hook of a launch | one `u64` per launch per daemon run | daemon exit |
| Buffered delivery | row 11 | 32 and 8 MiB per launch; 30 s | binding (promotion), overflow, expiry, `resolve_unbindable` at exit or restart |
| Descendant row | job step 3 | 1024 per launch | kept (store D9) |
| Emitter process | the CLI, per hook | 3 s | `exit(0)` after the reply or a final status, or the watchdog |
| Emitter stdin thread | emitter step 2 | 4 MiB + 1 bytes | EOF, error, or the process exit |

**Endings of a hook request** (each row of §2.11 is one): a hyper parse rejection or slow-head EOF; a guard rejection; a header rejection; the body budget; the body deadline, size, or early end; a pre-read failure or replay; the walk cut by the deadline; the job's outcome (recorded, replayed, expired, busy, refused, rolled back, panicked, commit unknown, writer gone, failed, no reply by the grace); a client disconnect before submission (nothing written) or after it (the job finishes on its observer); daemon shutdown (registration refused → 503 `shutting_down`; an admitted job is answered by the writer, http §5).

**Waits.** Head: hyper's `header_read_timeout`. Body, pre-read, walk, pid wait, job admission: the request deadline (5 s from the first head byte). Job reply: deadline + 100 ms. Pid wait: also 1 s from intake. The intake mutex: a map operation and a non-blocking probe. The emitter: 3 s wall clock from its start, each socket operation bounded by the remainder.

**Windows and macOS.** The daemon side has no `cfg`: OS differences are in `pty_runtime::process` (Toolhelp vs `proc_pidinfo`) and in K2's reset behaviour, which B-8 records per OS.

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked on a running daemon (PR 8's binary once it exists; until then the test server of `crates/daemon/tests/support` with the hook mount, or the server built in `crates/cli/tests/hook_e2e.rs`). `$O` is an owner token, `$H` a launch's hook token, `$P` the port.

| ID | Criterion (EARS) | Source | How to observe |
|---|---|---|---|
| PR9-P-1 | When a Codex-shaped launch's CLI fires `UserPromptSubmit` before `SessionStart` through `chaldea hook-emit`, the thread stream shall show `session_bound` followed by the earlier hook's events, in the order the daemon received them. | mvp §2.9; K11 | FakeCli `hooks` with the order `UserPromptSubmit, SessionStart` and a test adapter; `GET /api/events?stream_id=thread:<t>` with `$O`. |
| PR9-P-2 | If a nested CLI inheriting the launch's environment emits hooks, then the launch shall stay bound to its own session and the nested hooks shall appear as `raw_unknown` `foreign_session`. | prior-art row 6c; spike B-11 | FakeCli `echo --nested`; the stream. |
| PR9-P-3 | If a `/hook` request is malformed, unauthenticated, misdirected, or too large, then the daemon shall reject it and the thread and host streams and the delivery table shall not change. | invariant 5; spike B-8 | `curl` probes of §2.11; `head_seq` of both streams before and after. |
| PR9-P-4 | When `chaldea hook-emit` runs, it shall exit 0 within 3 s with empty stdout, whether the daemon is down, slow, rejecting, or accepting. | arch §2; CL-23 | `time chaldea hook-emit codex < payload.json > out; echo $?` against each daemon condition. |
| PR9-P-5 | When the same `X-Chaldea-Emit-Id` is posted twice, the thread shall show one event. | mvp §2.1 | Two `curl` posts with the same header; the stream. |
| PR9-P-6 | If a hook body is not JSON or lacks the hook fields, then the thread shall show it verbatim as `raw_unknown`. | arch §1; P-14 | Post `not json` and `{"x":1}`; `GET /api/blobs` or the inline `raw`. |
| PR9-P-7 | When the B-8 suite runs on `windows-latest` and `macos-latest`, every case shall end with its listed observation, and the recorded per-OS outcomes of the in-flight-body cases shall be in the CI log. | arch §5 open item | The `workspace` job's test output (§7.1). |

### 6.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Test that falsifies it |
|---|---|---|---|
| PR9-CL-1 | When a hook delivery is recorded, its first reply, receipt ordinal, and routing shall commit in one job before the reply is sent, and a retry with the same `(launch_id, emit_id)` shall get the first reply and change nothing. | mvp §4 PR 9; CL-10; store §2.3 | `hook_ingest::retry_replays_first_reply` (one event, routing and reply unchanged after the second post); `…::reply_waits_for_commit` (writer held: no reply until released) |
| PR9-CL-2 | A retry of a buffered delivery after its launch became bound shall get the first reply, and the delivery's routing shall be the promoted one, with no new event. | mvp §4 PR 9 | `hook_ingest::retry_after_binding_keeps_first_reply` |
| PR9-CL-3 | Distinct `emit_id`s with identical bodies shall each create a delivery. | CL-10 | `hook_ingest::identical_bodies_are_two_deliveries` |
| PR9-CL-4 | A buffered hook shall be durable before 200 is returned. | mvp §4 PR 9; store D5 | `hook_ingest::buffered_durable_before_200` (with the writer held, no 200 arrives; after its release the 200 arrives and a read sees the buffered row; durability of a committed job is store D5's, tested by PR 6) |
| PR9-CL-5 | Concurrent hooks of one launch shall receive distinct receipt ordinals in the order of their intake, with intake facts taken in the same order. | store §2.3; §2.10 | `hook_ingest::concurrent_ordinals_unique` (64 concurrent posts, a probe recording its call order) |
| PR9-CL-6 | Of concurrent eligible `SessionStart`s of one launch, exactly one shall bind; the others shall be `raw_unknown` `foreign_session` when their session differs, or interpreted when it is equal. | mvp §2.9; §2.10 | `hook_ingest::concurrent_session_starts_bind_once`; unit `store triggers::session_bound_once` (a raw second insert refused) |
| PR9-CL-7 | A launch shall bind only through §2.5's rule: Claude by its expected session, Codex by an eligible chain; a nested CLI inheriting the environment shall not bind. | CL-17; prior-art row 6c | `hook_ingest::claude_binds_by_expected_session`, `…::codex_binds_by_ancestry`, `…::nested_chain_does_not_bind` (synthetic chains); `hook_e2e::nested_fake_cli_does_not_bind` (real processes, both OSes) |
| PR9-CL-8 | A later hook of a bound Codex launch shall be selected only when its chain is eligible at its own receipt. | mvp §4 PR 9 | `hook_ingest::later_hooks_revalidate_ancestry` |
| PR9-CL-9 | A `SessionStart` with another session for a bound launch shall be `raw_unknown` `foreign_session` and leave the binding unchanged. | mvp §4 PR 9 | `hook_ingest::conflicting_session_start_is_foreign` |
| PR9-CL-10 | A hook whose session is not the launch's selected session shall never reach `interpret` and shall be `raw_unknown` `foreign_session`. | formal §1; mvp §2.9 | `hook_ingest::foreign_session_never_interpreted` (a counting adapter) |
| PR9-CL-11 | The intake of a hook of an unbound launch shall append no event and change no projection. | formal §3 hole d | `hook_ingest::unbound_intake_changes_nothing` (all `proj_*` rows and heads equal before and after) |
| PR9-CL-12 | At binding, the buffered hooks shall be applied after `session_bound` in receipt order, each with its own intake facts, and only those whose session equals the bound one and whose stored chain is eligible for the launch. | mvp §2.9; K10 | `hook_ingest::promotion_in_receipt_order` (receipt ordinals 1, 2 committed 2, 1), `…::promotion_carries_intake_facts`, `…::promotion_rechecks_ancestry` (a stored chain naming another start time is promoted to `foreign_session`) |
| PR9-CL-13 | A buffer that would exceed 32 deliveries or 8 MiB, or whose oldest delivery is older than 30 s, shall become `raw_unknown` `unbound` as a whole, in receipt order. | mvp §2.9 | `hook_ingest::buffer_count_overflow`, `…::buffer_bytes_overflow`, `…::buffer_ttl_expires` (store clock advanced) |
| PR9-CL-14 | A hook of a launch that is not the thread's current launch, or that is unbound and exited or has no recorded pid by the end of its wait, shall be `raw_unknown` `unbound`; `resolve_unbindable` shall turn a launch's buffer into `raw_unknown` `unbound`. | mvp §2.9 | `hook_ingest::not_current_launch_is_unbound`, `…::exited_unbound_is_unbound`, `…::pid_wait_expires_unbound`, `…::resolve_unbindable_clears_buffer` |
| PR9-CL-15 | When `launch_started` commits during a hook's pid wait, the hook shall be decided against the recorded pid. | mvp §2.9 "Early hooks" | `hook_ingest::pid_wait_then_bind` (`launch_started` appended 200 ms after the post) |
| PR9-CL-16 | A body that is not JSON, or is truncated, shall be `raw_unknown` `unparseable`; JSON without the hook fields shall be `schema_mismatch`; both with the exact bytes. | arch §1; P-14 | `hook_ingest::unparseable_kept_verbatim` (invalid UTF-8 → base64), `…::truncated_is_unparseable`, `…::schema_mismatch_kept_verbatim`, `…::large_raw_is_a_blob` (200 KiB) |
| PR9-CL-17 | Every rejection of §2.11 rows P, G, and H shall produce its listed observation and create no delivery, event, descendant row, or projection change, each case within its deadline. | CL-16; mvp §2.5 | `hook_b8::parser_layer`, `…::guard_layer`, `…::handler_layer` (tables, §7.1) |
| PR9-CL-18 | A rejection decided before the body shall reach a client that withholds its body until it reads the response. | mvp §4 PR 9 "response ordering" | `hook_b8::ordering_body_withheld` |
| PR9-CL-19 | A hook's budget shall run from its connection's first head byte: a request whose head took most of the 5 s shall get 408 and record nothing once the deadline passes. | mvp §2.5; http §2.2 | `hook_b8::deadline_from_first_head_byte`, `…::body_held_is_408`, `…::slow_body_is_408` |
| PR9-CL-20 | With body bytes in flight at a rejection, the client shall observe the decided status or a reset, never a success, and nothing shall be created; the observation shall be recorded per OS. | K2; http H5 | `hook_b8::body_in_flight_observed` (1 MiB body sent up front with a revoked token; the observation printed) |
| PR9-CL-21 | Two equal `Content-Length` headers shall be accepted as one, and `Transfer-Encoding` in any combination shall be 400 with nothing created. | mvp §2.5; K1 | `hook_b8::duplicate_equal_length_accepted`, `hook_b8::guard_layer` rows G6 |
| PR9-CL-22 | A store outcome of the delivery job shall map to §2.11 rows J1–J8. | §2.11 | `hook_ingest::store_outcomes_map` (PR 7's fault points: expired, busy, commit unknown, writer gone; closing registry) |
| PR9-CL-23 | The emitter shall exit 0, write nothing to stdout or stderr, and finish within 3 s, whatever the daemon does. | CL-23 | `hook_emit::{refused, silent_listener, slow_status, rejecting, accepting, panic_path}` (elapsed < 3.5 s literal, exit 0, both outputs empty) |
| PR9-CL-24 | The emitter shall send at most 3 attempts with one `emit_id`, retrying only after a transport error, 408, or 503. | mvp §4 PR 9 | `hook_emit::retries_503_with_one_emit_id` (503, 503, 200: 3 requests, one id), `…::no_retry_on_401`, `…::retries_after_reset` |
| PR9-CL-25 | The emitter shall not send when the URL or token is missing or not exact, when stdin stays open past the budget, and shall send the first 4 MiB marked truncated when stdin is longer. | spike B-9 | `hook_emit::no_request_without_env` (each variable alone), `…::no_request_with_open_stdin`, `…::truncates_over_4_mib` |
| PR9-CL-26 | A process shall be recorded as a launch's descendant only when the daemon's own walk linked it to the launch's identity through parents created no later than their children. | mvp §4 PR 9 / 10 | unit `process::tests::walk_refuses_newer_parent`, `…::walk_ends_at_gone_and_unreadable`; `hook_ingest::descendants_only_through_the_launch` (a chain not reaching `L` records nothing), `…::descendants_capped`; `process_native::ancestry_of_a_child_reaches_this_process` |
| PR9-CL-27 | A `hook` token shall be accepted only with its thread and launch, the launch belonging to that thread. | mvp §2.5 | `hook_ingest::hook_token_checks` (no launch, another thread's launch, revoked) |
| PR9-CL-28 | A `raw_unknown` of a hook shall be in the token's thread stream with the token's launch. | mvp §2.1 | asserted in every `raw_unknown` case of `hook_ingest` through one helper |

Every PR 9 obligation maps to a claim: `hook_deliveries` reply, ordinal, and routing (PR9-CL-1, -2, -5); the emitter's retry policy (PR9-CL-24); B-8 per layer (PR9-CL-17); budgets and response ordering (PR9-CL-18, -19); oversized heads (PR9-CL-17, rows P2); duplicate `Content-Length` and `Transfer-Encoding` (PR9-CL-21); held, sent, and slow bodies (PR9-CL-19, -20); positive controls (§7.1); a retry after binding (PR9-CL-2); nested FakeCli (PR9-CL-7); durable buffering (PR9-CL-4); ancestry re-validation (PR9-CL-8, -12); a conflicting `SessionStart` (PR9-CL-9); hole d (PR9-CL-11, -12); session selection (PR9-CL-10); `extra` attribution (PR9-CL-26); early hooks and buffer bounds (PR9-CL-13–15); concurrent binding and ordinals (PR9-CL-5, -6). mvp CL-10 is PR9-CL-1–3, CL-16 is PR9-CL-17, CL-17 is PR9-CL-7, CL-23 is PR9-CL-23.

## 7. Tests and Completion Commands

All tests run on `macos-latest` and `windows-latest`. Caps and budgets in tests are the literals of this design (5 s, 1 s, 3 s, 4 MiB, 8 MiB, 32, 30 s, 32 MiB, 1024, 16), set through `HookConfig` / `HttpConfig` only where a test must shorten a wait, and then named in the test.

### 7.1 B-8 (`crates/daemon/tests/hook_b8.rs`)

Each table row: a name, the raw request (head, then body sent, withheld, trickled, or none), the expected observation, a per-case deadline, and the no-side-effect check. The client is the raw client of `tests/support/http.rs`; its observation is one of `Status(code, ErrorBody code or none)`, `Eof`, `Reset`. **No side effect** (`support::hook::Snapshot`): `hook_deliveries` rows of the launch, `launch_descendants` rows, `head_seq` of the host and thread streams, and every `proj_*` row are equal before and after the case. Every table ends with its positive control: the same server and launch accept a valid hook (200, one delivery), so a broken setup cannot pass the negative cases.

| Layer | Cases (expected; per-case deadline) |
|---|---|
| Parser | `+5` with body withheld (400, no `ErrorBody`; 8 s); two different lengths (400; 8 s); 9 KiB head (431; 8 s); 65 headers (431; 8 s); HTTP/1.0 with `Transfer-Encoding: chunked` (400; 8 s); head missing its final CRLF (`Eof` after ≥ 4.9 s, before 8 s; 12 s) |
| Guard | bad `Host`, wrong port, two `Host`, no `Host` (421); `GET /hook` (405, `Allow: POST`); `POST /hooks` (404); `Origin: http://127.0.0.1:<port>` (403); no `Authorization`, two, `Basic`, malformed, unknown, revoked (401); owner token (403); `Transfer-Encoding: chunked`, `chunked` + `Content-Length`, `Content-Length` + `chunked` (400 `bad_request`); no `Content-Length` (411); `4 MiB + 1` declared (413); each 8 s |
| Handler | no `X-Chaldea-Emit-Id`, two, `not-a-uuid` (400); body budget exhausted by held requests (503 `busy`); each 8 s |
| Ordering | every guard and handler case above with the body withheld until the status is read (status read with zero body bytes sent; 8 s) |
| Deadline | head trickled over 4.5 s then body (408, nothing recorded; 12 s); head complete, body never sent (408 at 5 s ± 0.3 s; 12 s); body trickled past the deadline (408 or `Reset`, printed; 12 s) |
| In flight | revoked token with a 1 MiB body sent up front (401 or `Reset`, printed with the OS; 12 s); same for 413 declared over 4 MiB with 1 MiB sent (413 or `Reset`; 12 s) |
| Accepted | two equal `Content-Length` (200, one delivery); valid hook (200) |

`body_in_flight_observed` and `slow_body_is_408` print `B8-OBSERVED <case> <os> <observation>` lines, which are the per-OS record that arch §5's open item asks for.

### 7.2 Ingest (`crates/daemon/tests/hook_ingest.rs`) and units

The tests of §6.2 named `hook_ingest::*`, against the test server with the hook mount, a `TestStore`, launches created by appending `thread_registered`, `launch_reserved`, `launch_started` directly, a synthetic `ProcessProbe` (chains as data), a recording `IntakeProbe`, and a test `HookAdapter` that maps `UserPromptSubmit` to `user_message { input_id: null }` and counts calls. Units: `hook::payload` (table of bodies → `Decoded`), `hook::ancestry` (eligibility table: nested same name, different agent name, `L` absent, empty chain), `hook::request` (header table), store `triggers::session_bound_once`, `store_descendants::{dedupe, cap, immutable}`, `schema_tests::shipped_fingerprints_match_the_migrations` (v2 added).

### 7.3 Process lookup (`crates/pty-runtime`)

`process::tests::*` (synthetic table: walk to root, depth limit, `Gone`, `Unreadable`, `ParentNewer`); `tests/process_native.rs::ancestry_of_a_child_reaches_this_process` (spawns a child under `with_spawn_lock` that spawns a grandchild; the grandchild's walk contains the child and this test process with identities equal to `lookup` of each) and `::name_is_normalized` (the test binary's name without `.exe`).

### 7.4 Emitter and end to end (`crates/cli`)

`tests/hook_emit.rs`: the emitter binary against stub `TcpListener`s on `127.0.0.1:0` (refusing, silent, slow status, scripted statuses, reset after the head), the cases of PR9-CL-23–PR9-CL-25; units for URL and status-line parsing. `tests/hook_e2e.rs` (real daemon from public APIs, real FakeCli, real emitter, real walk): `codex_order_binds_and_promotes` (FakeCli `hooks` firing `UserPromptSubmit` and `SessionStart` concurrently), `claude_session_id_binds`, `nested_fake_cli_does_not_bind` (`echo --nested`: the nested `SessionStart` before and after the real one), `descendants_recorded_from_a_real_chain`.

### 7.5 Completion commands

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p chaldea-pty-runtime --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings   # on macOS before pushing; CI runs the Windows clippy natively
cargo test --workspace --locked
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum|hyper|tower|getrandom) ' <<<"$deps"
```

plus the `workspace` job green on both runners with the sentinel of §1, the `specs` job unchanged (no Quint change: the model already states what PR 9 implements, K10), and the `pty-hooks` job unchanged. A regression test counts only after it was seen failing with its guard removed: in particular row 5's ancestry check (CL-8), the promotion recheck (CL-12), the `ParentNewer` guard (CL-26), the dedup step (CL-1), and the emitter's watchdog (CL-23).

## 8. Decisions

**D1. `chaldea hook-emit <agent>` is the only emitter command form, from the first release.** Codex hashes the command string into hook trust (arch §5), so any later change of the arguments costs every user a `Hooks need review`; everything else passes through the environment. Irreversible in practice; it follows mvp §2.7's "fixed command strings".

**D2. A conflicting second `SessionStart` does not rebind** (open question Q1 below). It is `raw_unknown` `foreign_session`; the binding and its session stay. Reason: one binding per launch is what the store trigger, PR 10's gate, and PR 13's tailer assume, and an ambiguous rebind could attribute hooks to the wrong session. Cost, if A1 holds: after `/clear` (Claude) or `/new` (Codex) in an attached session, that launch's later hooks are all `raw_unknown` and its gate never reopens until a resume. Reversible: a rebind rule can be added (dropping the trigger in a migration) without changing stored events.

**D3. Claude binds and selects by session id; its ancestry is recorded, not required.** mvp §2.9 and the spike verified the session-id rule for Claude; requiring ancestry for Claude was never run on a real Claude host (the spike's Claude binding used the session id only, K9). PR 13 checks the chain on real hosts; switching Claude to rule `Ancestry` is one row of `HookAdapter::rule`.

**D4. The buffer bounds are 32 deliveries, 8 MiB, and 30 s per launch.** Codex's early hooks are the first turn's few concurrent hooks; 30 s exceeds the 5 s request deadline plus the 1 s pid wait many times, and 8 MiB holds two maximal bodies. Internal; tunable without migration.

**D5. Stdin over 4 MiB is sent truncated and kept as `raw_unknown` `unparseable`.** The spike dropped it (B-9), which is a silent loss (arch §1); the truncated prefix keeps the evidence that the hook fired. The 4 MiB cap (mvp §2.5) is unchanged.

**D6. The `X-Chaldea-*` headers other than `X-Chaldea-Emit-Id` are internal.** Both ends are the same binary; a running launch keeps the emitter path it was given, and a daemon restart revokes old launches' tokens, so a version mix lasts at most until the restart. The daemon ignores unknown `X-Chaldea-*` headers and treats a missing pid as an empty chain.

**D7. A recorded delivery answers 200 with an empty body.** No new public body (mvp §2.5 lists the public bodies); the emitter reads only the status; dispositions are in the stream and in `hook_deliveries`. A disposition body can be added later; it is additive.

**D8. No emitter log file.** A log path would be a new public environment variable (mvp §2), and the emitter is silent by contract; the doctor (PR 21) observes deliveries on the daemon side. A `CHALDEA_HOOK_LOG` can be added later; it is additive.

**D9. B-8 does not drain rejected bodies.** With a large body in flight a client may see a reset instead of the status (K2); the only client retries and needs no status (§2.13), and nothing is created either way (PR9-CL-20). A lingering drain in PR 7's connection loop can be added if a client ever needs the status.

### Open Questions

**Q1. Should a launch follow a session switch (`/clear`, `/new`)?** Changes public behaviour of a thread after a session switch (mvp §2.9 binds once). Options: (a) no rebind (D2): safe, the launch goes dark after a switch until resume; (b) rebind on a `SessionStart` with a new session from an eligible chain of the bound launch: the thread follows, but `session_bound` becomes repeatable per launch, PR 10's gate and PR 13's tailer must treat it as a new turn start and a new transcript generation, and the store trigger is relaxed by migration; (c) (b) only for payloads whose `source` says `clear` or `new`, once PR 12 fixtures show those values (A1). Reversibility: (a) → (b)/(c) is a migration and additive handling; (b) → (a) leaves stored rebinds. **Recommendation:** (a) in PR 9; decide (c) after PR 12 captures `/clear` and `/new` payloads.

## Related Text in Other Documents

- `docs/architecture.md` §5 "Windows hook receiver": corrected in this PR to the daemon's verified expectations (`Content-Length: +5` → hyper's 400, K1; slow head → EOF). `docs/mvp-design.md` §2.5 already states them; its "Proposed Canonical-Doc Changes" bullet for arch §5 is thereby applied.
- `docs/design/http-surface.md` §2.4 ("for `hook`, only revocation (PR 9 adds its launch and binding checks)") and §2.11: PR 9 fills both (§2.8, §2.3); `WriteCtx` gains `read` and `commits` (§1 files).
- `docs/design/event-store.md` §2.4: migration v2 adds `launch_descendants` and `events_session_bound_once` (§2.12).
- `docs/design/pty-unix.md` §4 and `docs/design/pty-windows.md` §4 "Recorded descendants": the walk and its public lookup are §2.5 and §2.12 here.
