# Design: Hook Ingest (MVP PR 9)

Status: design. Scope is the PR 9 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): `POST /hook`, binding, `raw_unknown`, `chaldea hook-emit`, and the B-8 cases (arch §5 open item **Windows hook receiver**). The contract implemented is mvp §2.1 (hook idempotency keys, `raw_unknown`), §2.5 (the `/hook` route, its rejections, deadline, and log rules), §2.9 (early hooks, binding, buffering), the PR 9 and PR 9 / 10 design obligations of mvp §4, and the claims CL-10, CL-16, CL-17, CL-23 of mvp §6.2. It builds on the store of `docs/design/event-store.md` (cited as "store §N"), the HTTP surface of `docs/design/http-surface.md` (cited as "http §N"), and implements the `buffer*` and `bind` actions that `specs/thread_input.qnt` assumes (`docs/design/formal-specs.md`, cited as "formal §N").

**What PR 9 decides and what it leaves.** PR 9 owns everything between the `/hook` guard and the store: the request headers, the hook body budget, intake order, intake facts, and the binding order, the process-ancestry walk and its proof, delivery deduplication, binding, session selection, buffering and promotion, `raw_unknown` reasons, the attribution of descendant processes to a launch, and the emitter (`chaldea hook-emit`). It does not decide what a bound hook *means* (which events a `UserPromptSubmit`, `Stop`, or `PermissionRequest` produces, readiness evidence, ack attribution): that is a seam (`HookAdapter`, §2.6) filled by PR 10 (supervisor facts) and PR 13 / 14 (payload decoding). It does not launch CLIs, inject hooks, mint hook tokens, run timers, or recover at restart (PR 10, 13, 14); it names the functions those PRs call and the obligations they carry (§2.9, §4).

**Trust boundary.** Processes of the user's OS account are trusted already: they can read the owner token file and get owner access (mvp §2.5). Other OS accounts are outside the threat model (as in PR 8's design). Within that model:

- The **hook token is the authentication boundary for its launch.** Every process of the launch's tree inherits `CHALDEA_HOOK_TOKEN` and can post any hook for that launch, with any headers; it can never read or write another launch or thread (the token names one launch, §2.8).
- The **ancestry walk separates unintentional emissions from the launch's own.** It keeps a nested CLI that runs the unmodified `chaldea hook-emit` with the inherited environment (spike B-11, prior-art row 6c) from binding or feeding the launch. It is not a defence against a process that deliberately forges `X-Chaldea-*` headers or names another process's pid; such a process already holds the token.
- The walk's other output, descendant identities for `extra` (§2.12), rests on the parent links the daemon proved itself, never on a header; a forged pid can only make the daemon walk a real chain that reaches the launch.

**Commitments from PR 6 and PR 7 this design keeps.**

1. One store job per delivery: deduplication on `(launch_id, emit_id)`, the routing decision, the events, binding, and promotion run in one `BEGIN IMMEDIATE` transaction (store §2.3 "Hook deliveries"). The store keeps buffered data opaque; PR 9 owns its format (§2.9).
2. The reply is sent only after the job's ticket resolved `Ok`, so a buffered hook is durable before 200 (store §2.9, D5).
3. `/hook` is a `Mount::write` route (http §2.2 route table, `crates/daemon/src/http/routes.rs:264-270`): Host, `Origin` absent, `hook` token, framing, and size are decided by PR 7's guard before the handler, and the deadline runs from the connection's first head byte (http §2.2 "One deadline origin"). PR 9 adds no check before the guard and changes none of its order.
4. Store outcomes map through PR 7's `run_write` / `write_failure` mapping (`crates/daemon/src/http/ctx.rs:145-229`, `error.rs:223-244`); PR 9 adds no status code.
5. `core` is unchanged: no public type, payload, kind, or reason is added. No hook body or token reaches a log (mvp §2.5 "Secrets"; http §2.10).

Sources read for this document:

| Source | Revision | Read for |
|---|---|---|
| This repository | `3df7c70` | `docs/mvp-design.md`, `docs/architecture.md` §5, `docs/prior-art.md` rows 5a, 5b, 6a–6c, 9, `docs/design/{event-store,http-surface,formal-specs,pty-runtime,pty-unix,pty-windows}.md`, `specs/thread_input.qnt` (`bind`, `buffer*`), `crates/daemon/src/http/{routes,guard,ctx,auth,error}.rs`, `crates/daemon/src/http/tests/faults.rs`, `crates/daemon/src/store/{rows,write_tx,readers,schema,annex,writer}.rs`, `crates/daemon/tests/{http_layers,http_matrix,http_logs}.rs`, `tests/support/http.rs`, `crates/core/src/{state,payload,ids}.rs`, `crates/pty-runtime/src/{lib,backend}.rs`, `unix/{table,sys}.rs`, `windows/sys.rs`, `crates/testkit/src/fake_cli/`, `Cargo.toml`, `.github/workflows/ci.yml` |
| Spike `spikes/pty-hooks` | `3df7c70` | `src/emitter.rs`, `src/http_client.rs`, `src/receiver.rs` (binding, B-8), `src/fake_cli.rs` (`echo --nested`), `src/os/{mod,unix,windows}.rs` (ancestry), `src/scenario/{fake,real}.rs` (CLI names), `docs/spikes/pty-hooks-spike.md` B-8, B-9, B-11, F-3c, §4.3 |
| `hyper` | crate 1.12.0 | `src/proto/h1/role.rs`, `src/proto/h1/conn.rs` |
| `libc` | crate 0.2.191 | `src/unix/bsd/apple/mod.rs` (`proc_bsdinfo`) |
| `windows-sys` | crate 0.61.2 | `Cargo.toml` features (`Win32_System_Diagnostics_ToolHelp`) |

**Facts this design relies on.** T-facts are exercised by named tests of §7 on both runners; D-facts rest on the cited source; A-facts are assumptions not verified in source for this document.

| # | Fact | Where | Kind |
|---|---|---|---|
| K1 | hyper answers a `Content-Length` that is not all digits (`+5`) and two different `Content-Length` values with its own 400 before the service; it keeps one of two equal `Content-Length` headers and drops the second; with `Transfer-Encoding` present it removes `Content-Length` (either order); an HTTP/1.0 request with `Transfer-Encoding` is its own 400. | hyper `src/proto/h1/role.rs:275-312` | T (`http_layers::parser_layer_cases`, `te_with_content_length_is_400`; PR 9's `hook_b8::*`) |
| K2 | After a response sent before the body was read, hyper reads what is already available once and otherwise closes the read side (http H5); on Windows a close with unread data can reset the connection. | hyper `src/proto/h1/conn.rs:875-891`; spike §3 B-8 | T: PR 9 records the observation per OS (`hook_b8::observed_*`) |
| K3 | `WriteCtx::write` verifies the token inside the job, submits with `not_after = deadline`, awaits the result until `deadline + write_reply_grace`, and maps failures; `verify_hash_in` checks only revocation for a `hook` token; PR 7's hook fixtures mint hook tokens with no thread and no launch. | `crates/daemon/src/http/ctx.rs:206-229`, `:358-374`; `auth.rs:152-180`; `error.rs:223-244`; `tests/support/http.rs:194-205`; `http_layers.rs:209`; `http_logs.rs:81` | D |
| K4 | `hook_delivery_record` returns `Replay` for an existing `(launch_id, emit_id)` and changes nothing; `receipt_ordinal` is the caller's, unique per launch; promotion is only `buffered → appended \| raw_unknown`, enforced by trigger; `buffered_deliveries` returns rows in receipt order with their `received_at` (the record job's commit time). | store §2.3, §2.4; `write_tx.rs:884`, `:939`; `readers.rs:1232` | D (PR 6 tests) |
| K5 | `reduce(SessionBound)` refuses unless `process_state` is `running` or `stopping`, and removes both startup attention causes. | `crates/core/src/state.rs:274`, `:298-305` | D |
| K6 | The macOS process lookup reads `proc_bsdinfo` (pid, ppid, pgid, start in µs, zombie flag); `proc_bsdinfo` also has `pbi_name[32]` (31 characters and a NUL) and `pbi_comm[16]`. The Windows backend has `open_process` and `creation_time` but no parent lookup; `windows-sys 0.61.2` has the `Win32_System_Diagnostics_ToolHelp` feature, which the spike's Windows ancestry used. | `crates/pty-runtime/src/unix/table.rs:17-48`, `unix/sys.rs:189-225`; libc `apple/mod.rs:682-683`; `windows/sys.rs:485`, `:497`; windows-sys `Cargo.toml:200`; spike `src/os/windows.rs:27-28`, `:126-128` | D; T (`process_native::*`) |
| K7 | `ProcessIdentity.start_time` is decimal µs since the Unix epoch on macOS and the creation `FILETIME` in decimal 100 ns units on Windows, both wall-clock stamps; equal `(pid, start_time)` identifies a process, not its provenance. | `docs/design/pty-unix.md` §2.1 ("Identity format"); `docs/design/pty-windows.md` §2.1, §2.5 | D |
| K8 | The spike emitter exits 0 from a watchdog thread at 3 s whatever the main thread blocks on, caps stdin at 4 MiB + 1, never writes stdout, accepts only `http://127.0.0.1:<port>/hook` or `http://localhost:<port>/hook`, and connects to `127.0.0.1` whatever the `Host`. | spike `src/emitter.rs:34-103`, `src/http_client.rs:28-50`, `:62-95` | D; T again in PR 9 (`hook_emit::*`) |
| K9 | The spike bound Codex and FakeCli launches by the nearest ancestor of the emitter (skipping the emitter) whose normalized name is a CLI name (the agent name and the launched binary's file stem), equal to the launched pid; later hooks re-checked it; a nested FakeCli failed it (B-11). Its chain came from the emitter's own header. | spike `src/receiver.rs:854-861`, `:873-1003`; `src/scenario/real.rs:285-291` | D |
| K10 | `bind` appends `session_bound`, then applies the buffered hooks in intake order with the facts of their intake (`SessionBound → buffered PromptSubmitted → Stop`); `buffer*` records hooks of an unbound live launch without feeding a reducer. | `specs/thread_input.qnt:885-993`; formal §1, §3 hole d | D |
| K11 | Codex runs hook commands concurrently, so `UserPromptSubmit` can arrive before `SessionStart`. | mvp §2.9; arch §5 | D (from the mvp) |
| K12 | The store writer takes jobs from one FIFO queue, one at a time, and takes the next job only after the previous one's `COMMIT` or rollback; the writer has test fault points for commit failures and a writer panic. | store §2.2; `crates/daemon/src/store/writer.rs:94-112`, `:441` | D |
| K13 | macOS reparents a process's children when it exits, before its pid can be freed (the zombie is reaped later), so a live process's `pbi_ppid` never names a later process. | XNU `proc_exit` / reparenting (not read for this document) | A (T for the walk's behaviour: `process::tests::*`) |
| A1 | Claude Code fires `SessionStart` with a new session id on `/clear` and on an in-session `/resume`; Codex 0.171.0 treats `/new` and `/clear` as new chats, and its `SessionStart` `source` has no `new` value. | reported against the Claude hooks reference and Codex `tui/src/slash_command.rs:90-106`, `hooks/src/events/session_start.rs:20-38` (v0.171.0); not read for this document | A (PR 12 fixtures) |
| A2 | On Windows, a loopback connect to a port with no listener can take about 2 s to be refused. | not measured | A (the emitter's connect bound covers either way) |

## 1. Goal and Dependencies

**Goal.** A `hook` module in `chaldea-daemon` that accepts a CLI hook on `POST /hook`, decides its routing (appended to the thread, buffered while its launch is unbound, or kept as `raw_unknown`) in one store job, binds a launch only through mvp §2.9's rule and never ahead of an earlier hook of the same launch, promotes buffered hooks in receipt order at binding, records the processes a proven hook chain shows to descend from the launch, and replies only after the decision is durable; and `chaldea hook-emit`, which posts one hook payload with one `emit_id`, retries within its budget, and always exits 0 within 3 s with nothing on stdout. Every B-8 case is a table-driven test per layer on `macos-latest` and `windows-latest`.

**PR split** (§3 "PR size"): **9a** — `pty_runtime::process` (public lookup and proven ancestry walk), `chaldea hook-emit` in `crates/cli`, FakeCli hook emission; **9b** — the daemon's `hook` module, store migration v2, the `/hook` mount, the PR 7 fixture updates, B-8, the end-to-end tests. 9b depends on 9a.

**`crates/cli` ownership.** PR 8 (daemon lifecycle) also creates `crates/cli`. Whichever of PR 8 and 9a merges first creates it with the workspace member entry, `[[bin]] name = "chaldea"`, and a `main.rs` whose first statement dispatches `hook-emit` (§2.13); the other rebases onto it and adds its modules. The dispatch rule (hook-emit first, before any runtime or configuration initialization; mvp §3 "Single binary") is the shared contract.

**Files.**

```
crates/pty-runtime/src/process.rs         # public: ProcessEntry, Lookup, lookup(pid), Ancestry, ancestry(pid, max); the walk over a Table trait (9a)
crates/pty-runtime/src/unix/table.rs      # + image name; the per-link re-read (9a)
crates/pty-runtime/src/unix/sys.rs        # BsdInfo gains `name` from the same proc_pidinfo call (9a)
crates/pty-runtime/src/windows/process.rs # two Toolhelp snapshots + one handle per process (9a)
crates/pty-runtime/src/windows/sys.rs     # CreateToolhelp32Snapshot, Process32FirstW / NextW wrappers (9a)
crates/pty-runtime/tests/process_native.rs
crates/cli/Cargo.toml, src/main.rs        # binary `chaldea`; `hook-emit` dispatched first (created by PR 8 or 9a, whichever merges first)
crates/cli/src/hook_emit/{mod,env,client,watchdog}.rs   # the emitter (9a)
crates/cli/tests/hook_emit.rs             # emitter against stub listeners (9a)
crates/cli/tests/hook_e2e.rs              # daemon + FakeCli + emitter, real processes (9b)
crates/testkit/src/fake_cli/echo.rs       # + --emitter, --agent, --session-id, --nested, --nested-child (lift of spike echo, 9a)
crates/testkit/src/fake_cli/hooks.rs      # new mode `hooks`: a scripted hook sequence (9a)
crates/daemon/src/hook/mod.rs             # HookIngest, HookConfig, HookSeams, mount() (9b)
crates/daemon/src/hook/request.rs         # X-Chaldea-* header parsing (9b)
crates/daemon/src/hook/payload.rs         # decode(): the pure hook-head decoder, the fuzz target of PR 22 (9b)
crates/daemon/src/hook/ancestry.rs        # eligibility, descendants to attribute (9b)
crates/daemon/src/hook/intake.rs          # IntakeOrder, IntakeTicket (9b)
crates/daemon/src/hook/route.rs           # the delivery job: dedup, decide, bind, buffer, promote (9b)
crates/daemon/src/hook/buffer.rs          # buffer bounds, expiry, resolve_unbindable (PR 10 calls it) (9b)
crates/daemon/src/hook/adapter.rs         # HookAdapter, MvpHooks (the default until PR 10 / 13 / 14) (9b)
crates/daemon/src/hook/tests/             # unit tests with the HTTP and store fault points (9b)
crates/daemon/src/http/ctx.rs             # + WriteCtx::read, WriteCtx::commits (9b)
crates/daemon/src/http/auth.rs            # verify_hash_in: the hook token's launch checks (9b)
crates/daemon/src/http/routes.rs          # Mounts::mvp() mounts /hook with HookSeams::mvp() (9b)
crates/daemon/src/store/schema.rs         # migration v2 (§2.12) (9b)
crates/daemon/src/store/{write_tx,readers,annex}.rs     # descendants_record, descendants, buffered_summary, max_receipt_ordinal, EventAnnex::Hook (9b)
crates/daemon/tests/support/http.rs       # token_of(Hook) mints a scoped hook token for a registered, started launch (9b)
crates/daemon/tests/http_layers.rs        # hook fixtures scoped; /hook expectations now reach the handler (501 → 400 for a missing emit id) (9b)
crates/daemon/tests/http_matrix.rs        # INVENTORY: POST /hook is mounted; hook-kind cells use the scoped token (9b)
crates/daemon/tests/http_logs.rs          # the hook case runs the real handler with a scoped token (9b)
crates/daemon/tests/hook_b8.rs, hook_ingest.rs, support/hook.rs   # (9b)
.github/workflows/ci.yml                  # Windows sentinel; B-8 observation step (§1 "Workspace and CI")
```

**Dependencies.** No new crate and no new version.

```toml
# Cargo.toml [workspace.dependencies] windows-sys features: one line added
  "Win32_System_Diagnostics_ToolHelp", # CreateToolhelp32Snapshot, Process32FirstW / NextW (the parent pid of the ancestry walk)
```

- New dependency edges: `chaldea-daemon → chaldea-pty-runtime` (normal; PR 10 needs it anyway; its packages are already in `Cargo.lock`); `chaldea-cli` (new package in `Cargo.lock` unless PR 8 added it) `→ uuid` with `features = ["v4"]` (the `emit_id`; `uuid =1.29.1` already uses `getrandom 0.4.3`). Nothing else on the `hook-emit` path: std networking only (prior-art row 5b's hand-written alternative, needed for separate connect and attempt bounds). `crates/cli` dev-dependencies: `chaldea-daemon`, `chaldea-core`, `chaldea-pty-runtime`, `tokio` (`rt-multi-thread`, `net`, `time`, `macros`), `tempfile`, `serde_json`, `sha2`, all workspace pins.
- `crates/testkit`: unchanged dependencies; FakeCli runs the emitter as a child process given by `--emitter <path>`, as in the spike.

**Workspace and CI.** `crates/cli` joins `members` if absent. No new job. `cargo test --workspace --locked` runs everything on both runners. Two steps change in the `workspace` job: the Windows sentinel step lists `chaldea-pty-runtime --test process_native ancestry_of_a_child_reaches_this_process` (the Toolhelp path compiles only on Windows); and a new step after "Run tests", `cargo test -p chaldea-daemon --test hook_b8 --locked -- observed_ --show-output`, prints the per-OS observations of the in-flight-body cases on success (the main run captures passing tests' output). The `core` dependency check is unchanged. `lefthook.yml` mirrors the sentinel change only.

**FakeCli binary in `crates/cli` tests.** The emitter is `env!("CARGO_BIN_EXE_chaldea")`. FakeCli is `chaldea-fake-cli[.exe]` in the directory above the test executable's `deps/`. Cargo builds testkit's binary for testkit's own integration tests, so under `cargo test --workspace` (the CI command) it exists before any test runs; under `cargo test -p chaldea-cli` alone it may not, and the helper then fails with "build with cargo test --workspace", never skips. Its name (16 characters) survives macOS `pbi_name` truncation.

## 2. Types and Functions

Names may change in review; shapes may not.

### 2.1 The Rules

1. **One job decides.** Deduplication, routing, binding, buffering, promotion, buffer bounds, and descendant records are decided inside the delivery's store job, from what the database holds at that moment, with every target (thread, launch) derived from the token as re-verified in that job (§2.10). Everything before the job (header checks, body, ancestry walk, pid wait, binding order) gathers inputs and writes nothing.
2. **Memory holds only intake order.** `IntakeOrder` (§2.4) keeps, per launch, the next receipt ordinal (seeded from the stored maximum) and the set of ordinals taken but not yet handed to the writer. Losing it at restart loses nothing: the stored maximum seeds it again, and no request outlives the process.
3. **Binding never overtakes an earlier hook.** A `SessionStart` that could bind waits until every lower receipt ordinal of its launch has been handed to the writer or released (§2.4).
4. **Hook meaning is the adapter's.** PR 9 interprets a payload only as far as binding and session selection need: `hook_event_name`, `session_id`, and for `SessionStart` `transcript_path` (§2.5). Every other event of a selected session goes through `HookAdapter::interpret` (§2.6).
5. **Only the current, live, bound launch and its session change state.** A hook reaches `interpret` only when its launch is the thread's current launch, has a recorded pid and no exit, is bound, and the hook is of the bound session (rows 3–8 of §2.7). A buffered hook appends no event, feeds no reducer, and changes no projection (formal §3 hole d).
6. **Every wait has a deadline** (§5 "Waits"), and every ending of a request is listed (§5 "Endings").

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
X-Chaldea-Attempt: <1|2|3>              (internal; the emitter's attempt number)
X-Chaldea-Emitter-Pid: <decimal pid>    (internal; the walk's starting pid)
X-Chaldea-Thread-Id: <uuid>             (internal; CHALDEA_THREAD_ID)
X-Chaldea-Agent: <agent>               (internal; the emitter's argument, recorded only)
X-Chaldea-Emitter-Version: <version>   (internal; recorded only)
X-Chaldea-Body-Truncated: 1             (internal; present only when stdin exceeded 4 MiB)
```

No `Origin` (403 otherwise, guard step 3). Header parsing (`request.rs`) runs after the guard and before the body:

- `X-Chaldea-Emit-Id` must occur once and parse as a hyphenated UUID, else 400 `bad_request`.
- Each other header of the list above may occur at most once; a second occurrence is 400 `bad_request`.
- Values outside their form are treated as absent: a pid that is not a decimal `u32` in `1..` (the chain is then empty), an attempt outside `1..=3` (then `None`, treated as a retry, §2.6), an empty agent or version (recorded as `unknown`). The agent and version are otherwise kept as sent, within the guard's 8 KiB head; they decide nothing.
- `X-Chaldea-*` headers not in the list are ignored, repeated or not (a newer emitter against an older daemon).

The internal headers are a protocol between two copies of the same binary (§8 D6). Their values are trusted only as far as the trust boundary allows: they steer which chain is walked and are never authority over another launch.

**Body cap.** 4 MiB (mvp §2.5): over it the guard answers 413 before the body. The emitter never sends more: a longer stdin is sent as its first 4 MiB with `X-Chaldea-Body-Truncated: 1` and is kept as `raw_unknown` `unparseable` with a truncation marker, never interpreted (§2.7 row 1; §8 D5).

**Body budget.** Before reading the body the handler takes `ceil(Content-Length / 1 KiB)` permits from one `Semaphore` of `body_budget / 1 KiB` permits (32 MiB), with `try_acquire_many_owned`; none free → 503 `busy`, nothing read. The permit moves with the body bytes into the delivery job's closure and is released when the writer drops the closure (after the job ran, expired, or was refused at submission), or when the request ends before submission. The budget counts the request bodies held between their read and the end of their job, so a client that disconnects after submission still holds its permit until the writer is done with the body. It does not count the buffered bodies a binding or expiry job reads back (at most 8 MiB, one job at a time on the single writer), the parsed JSON of the job's own body, or SQLite's internal copies.

**Reply.** Every recorded delivery answers `200` with an empty body (`Content-Length: 0`), the guard's `Cache-Control: no-store`, and PR 7's `Connection: close`; the stored first reply is `{ http_status: 200, body: "" }`. The disposition is observable in the thread stream and in `hook_deliveries`, not in the reply (§8 D7). Every other status is a rejection or an unknown outcome (§2.11).

### 2.3 Handler Flow

```rust
async fn handle(ingest: Arc<HookIngest>, ctx: WriteCtx, req: Request<Body>) -> Result<Response, ApiError>
```

| Step | What | Bound | Failure |
|---|---|---|---|
| 0 | PR 7 guard: Host → method → `Origin` absent → Bearer → kind `hook` → framing → size → deadline (http §2.2) | parser and request deadline | §2.11 rows P and G |
| 1 | Parse `X-Chaldea-*` headers (§2.2) | — | 400 `bad_request` |
| 2 | Take body permits (§2.2) | non-blocking | 503 `busy` |
| 3 | `ctx.body_within(body, 4 MiB)`; `payload::decode(body)` (pure) | the deadline | 408, 413, 400 |
| 4 | **Pre-read** (`WriteCtx::read`, one snapshot): `verify_hash_in` (with PR 9's hook checks, §2.8) → `Authorized { thread, launch }`; the launch and thread rows; `hook_delivery(launch, emit_id)`; `max_receipt_ordinal(launch)`. An existing delivery is answered with its stored first reply now (a shortcut; the job's dedup decides). | the deadline | 401; 503 `busy` / `shutting_down`; 500; 408 at the cutoff |
| 5 | **Intake**: `IntakeOrder::intake(launch, stored_max)` → an `IntakeTicket` with the ordinal, and `seams.intake.facts(launch)`, in one critical section (§2.4) | non-blocking | — |
| 6 | **Ancestry walk** of the emitter pid in `spawn_blocking` (§2.5), awaited until the deadline | `max_chain` links; the deadline | not finished by the deadline: 408 |
| 7 | **Pid wait**, only when the pre-read found no recorded pid and no exit (below) | `min(intake + pid_wait, deadline)` | none: an expired wait is passed to the job |
| 8 | **Binding order**, only for a decoded `SessionStart` whose launch was unbound at the pre-read: wait until `IntakeOrder` holds no lower ordinal of the launch (§2.4) | the deadline | 408, nothing written |
| 9 | **Delivery job** through `ctx.write` (`not_after = deadline`), the job's closure owning the body, the permit, the decoded payload, the chain, the facts, the `IntakeTicket`, and `pid_wait_expired` (§2.7) | the deadline + 100 ms reply grace | §2.11 rows J |
| 10 | Reply with the delivery's first reply | — | — |

**Pid wait (step 7).** Subscribe to the store's commit notifications first (`WriteCtx::commits()`), then re-read the launch and thread rows, so a `launch_started` that committed between the pre-read and the subscription is seen. The wait ends, without a decision, when the launch has a recorded pid, has exited, is no longer the thread's current launch, or the notification channel is closed (store shutdown); each notification naming the thread's stream, and each `Lagged`, re-reads. If it reaches `min(intake + pid_wait, deadline)` with none of these, `pid_wait_expired = true` goes into the job, which then records `raw_unknown` `unbound` (row 6) even if `launch_started` commits between the wait's end and the job: past the bound the hook is `unbound` (mvp §2.9 "Early hooks"), decided once.

Steps 1–8 write nothing. A request that ends before step 9 (client disconnect, deadline, rejection) drops its ticket and permit, which releases its ordinal for the binding order (§2.4); its ordinal is a gap (store §2.3 allows gaps). After step 9 submits, the job runs to its end on PR 7's registered observer (http §2.3).

### 2.4 Intake: Ordinal, Facts, and Binding Order

```rust
pub(crate) struct IntakeOrder { launches: std::sync::Mutex<HashMap<LaunchId, LaunchIntake>> }
struct LaunchIntake { next: u64, pending: BTreeSet<u64>, released: Arc<tokio::sync::Notify> }
pub(crate) struct IntakeTicket { launch: LaunchId, ordinal: u64, order: Arc<IntakeOrder> }   // Drop: remove from pending, notify_waiters
impl IntakeOrder {
    /// The next ordinal (an absent entry starts at stored_max + 1) and the probe's facts, in one critical section.
    pub(crate) fn intake(&self, l: LaunchId, stored_max: u64, probe: &dyn IntakeProbe) -> (IntakeTicket, Option<IntakeFacts>);
    /// Resolves once no ordinal lower than `t`'s is pending for its launch.
    pub(crate) async fn earlier_released(&self, t: &IntakeTicket);
}
pub trait IntakeProbe: Send + Sync + 'static {
    /// The facts PR 10 needs at a hook's intake (the PTY writer's position, dispatched writes), or None
    /// (no session yet). Non-blocking: it runs under the intake mutex.
    fn facts(&self, launch: LaunchId) -> Option<IntakeFacts>;
}
#[derive(Clone, Serialize, Deserialize)] pub struct IntakeFacts(pub serde_json::Value);   // opaque to PR 9
```

**Intake** is the instant step 5 runs: the request is complete, authenticated, and its launch known. The ordinal and the facts are taken in one critical section, so receipt order and the order of intake facts agree.

**Ordinals across restarts.** An absent entry starts at `max(receipt_ordinal) + 1` of the launch's stored deliveries (`ReadTx::max_receipt_ordinal`, read in the pre-read), so a hook token that is still valid after a restart (a launch that had exited before it, whose token PR 10's restart revocation does not reach) never reuses an ordinal (`UNIQUE (launch_id, receipt_ordinal)`, store §2.4). Two concurrent first requests both read the same maximum; the first creates the entry and the second takes the next value from it. Within a run, every ordinal of a launch comes from its entry, which is never removed.

**Binding order** (rule 3). `pending` holds every ordinal whose ticket is alive. A ticket dies when the request ends before submission, or when the writer drops the job closure that owns it: after the closure ran (the ticket is dropped at the end of the call, before the writer's `COMMIT`), when the writer refused it as expired, or when submission failed. A `SessionStart` of a launch that was unbound at the pre-read awaits `earlier_released` before step 9 (registering for `released` before checking `pending`, so no release is missed). Because the writer is one FIFO queue that takes the next job only after the previous one's `COMMIT` (K12), every lower-ordinal job has then either committed, rolled back, or will never run, before the binding job is even queued. So:

- a hook received before the binding `SessionStart` is buffered (or decided otherwise) before binding, and promoted in receipt order with it: `SessionBound → PromptSubmitted → Stop`, never `SessionBound → Stop → PromptSubmitted` (K10);
- of two eligible `SessionStart`s of different sessions, the one received first binds; the later one finds the launch bound and is `foreign_session` (§2.7 row 7);
- a lower request stuck in its own walk or pid wait delays the binding by at most its own deadline; a lower request that ends early releases its ordinal at once. A `SessionStart` whose own deadline passes while waiting answers 408 with nothing written, and the emitter retries (§2.13).

The wait never cycles: a `SessionStart` waits only for lower ordinals, and only `SessionStart`s wait.

**What intake order means for bound launches.** Deliveries appended directly while bound appear in the stream in commit order; their intake facts, not their stream order, are what ack attribution uses (mvp §2.3 "Ack"). A first attempt that recorded nothing leaves its retry as the delivery, with the retry's ordinal and facts, marked as a retry (§2.6).

Default probe (`HookSeams::mvp()`): returns `None`. PR 10 supplies the real one.

### 2.5 Process Lookup, Proven Ancestry, and Binding Rules

**Process lookup** (`pty_runtime::process`, public; PR3 D7 "the PR that first builds `extra` adds the public lookup"):

```rust
pub struct ProcessEntry { pub identity: ProcessIdentity, pub ppid: u32, pub name: String }
pub enum Lookup { Live(ProcessEntry), Gone, Unreadable }
pub fn lookup(pid: u32) -> Lookup;
pub struct Ancestry { pub links: Vec<ProcessEntry>, pub end: AncestryEnd }   // links[0] is `pid` itself
pub enum AncestryEnd { Root, Limit, Gone(u32), Unreadable(u32), Unproven(u32) }
pub fn ancestry(pid: u32, max: usize) -> Ancestry;
```

- `identity` uses the backend's identity format (K7), produced by the same functions as `launch_started`'s identity, so `(pid, start_time)` compares equal with it. `identity.group` is display only (macOS pgid; empty on Windows); comparisons use `(pid, start_time)` only, never `ProcessIdentity`'s derived equality.
- `name`: the image name, ASCII-lowercased, a trailing `.exe` removed. macOS: `pbi_name` (at most 31 characters), else `pbi_comm` (at most 15), from the same `proc_pidinfo` call; Windows: `szExeFile` of the Toolhelp entry. Names are compared only with names from the same function on the same host, so truncation applies equally to both sides.
- `Lookup`: a macOS zombie (`unix::table::Lookup::Zombie`) is `Gone`; on Windows, a process whose handle is signalled (exited, even while a handle keeps its object) is `Gone`; a refused open or read is `Unreadable`.

**The proven walk.** A link `child → parent` is accepted only when the walk proves that `parent` is the process that created `child`, in the same generation as the identities it reports; otherwise the walk ends with `Unproven(pid)` and the chain stops there. The proof differs per OS, behind one platform-independent walker over a `Table` trait, so the decision logic is unit-tested on every OS with synthetic tables:

- **macOS: read, read, re-read.** Read `child` (pid, ppid `P`, start `S`), read `P`, then re-read `child`: the link holds when the re-read is live with the same start `S` and the same ppid `P`. A process's children are reparented when it exits, before its pid can be freed (K13), so a child still naming `P` after `P` was read proves that `P` had not exited by then, so the `P` read in between is the creator. No clock is compared.
- **Windows: snapshot, handles, snapshot.** Take Toolhelp snapshot S1 and follow `th32ParentProcessID` from `pid`; open one handle (`PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE`) to every process of the chain and read its creation time from that handle; take snapshot S2. A link holds when (a) S2 has the same `(pid, ppid, szExeFile)` as S1 for both processes, (b) neither handle is signalled after S2, and (c) the parent's creation time is not later than the child's. While a handle is open its pid cannot be reused, so (a) and (b) bind S2's entries to the handles' processes and to the creation times read from them; (c) rejects a parent pid that a later process took after the creator exited (Windows keeps the creator's pid in `th32ParentProcessID` and never updates it). (c) is the only clock comparison: a system clock set back between a creator's exit and the reuse of its pid can defeat it, a residual stated in §8 D10. Handles are closed when the walk returns.
- The walk ends with `Root` at pid 0 or a self-parent, `Limit` past `max` links, `Gone` / `Unreadable` when a lookup fails.
- `unsafe` stays in `unix::sys` and `windows::sys` (`docs/design/pty-unix.md` §2.7).

**Eligibility** (`hook/ancestry.rs`, pure; decided in the job against the launch row). Let `L` be the launch's recorded `(pid, start_time)` from `launch_started`, and `emitter_name` the normalized image name of the emitter binary (`chaldea`, `HookConfig::emitter_name`). A chain is *eligible* for `L` when:

1. `links[0].name` equals `emitter_name` (the request's pid names a process running the emitter binary);
2. some proven link `k ≥ 1` has `(pid, start_time)` equal to `L`;
3. the CLI names are `{ agent name ("claude" / "codex"), links[k].name }`; and
4. the nearest link `j ≥ 1` whose name is in the CLI names is `k`.

A nested CLI inheriting the environment puts its own process (same image) between the emitter and `L`, so rule 4 fails (B-11; K9). An empty chain (no pid header, or link 0 not readable) is never eligible. The chain is walked at intake while the emitter is still waiting for its reply, so its parent links are intact.

**Binding rule per agent** (`HookAdapter::rule`, §2.6; mvp §2.7 "Session binding", §2.9):

| Agent | `SessionStart` of an unbound launch binds when | A later hook of the bound launch is selected when | Expected session |
|---|---|---|---|
| Claude | `session_id` equals the expected session | `session_id` equals the bound session | `HookAdapter::expected_session` (§2.6) |
| Codex | the chain is eligible | `session_id` equals the bound session **and** the chain is eligible | — (the first eligible `SessionStart`, in receipt order, names it) |

Both rows also require `X-Chaldea-Thread-Id` equal to the token's thread. Claude binds by its predetermined `--session-id` (mvp §2.9; spike §4.3) and keeps that check; its ancestry is walked and used for descendants, not required for selection (§8 D3).

### 2.6 The Adapter Seam

```rust
pub trait HookAdapter: Send + Sync + 'static {
    fn rule(&self, agent: Agent) -> BindingRule;                                      // SessionId | Ancestry
    /// Claude's predetermined session id for this launch, from stored facts only.
    fn expected_session(&self, tx: &ReadTx<'_>, launch: &LaunchRow) -> Result<Option<String>, StoreError>;
    /// Events for one selected hook of the current, live, bound launch, decided inside the delivery or binding job.
    fn interpret(&self, tx: &ReadTx<'_>, hook: &AcceptedHook<'_>) -> Result<Interpretation, StoreError>;
}
pub enum Interpretation { Events(Vec<HookEvent>), Raw(RawUnknownReason) }       // Events non-empty
pub struct HookEvent { pub event: TypedEvent, pub name: &'static str, pub annex: Option<EventAnnex>, pub blobs: Vec<BlobInput> }
pub struct AcceptedHook<'a> {
    pub thread: ThreadId, pub launch: &'a LaunchRow, pub emit_id: Uuid, pub receipt_ordinal: u64,
    pub event_name: &'a str, pub session_id: &'a str, pub json: &'a serde_json::Value, pub body: &'a [u8],
    pub intake: Option<&'a IntakeFacts>,
    pub attempt: Option<u8>,            // X-Chaldea-Attempt; None when absent or invalid
    pub intake_reliable: bool,          // attempt == Some(1): the facts were taken when the CLI's first request arrived
    pub binding_ordinal: u64,           // the receipt ordinal of the SessionStart that bound the launch
    pub promoted: bool,                 // true: applied from the buffer in the binding job
}
pub struct HookSeams { pub adapter: Arc<dyn HookAdapter>, pub intake: Arc<dyn IntakeProbe>, pub procs: Arc<dyn ProcessProbe> }
pub trait ProcessProbe: Send + Sync + 'static { fn chain(&self, emitter_pid: u32, max: usize) -> Ancestry; }  // blocking
```

- `interpret` is called only under rule 5 of §2.1. `interpret` and `expected_session` run on the writer thread inside the job: data only, non-blocking, no network, PTY, supervisor, or channel handle (http commitment 5). They read through the job's `ReadTx`, which sees the job's earlier appends: in the binding job, `session_bound` and its status events are already applied when `interpret` runs, and a promoted hook sees the events of the hooks promoted before it. Effects on the supervisor travel through the store's commit notifications.
- **Retries** (`attempt`, `intake_reliable`). A delivery recorded by a retry carries the retry's intake facts, which were taken after the CLI fired the hook and can include writes that came later (e.g. an input dispatched in between). `intake_reliable: false` tells PR 10 to treat the hook conservatively: it acks no input and is no readiness evidence (mvp §2.3: ambiguity leaves the gate closed or the input `unconfirmed`). A first attempt that committed is replayed by the dedup and never interpreted twice.
- **`binding_ordinal`** lets PR 10 tell a `Stop` received before the binding `SessionStart` from one received after it (formal §3, `trace_buffered_stop_after_session_start`). It is the `receipt_ordinal` of the delivery whose primary event is the launch's `session_bound`.
- PR 9 builds the envelope for each `HookEvent`: stream `thread:<thread>`, `thread_id`, `launch_id`, `command_id: null`, source `{ kind: hook, agent: launch.agent, cli_version: launch.cli_version, transcript: null }`, `occurred_at: None`. Keys: the first event of a delivery `hook:<emit_id>` (mvp §2.1); every further one `derived:hook:<emit_id>:<name>` (mvp §2.1 "Derived event"). **Reserved names:** `bind_status:<i>` and `binding_raw`, used by the binding job (§2.7); a `HookEvent.name` starting with `bind` is refused by PR 9 before any append (the job rolls back, 500), and so is a name repeated within one delivery (`KeyConflict`). Both are adapter bugs, caught by the adapter's own tests.
- `Interpretation::Raw(r)` stores the hook as `raw_unknown` with reason `r`.
- **`MvpHooks`** (the default in `HookSeams::mvp()` until PR 10 / 13 / 14 replace it): `rule` as in §2.5; `expected_session` = the thread's bound `session_id` when the thread has one (a resume passes `--resume <it>`), else the launch id in lowercase hyphenated form (a fresh launch passes `--session-id <launch_id>`); `interpret` = `Raw(UnknownEvent)` for every hook (no decoder exists before PR 13 / 14). The obligations this puts on PR 10 / 13 are in §4.

### 2.7 The Delivery Job

`route.rs` builds one closure capturing only data: the parsed headers, the body bytes and their permit, the decoded payload, the chain, the `IntakeTicket`, the intake facts, `pid_wait_expired`, `Arc<dyn HookAdapter>`, and `HookConfig`. It runs inside `ctx.write`, which first re-verifies the token in the job's transaction (§2.8). Every target below (the thread, the launch, `L`) is the re-verified token's, never a value from the pre-read.

1. **Dedup.** `tx.read().hook_delivery(launch, emit_id)`: present → return its first reply; nothing else runs.
2. **`emit_id` reuse.** `tx.read().event_by_key(thread stream, hook:<emit_id>)` present (it can only be another launch's delivery, since this launch's would have matched step 1) → refuse with 400 `bad_request`, nothing stored. The key contract stays `hook:<emit_id>` per stream (mvp §2.1); an honest emitter's v4 id never repeats, so only a forged or broken request meets this.
3. **Expire** (only while the launch is unbound). `buffer::expire(tx, launch, cfg)` (§2.9).
4. **Descendants.** When the chain contains `L` as a proven link (rule 2 of §2.5, whatever the other rules), `tx.descendants_record(launch, links[1..k], cap)` records the links strictly between the emitter and `L` (§2.12).
5. **Decide** the routing by the table below, first matching row.
6. **Record**: `tx.hook_delivery_record(NewHookDelivery { launch, emit_id, receipt_ordinal, first_reply: 200 "", routing })`.
7. **Bind** (row 9 only), after step 6 in the same transaction: re-read the launch and thread (now bound); call `interpret` for this `SessionStart` with `promoted: false` and append its events with `tx.append` (`Raw(r)` → a `raw_unknown` of this body with reason `r`, key `derived:hook:<emit_id>:binding_raw`, so the binding hook's payload is kept as a public event); then `tx.read().buffered_deliveries(launch)` in receipt order, each decided by the promotion table and passed to `hook_delivery_promote` in that order. Any error in step 7 rolls back the whole job, binding included.
8. Return the first reply.

`D` is the decoded payload; `L` the launch row (and its recorded identity); `T` the thread row.

| # | Condition | Routing |
|---|---|---|
| 1 | `X-Chaldea-Body-Truncated` present, or `D` is not JSON (invalid JSON or invalid UTF-8) | `raw_unknown` `unparseable` (truncated: with `EventAnnex::Hook { truncated: true }`, §8 D5) |
| 2 | `D` is JSON but not an object with a non-empty string `hook_event_name` and a non-empty string `session_id`; or a `SessionStart` whose `transcript_path` is present and neither a string nor `null` | `raw_unknown` `schema_mismatch` |
| 3 | `L` is not `T.current_launch_id` | `raw_unknown` `unbound` |
| 4 | `X-Chaldea-Thread-Id` absent or not the token's thread | `raw_unknown` `foreign_session` |
| 5 | `L` has exited (`exit_reason` set), bound or not | `raw_unknown` `unbound` |
| 6 | `L` has no recorded pid, or `pid_wait_expired` | `raw_unknown` `unbound` |
| 7 | `L` bound to session `S`: `D.session_id ≠ S` (a conflicting `SessionStart` included, §8 D2), or the rule is `Ancestry` and the chain is not eligible | `raw_unknown` `foreign_session` |
| 8 | `L` bound: otherwise | `interpret` → `appended` (its events) or `raw_unknown` (its reason) |
| 9 | `L` unbound, `SessionStart`, the rule's condition (§2.5) holds, and `reduce(T.state, SessionBound)` accepts | **bind**: `appended` with `session_bound { launch_id, session_id, transcript_path }` and one `status_changed` per `StatusChanged` the reducer returned (keys `derived:hook:<emit_id>:bind_status:<i>`); then step 7 |
| 10 | `L` unbound, `SessionStart`, the rule's condition holds, `reduce` refuses | `raw_unknown` `unbound` |
| 11 | `L` unbound, `SessionStart`, otherwise | `raw_unknown` `foreign_session` |
| 12 | `L` unbound, any other event, selected for the launch (Claude: `session_id` equals the expected session; Codex: the chain is eligible) | `buffered` if the bounds hold after adding it (§2.9), else **overflow**: every buffered delivery of `L` is promoted to `raw_unknown` `unbound` in receipt order, and this one is recorded as `raw_unknown` `unbound` |
| 13 | `L` unbound, any other event, otherwise | `raw_unknown` `foreign_session` |

**Promotion table** (step 7, each buffered delivery `B` read back with its meta, §2.9; the launch is current, live, and just bound to `S`):

| Condition | Promotion |
|---|---|
| `B.session_id ≠ S`, or its thread header is not the token's thread, or (rule `Ancestry`) its stored chain is not eligible for `L` — the ancestry evaluated at receipt, checked again against `L` here (mvp §2.9) | `RawUnknown` `foreign_session` |
| otherwise | `interpret` with `promoted: true`, `binding_ordinal` = this binding's ordinal, and `B`'s stored intake facts and attempt → `Appended(events)` or `RawUnknown(reason)` |

**`raw_unknown` of a hook**: stream `thread:<thread>` (the token names the thread, so a hook is never "unbound to a thread" in mvp §2.1's stream rule), `launch_id` = the token's launch, key `hook:<emit_id>` (or the reserved `binding_raw` name above), payload `{ source: hook, reason, agent: launch.agent, cli_version: launch.cli_version, content_type: "application/json" when row 1 did not apply else "application/octet-stream", raw: prepare_content(body) }`. The raw bytes are the request body exactly (`Content` inline UTF-8, base64 for invalid UTF-8, a blob above 64 KiB; store §2.5).

**Why the rows are in this order.** Rows 1–2 keep anything PR 9 cannot read as a hook, before any session logic. Rows 3, 5, 6 enforce rule 5 of §2.1: a superseded launch, an exited launch, and a launch with no pid (or whose wait ran out) change no state. Rows 4, 7, 11–13 are session selection: a hook of another session never reaches `interpret` (the Quint model takes in only hooks of the selected session, formal §1). Row 9 is the only place a launch binds.

**Hooks after exit, and PR 10's late ack.** Every hook whose job runs after `launch_exited` committed is kept verbatim as `raw_unknown` `unbound` (row 5), even if it is of the bound session and was fired before the exit. A `UserPromptSubmit` delayed past the exit therefore acks nothing: the exit already moved the launch's `sending` / `sent` inputs to `unconfirmed` (mvp §2.3), and a human settles them with `chaldea input resolve`. PR 10's late attributable ack (`unconfirmed` → `acked`, mvp §2.3) applies only to hooks of a live launch, after the ack window expired. A launch that exits before binding has its buffer resolved by `resolve_unbindable` in the exit job (§2.9), and a `SessionStart` arriving afterwards is row 5; a launch that binds before exiting has its buffer promoted once, in the binding job, and the promotion trigger (store §2.4: `buffered → terminal` once) keeps a later `resolve_unbindable` from touching it.

### 2.8 Token Check

`verify_hash_in` (http §2.4) gains, for kind `hook`: the token has both `thread_id` and `launch_id`, the launch exists, and its thread is the token's thread; otherwise `Invalid` (401). The launch need not be current, running, or bound: late hooks are kept (rows 3, 5, 6), and revocation (PR 10 at restart, §2.11 close) ends a token. The guard's step-4 lookup, the pre-read, and the job each run it, and the job's result is the one every target is derived from, so a revocation committed after the pre-read refuses the job with 401 and nothing changes (row J4).

### 2.9 Buffer

A launch's buffer is its deliveries with routing `buffered`; nothing else holds it.

```rust
pub struct HookConfig {
    pub emitter_name: String,        // "chaldea": links[0] must run the emitter binary
    pub pid_wait: Duration,          // 1 s: early hooks wait this long, inside the deadline, for launch_started
    pub max_chain: usize,            // 16 links walked from the emitter
    pub buffer_max_count: usize,     // 32 buffered deliveries per launch
    pub buffer_max_bytes: u64,       // 8 MiB of buffered bodies per launch
    pub buffer_ttl: Duration,        // 30 s from the earliest received_at of the buffered deliveries
    pub body_budget: u64,            // 32 MiB of hook bodies held from read to job end (§2.2)
    pub max_descendants: usize,      // 1024 attributed identities per launch
}
pub fn expire(tx: &mut WriteTx<'_>, launch: LaunchId, cfg: &HookConfig) -> Result<usize, StoreError>;
pub fn resolve_unbindable(tx: &mut WriteTx<'_>, launch: LaunchId) -> Result<usize, StoreError>;
impl ReadTx<'_> { pub fn buffered_summary(&self, l: LaunchId) -> Result<BufferedSummary, StoreError>; }   // count, body bytes, MIN(received_at); no bodies read
```

- **Meta** (`BufferedHook.meta`, JSON, version 1): `{ "v": 1, "thread_header", "agent_header", "emitter_version", "attempt", "chain": [{ "pid", "start_time", "name" }], "chain_end", "intake": <IntakeFacts or null> }`; `BufferedHook.body` is the request body. A meta that does not parse at promotion (only a bug or a foreign writer can cause it) promotes to `raw_unknown` `schema_mismatch` with the body as raw.
- **Bounds**, checked in the job that would add a delivery, from `buffered_summary`: the count after adding ≤ 32 and the summed body bytes after adding ≤ 8 MiB; otherwise overflow (row 12).
- **Time.** `expire` reads `buffered_summary` (no bodies) and, when `recorded_at − MIN(received_at) > buffer_ttl`, promotes every buffered delivery of the launch to `raw_unknown` `unbound` in receipt order. `received_at` is the record job's commit time (K4), so the earliest one, not the first in receipt order, is the oldest; measured from commit, a buffer lives at least 30 s from its first intake. Both stamps are the store clock's fixed format `YYYY-MM-DDTHH:MM:SS.mmmZ` (`core::ids::Timestamp`), parsed by `buffer.rs` itself into milliseconds since the epoch (the daemon has no date crate); a negative age (the clock stepped back) or an unparsable stamp does not expire. `expire` runs at the start of every delivery job of an unbound launch (step 3; a bound launch has no buffer, so it is skipped) and in PR 10's timer tick. **While PR 9 ships without PR 10, an idle buffer expires only when the next hook of its launch arrives**; launches exist only through PR 10's supervisor, so no production launch is affected before then.
- **Launch that can no longer bind.** `resolve_unbindable` promotes every buffered delivery of the launch to `raw_unknown` `unbound`. PR 10 calls it in the job that appends `launch_exited`, and at restart for each of `buffered_launches()` before serving (mvp §2.9; formal `restart`). Row 5 covers a hook arriving after that.
- After an overflow or expiry the buffer is empty and later unbound hooks buffer again; if the launch then binds, only those are promoted. The earlier ones stay `raw_unknown`, visible, never applied out of order.

### 2.10 Single Deciding Points

| Operation several requests can hit at once | Decided by | Why a second caller cannot act |
|---|---|---|
| Duplicate `(launch_id, emit_id)` (a retry racing its first attempt) | the delivery job's step 1 + `hook_delivery_record`, in one `BEGIN IMMEDIATE` transaction on the one writer | jobs run one at a time, so the second reads the first's row and replays its reply; `PRIMARY KEY (launch_id, emit_id)` refuses a second process's insert. The pre-read's replay is a shortcut, never the decision |
| Which hook binds, and that no earlier hook is overtaken | the binding order (`IntakeOrder`: a `SessionStart` queued only after every lower ordinal of its launch was handed to the writer or released) and then the job reading `L.session_id IS NULL` | the FIFO writer runs every lower job before the binding job (K12); a second eligible `SessionStart` reads the committed binding and takes row 7; trigger `events_session_bound_once` refuses a second `session_bound` for the launch |
| Receipt ordinals | `IntakeOrder`'s mutex: seed, read, and increment in one critical section | each caller gets a distinct value; `UNIQUE (launch_id, receipt_ordinal)` is the backstop |
| Intake facts against the ordinal | the same critical section | facts and ordinals are taken in one order |
| `emit_id` reused by another launch of the thread | the job's step 2 (`event_by_key`) | `UNIQUE (stream_id, idempotency_key)` is the backstop |
| Buffer bounds and expiry | the job reading `buffered_summary(launch)` | the count and bytes include every committed delivery; a concurrent delivery is a later job |
| Promotion against overflow, expiry, `resolve_unbindable` | whichever job runs first | the trigger allows `buffered → terminal` once (store §2.4); the later job reads no buffered rows |
| Binding against the launch's exit | the job: rows 5–6 and `reduce(SessionBound)` read the projection that `launch_exited` updates in its own job | the writer orders them |
| Descendant records | the job, `INSERT … ON CONFLICT DO NOTHING` on `PRIMARY KEY (launch_id, pid, start_time)`, cap counted in the same transaction | concurrent chains of one launch insert each identity once |
| Revocation against a hook | `verify_hash_in` inside the job | committed before or after in the writer's order |
| Body budget | `Semaphore::try_acquire_many_owned` | permits are atomic |

### 2.11 Status and Disposition Table

What the client observes per outcome, the layer that decides it, and whether the emitter retries (§2.13). Rows P, G, H, J1, J2, J4–J6, J8 store nothing; J-ok stores or replays one delivery; J7's outcome is unknown until a later read (a retry with the same `emit_id` replays a delivery that did commit).

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
| H1 | `X-Chaldea-Emit-Id` missing, repeated, or not a UUID; another listed `X-Chaldea-*` header repeated | handler 1 | 400 `bad_request` | final |
| H2 | body budget exhausted | handler 2 | 503 `busy` | retry |
| H3 | body not complete by the deadline (held or slow) | handler 3 | 408, or a reset when body bytes are still in flight (K2) | retry |
| H4 | body ended early (client closed) | handler 3 | — (client gone) | — |
| H5 | pre-read: token invalid now / reader busy / store closed / other store error / cut off | handler 4 | 401 / 503 `busy` / 503 `shutting_down` / 500 / 408 | final / retry / retry / final / retry |
| H6 | walk not finished, or binding order not released, by the deadline | handler 6 / 8 | 408 | retry |
| J-ok | the job recorded a delivery, or replayed one | job | 200, empty body | final |
| J1 | `WriteError::Expired` (the deadline passed before the job held the lock) | job | 408 | retry |
| J2 | `WriteError::Busy`, `RolledBack(StoreError::Busy)`, `SubmitError::QueueFull` | job / submit | 503 `busy` | retry |
| J3 | registry closing; `SubmitError::Closed` | observer / submit | 503 `shutting_down` | retry |
| J4 | `Rejected(401)` (revoked after the pre-read); `Rejected(400)` (step 2) | job | 401 / 400 | final |
| J5 | `RolledBack(StoreError::DiskFull)` | job | 503 `disk_full` | retry |
| J6 | `RolledBack(other)`, `Panicked`, `Rejected(Store)`, `WouldDeadlock` | job | 500 `internal` | final |
| J7 | `CommitUnknown`, `WriterGone` (each reported to `LifecycleSink`), or no reply by `deadline + 100 ms` | job | 503 `outcome_unknown` | retry |
| J8 | `WriteError::Failed`, `SubmitError::Failed` (reported to `LifecycleSink`) | job / submit | 503 `store_failed` | retry |
| — | client disconnect after submission | observer | — | — |

A rejection decided before the body (rows G1–G8, H1, H2) is sent before the handler reads any body byte (http §2.2 "The body boundary"): a client that withholds its body until it reads the response gets the status (the ordering cases of §7). With body bytes in flight, a client may observe a reset instead of the status (K2); B-8 records which (§7.1).

### 2.12 Descendant Attribution (PR 9 / 10)

A matching creation time identifies a process, not its provenance (`docs/design/pty-windows.md` §2.5), so `extra` must contain only processes proven to descend from the launch. PR 9 proves it at the one moment it can: at intake, the walk follows parent links upward from a live emitter to `L`, each link proven within one process generation (§2.5). The links strictly between the emitter and `L` are then descendants of `L` at that instant, recorded with their identities. The emitter itself is not recorded (it exits within 3 s). A chain that does not reach `L` through proven links records nothing; an unprovable chain creates no `extra`.

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
CREATE TABLE launch_descendants_incomplete (     -- a launch whose list reached the cap
  launch_id TEXT PRIMARY KEY, since TEXT NOT NULL
) STRICT, WITHOUT ROWID;
-- update and delete refused by trigger (RAISE(ROLLBACK)) on both, as for hook_deliveries
CREATE TRIGGER events_session_bound_once BEFORE INSERT ON events WHEN NEW.kind = 'session_bound' BEGIN
  SELECT RAISE(ROLLBACK, 'events_session_bound_once: a binding without a launch, a second binding, or a binding of another launch')
  WHERE NEW.launch_id IS NULL
     OR EXISTS (SELECT 1 FROM events WHERE stream_id = NEW.stream_id AND kind = 'session_bound' AND launch_id IS NEW.launch_id)
     OR NEW.launch_id IS NOT (SELECT launch_id FROM events WHERE stream_id = NEW.stream_id AND kind = 'launch_reserved'
                              ORDER BY seq DESC LIMIT 1);
END;
```

```rust
pub struct Descendants { pub ids: Vec<ProcessIdentity>, pub complete: bool }
impl WriteTx<'_> { pub fn descendants_record(&mut self, l: LaunchId, ids: &[ProcessEntry], cap: usize) -> Result<u64, StoreError>; }
impl ReadTx<'_>  { pub fn descendants(&self, l: LaunchId) -> Result<Descendants, StoreError>; }
```

- `descendants_record` inserts each identity not yet present while the launch has fewer than `cap` rows (counted in the transaction). When a new identity would exceed the cap, it inserts the launch into `launch_descendants_incomplete` (once) instead: the cap never drops silently. `descendants` returns `complete: false` for such a launch, and PR 10 must then not treat a kill with the recorded list as a complete death proof (§4).
- Neither table is a projection (like `hook_deliveries`, store §2.1 "Projections"); rows are kept forever (store D9). PR 10 passes `descendants(launch).ids` as the full `extra` list on every `kill_tree` (mvp §4 PR 10).
- **Why the binding trigger.** One binding per launch, only for the stream's current launch, is an invariant SQLite can express, so it is a trigger (store §2.1 rule 1: "SQLite is the proof, Rust is the second line"); row 7 is the Rust check that returns the typed outcome first. It reads `events` only, so it holds while projections are dropped, and its `NEW.launch_id IS NULL` clause keeps a `NULL` operand from passing (store S10). If PR 13 / 14 adopt rebinding (§8 D2), a later migration drops it.
- `EventAnnex` gains `Hook { truncated: bool }`, accepted only on a `raw_unknown` whose source kind is `hook` (row 1); the annex column needs no DDL change. The migration's schema fingerprint is added to `schema.rs`.

### 2.13 The Emitter: `chaldea hook-emit <agent>`

**Dispatch.** `main` compares `argv[1]` with `hook-emit` before any runtime, logging, or configuration initialization and calls `hook_emit::main(argv)`, which never returns (mvp §3 "Single binary"). The command form is fixed: `<absolute path to chaldea> hook-emit <agent>`, nothing else, because Codex hashes the command string into its hook trust (arch §5; §8 D1).

**Environment** (mvp §2.5 "Token kinds"): `CHALDEA_HOOK_URL` (exactly `http://127.0.0.1:<port>/hook` or `http://localhost:<port>/hook`, port 1–65535 with no leading zero; the spike's `parse_local_url` rules, K8), `CHALDEA_HOOK_TOKEN` (`chd_` + 43 base64url characters), `CHALDEA_THREAD_ID` (sent as `X-Chaldea-Thread-Id` when set). A missing or malformed URL or token: exit 0 at once without reading stdin. No other variable is read; no log file (§8 D8).

**Budget.** Let `T0` be the start of `hook_emit::main`. Fixed instants: the watchdog at `W = T0 + 3000 ms`; the work end `E = T0 + 2900 ms`; the stdin end `I = T0 + 1500 ms`. Attempt `k` starts at `s_k` and has the absolute deadline `A_1 = min(E, s_1 + 1800 ms)`, `A_k = min(E, s_k + 1000 ms)` for `k = 2, 3`.

**Run**, with the checks in this order:

1. Install a panic hook that calls `std::process::exit(0)` without printing; start the watchdog thread, which calls `exit(0)` at `W`.
2. Read stdin on a thread, up to 4 MiB + 1 bytes, waiting until `I`: EOF in time → the body; more than 4 MiB → its first 4 MiB and `X-Chaldea-Body-Truncated: 1`; still open at `I`, or a read error → exit 0 with no request.
3. `emit_id = Uuid::new_v4()` once.
4. For `k = 1, 2, 3`: the backoff before attempt `k` is 0, 50 ms, 100 ms; compute `s_k = now + backoff`; if `s_k + 300 ms > E`, stop without sleeping; else sleep the backoff and run the attempt, sending `X-Chaldea-Attempt: k`. An attempt: `TcpStream::connect_timeout(127.0.0.1:<port>, min(500 ms, A_k − now))`; write the head and body; read until the first CRLF (at most 1 KiB). Before every socket call the remaining time `A_k − now` is computed; if it is zero, the attempt ends as a timeout without calling `set_read_timeout` / `set_write_timeout` (which reject `Duration::ZERO`). The status line must match `HTTP/1.` + one digit + `SP` + three digits followed by `SP` or `CRLF`; anything else is a transport error.
5. **Retry on**: a connect error or timeout; a write or read error, a timeout, a reset, or EOF before a complete status line (the request may or may not have been recorded; the same `emit_id` makes a retry safe); status 408 or any 503 (a `retry_after` is not honoured: the fixed backoff fits the budget). **Stop on** any other status.
6. `exit(0)`.

Nothing is written to stdout or stderr on any path (Claude injects hook stdout into context, and exit 2 would block the prompt; spike §4.3). mvp CL-23 holds by construction: the watchdog bounds every path to 3 s, and exit code 0 is the only one used, panics included. With the deadlines above, a first attempt that gets no reply ends at `T0 + 1.8 s` and leaves a second attempt about 1.05 s. **Delivery is best effort across a stopped daemon**: a hook fired while the daemon is down or restarting is lost (its emitter gets connect errors or 401s from revoked tokens and exits 0); nothing in PR 9 queues it.

**Windows.** The same console-subsystem `chaldea.exe` (prior-art row 9; spike C-5 `go`: no window for hook launches); it inherits the CLI's console and creates none. A connect to a closed loopback port can take about 2 s to be refused on Windows (A2); the 500 ms connect bound keeps that inside the attempt. `exit` from the watchdog ends a main thread blocked in `connect`, `send`, or `recv`. No `cfg` in the emitter.

### 2.14 FakeCli Additions (9a)

- `echo --emitter <path> [--agent <claude|codex>] [--session-id <id>] [--nested]`: at start, runs the emitter once (`<path> hook-emit <agent>`, agent default `codex`) with `{ "hook_event_name": "SessionStart", "session_id": <id or a random UUID>, "transcript_path": null }` on stdin, and per input line a `UserPromptSubmit` with the line as `prompt`. `--nested` starts a nested FakeCli (`echo --nested-child`, the same binary, inheriting the environment) that emits a `SessionStart` with another session id before and after the parent's (spike `src/fake_cli.rs:244-269`, B-11).
- `hooks --emitter <path> [--agent <claude|codex>] --script <json>`: runs a list of steps in order: `{ "emit": <payload> }` (one emitter process with the payload on stdin; waits for its exit), `{ "concurrent": [<payload>, …] }` (one emitter each, all started, then all awaited), `{ "nested": [<step>, …] }` (a nested FakeCli `hooks` with those steps, inheriting the environment; waits for it), `{ "wait_file": "<path>" }` (waits until the file exists, at most 10 s, so a test can hold the next step). It reports each emitter's exit code, elapsed time, and stdout length on the report stream.

FakeCli stays free of workspace dependencies other than `core` (mvp §3); it starts the emitter with `std::process::Command`.

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **Where routing is decided** | A. In the handler with in-memory launch state | spike `receiver.rs` (a `Mutex` over bound session and events) | Rejected: cross-call state outside the database; two concurrent `SessionStart`s need one decider. |
| | B. One store job per delivery reading the projections (§2.7) | store §2.3 "Hook deliveries" | **Adopted.** |
| **Binding order** | A. Whichever job reaches the writer first binds | — | Rejected: a `SessionStart` can overtake an earlier `UserPromptSubmit` still in its walk, giving `SessionBound → Stop → PromptSubmitted`; of two sessions the later-received can win. |
| | B. A `SessionStart` is queued only after every lower ordinal of its launch was handed to the FIFO writer or released | K12 | **Adopted** (§2.4). |
| | C. Serialize every hook of a launch through the intake order | — | Rejected: only binding needs it; serializing all hooks makes one slow walk delay every hook. |
| **Hook source and trust** | A. A TCP peer pid or an OS-attested sender | Windows `GetExtendedTcpTable` | Rejected: macOS has no TCP peer-pid query (`LOCAL_PEERPID` is for Unix sockets), and same-account processes are trusted anyway (trust boundary). |
| | B. The hook token authenticates the launch; the daemon walks from the emitter's reported pid to separate unintentional nested emissions, with `links[0]` required to run the emitter binary | prior-art row 6c | **Adopted**; it does not resist a token holder forging headers, which the trust boundary accepts. |
| | C. The chain the emitter reports in a header | spike `X-Chaldea-Emitter-Ancestry` (K9) | Rejected: `extra` needs parent links the daemon proved (§2.12). |
| **Walk location** | A. In `chaldea-daemon` | — | Rejected: `unsafe` is forbidden there, and OS code belongs in `pty-runtime` (AGENTS.md). |
| | B. Public `pty_runtime::process` | PR3 D7 | **Adopted.** |
| **Proof of a parent link** | A. Trust `ppid` | — | Rejected: a stale Windows `ppid` can name a later process, and a macOS parent can exit and its pid be reused between two lookups. |
| | B. Order of start times alone | — | Rejected: wall-clock stamps (K7); a clock step defeats it. |
| | C. macOS read, read, re-read (reparenting proves the parent's life); Windows two snapshots bracketing open handles, plus creation order | spike `src/os/windows.rs:126-128` (Toolhelp) | **Adopted**; the Windows clock residual is D10. |
| | D. `NtQueryInformationProcess` for the parent | — | Rejected: an `ntdll` call with the same staleness. |
| **Receipt ordinal** | A. Assigned in the record job (commit order) | — | Rejected: a slow walk or pid wait reorders it (store §3 "Hook order"). |
| | B. Per-launch counter at intake under a mutex, seeded from the stored maximum | store §2.3 | **Adopted.** |
| | C. Counter keyed by token | — | Rejected: correctness would rest on one hook token per launch. |
| **Intake facts** | A. Recomputed at promotion | — | Rejected: the model applies buffered hooks with the facts of their intake (K10). |
| | B. PR 10's probe at intake, stored opaque, with the attempt number so retries are marked unreliable | — | **Adopted.** |
| **Interpreting the binding `SessionStart`** | A. Interpret before recording the binding | — | Rejected: `interpret` would see an unbound launch. |
| | B. Record `session_bound` and its status first, re-read, interpret, then promote, in one transaction | — | **Adopted** (§2.7 step 7). |
| **Hook meaning** | A. PR 9 decodes every kind | — | Rejected: adapter knowledge (PR 13 / 14) and supervisor facts (PR 10) are not PR 9's. |
| | B. `HookAdapter` seam; PR 9 decodes only the binding fields | — | **Adopted.** |
| **Claude's expected session** | A. An annex on `launch_reserved` | store annex | Rejected for now: a store change PR 9 cannot fill, since PR 10 writes `launch_reserved`. |
| | B. Derived from stored facts through `HookAdapter::expected_session` | — | **Adopted**; PR 10 may replace the default with a stored value. |
| | C. Claude binds like Codex (ancestry only) | — | Rejected: mvp §2.9 binds Claude by `--session-id`, the verified rule (spike §4.3). |
| **Hooks after exit** | A. Interpret them, the adapter checking the exit | — | Rejected: every adapter would repeat the check, and the store would advance `last_turn_start_seq` for a dead launch. |
| | B. Keep them as `raw_unknown` `unbound` | — | **Adopted** (row 5). |
| **Buffer expiry** | A. A daemon timer task per launch | — | Rejected: a task holding state the database holds. |
| | B. `expire` in each delivery job and in PR 10's tick; `resolve_unbindable` at exit and restart | — | **Adopted.** |
| **Buffer overflow** | A. Refuse the new hook (503) | — | Rejected: the emitter would retry into the same overflow and lose it. |
| | B. The whole buffer and the new hook become `raw_unknown` `unbound` | mvp §2.9 | **Adopted.** |
| **Conflicting `SessionStart`** | A. Rebind the launch | — | Not in PR 9 (§8 D2); PR 13 / 14 decide. |
| | B. `raw_unknown` `foreign_session`, binding unchanged | spike M3 (ambiguity never re-binds) | **Adopted.** |
| **`emit_id` reused by another launch of the thread** | A. Change the key to include the launch | — | Rejected: a change of mvp §2.1's public key format. |
| | B. Refuse with 400, nothing stored | — | **Adopted**; only a forged or broken request meets it. |
| **Emitter HTTP client** | A. `minreq` | prior-art row 5b | Rejected: one timeout for connect and read. |
| | B. Hand-written std POST | spike `http_client.rs` | **Adopted.** |
| **Emitter retries** | A. None | spike `emitter.rs` | Rejected: a busy writer or a reset loses the hook. |
| | B. Up to 3 attempts with absolute per-attempt deadlines, one `emit_id`, retry on transport errors, 408, and 503 only | — | **Adopted.** |
| | C. Retry every non-200 | — | Rejected: 4xx are final; retrying them only spends the budget. |
| **Stdin over 4 MiB** | A. Drop it (no request) | spike B-9 | Rejected: a hook silently dropped (arch §1). |
| | B. Send the first 4 MiB marked truncated; keep it as `raw_unknown` `unparseable` with an annex marker | — | **Adopted** (§8 D5). |
| **Large in-flight body on rejection (H5)** | A. Drain the body after an early rejection (lingering close) | spike F-3c | Rejected: a change to PR 7's connection handling for one route whose only client retries and needs no status. |
| | B. Record the observation per OS; the emitter treats a reset as retryable | — | **Adopted.** |
| **Descendant storage** | A. In memory | — | Rejected: lost at restart, when PR 10 needs it most. |
| | B. An annex on the hook's event | — | Rejected: buffered and foreign deliveries have no event, yet their chains prove descendants. |
| | C. A non-projection table written in the delivery job, with an incompleteness marker at the cap | `hook_deliveries` | **Adopted.** |
| **Binding trigger** | A. Rust check only | — | Rejected: store rule 1 puts expressible invariants in SQLite. |
| | B. `events_session_bound_once` | store §2.4 triggers | **Adopted** (§2.12). |
| **Hook body memory** | A. Only the 4 MiB per-request cap | — | Rejected: 128 connections × 4 MiB. |
| | B. A 32 MiB byte budget held from read to job end; 503 `busy` when full | http blob slots | **Adopted.** |
| **Stream of a hook `raw_unknown`** | A. `host` when the launch is unbound | — | Rejected: the token names the thread, and the host stream is unreadable to agents. |
| | B. Always the token's thread | — | **Adopted.** |
| **Emitter crate** | A. A module of `crates/cli` | mvp §3 | **Adopted**; PR 8 or 9a creates the crate, whichever merges first. |
| | B. A separate crate | — | Rejected: one binary either way; no isolation gained. |
| **PR size** | A. One PR | mvp §4 | Rejected: large, and the emitter is testable alone. |
| | B. 9a (process lookup, emitter, FakeCli) and 9b (ingest, store v2, B-8, e2e) | http D1 | **Adopted.** |

## 4. Same-Class Inventory

Every later PR that consumes hook facts or adds hook kinds, and the seam it uses. None changes PR 9's types.

| PR | Consumes or adds | Seam | What PR 9 provides; what that PR must do |
|---|---|---|---|
| 10 supervisor | launch reservation and tokens; `extra`; readiness, gate, ack; restart; timers | `IntakeProbe`, `HookAdapter` (wraps the adapters' decoders with supervisor facts), `buffer::expire`, `buffer::resolve_unbindable`, `ReadTx::descendants`, `launch_started` | Mints one hook token per launch with `thread_id` and `launch_id` (§2.8) and sets `CHALDEA_HOOK_URL`, `CHALDEA_HOOK_TOKEN`, `CHALDEA_THREAD_ID`. For Claude passes `--session-id <launch_id>` on a thread without a bound session and `--resume <thread session>` otherwise, or replaces `expected_session` with a stored value. Records `launch_started` with the backend's identity (the pid wait ends on it). Calls `resolve_unbindable` in the `launch_exited` job and at restart for every `buffered_launches()` launch before serving, and `expire` on its tick. Treats a hook with `intake_reliable: false` as no ack and no readiness evidence (§2.6). Uses `binding_ordinal` to order a buffered `Stop` against the binding `SessionStart`. Passes `descendants(launch).ids` on every `kill_tree`, and does not report `death_confirmed` from that list alone when `complete` is false. Consumes: `session_bound` and its `bind_status` events, each selected hook through `interpret` with intake facts, `raw_unknown` (no state effect). Late acks apply only to hooks of a live launch (§2.7 "Hooks after exit"). |
| 12 fixtures | real payloads per CLI version | `AcceptedHook.body` / `json` in `interpret`; `raw_unknown.raw` | A capture launch wraps the adapter to copy each accepted body; foreign and unparseable bodies are `raw_unknown` events with the exact bytes. Captures session-switch payloads (`/clear`, in-session `/resume` for Claude; `/new`, `/clear` for Codex: their `source` and new session id, A1) for PR 13 / 14's decision. |
| 13 Claude | decoders: `UserPromptSubmit`, `Stop`, `SessionEnd`, `PermissionRequest`, `PreToolUse`, `PostToolUse`, `Notification`; `SessionStart` readiness | `HookAdapter::interpret` for `Agent::Claude` | `SessionStart` reaches `interpret` once in the binding job (bound state visible) and again for every later same-session `SessionStart` (row 8). **Validates a received `transcript_path` before opening it** (absolute, a regular file, not a device, pipe, or socket; PR 9 stores it as received). Verifies Claude's ancestry on real hosts before any switch to rule `Ancestry` (§8 D3), and `--resume` keeping the session id (§2.6). **Decides session switches before merging** (§8 D2): from PR 12's fixtures, whether the launch follows a new session (rebind, a new turn start for PR 10's gate, a new generation for the tailer, the binding trigger dropped by migration) or stays on the first. |
| 14 Codex | decoders including `Interrupt`; command string and stable emitter path; session switches | `HookAdapter::interpret` for `Agent::Codex`; §2.13 command form | Registers `<stable path> hook-emit codex`; the path must not change across upgrades, or trust renews (arch §5). **Resolves and launches the native `codex` binary**, or refuses at spawn with a clear error (and reports it in the doctor) when the configured `codex` is a wrapper (an npm shim, `codex.cmd`, a version-manager shim): a wrapper between the launched pid and the CLI that runs hooks fails eligibility, so every hook would be `foreign_session`. Tests a wrapper and a native launch as controls. Decides session switches for Codex with PR 13 (§8 D2). |
| 20 extensions | `attention` on raise and clear; rescue reads `raw_unknown` | store commit notifications; thread streams | The binding's `status_changed` events are ordinary events; `raw_unknown` of hooks is on the thread stream. |
| 21 doctor | hook round trip through `hook-emit`; Codex wrapper detection | the public route and env | The emitter exits 0 silently by design, so the doctor observes the daemon side (a delivery recorded), not the emitter's exit. |
| 22 fuzz | the hook payload decoder | `hook::payload::decode(&[u8]) -> Decoded` (pure, no I/O) | A fuzz target that never panics and classifies every input as one of `Hook`, `Unparseable`, `SchemaMismatch`. |

## 5. Resource Paths

| Resource | Created by | Bound | Ends on |
|---|---|---|---|
| Connection | PR 7 accept loop | 128, one request each | PR 7 (http §5) |
| Request body and its permit | handler step 3 | 4 MiB each; 32 MiB of permits across requests | the writer dropping the job closure; the request ending before submission |
| Walk (`spawn_blocking`) | handler step 6 | 16 links; Windows two snapshots and one handle per link | returns on its own, closing its handles; the handler stops waiting at the deadline (408) |
| Pid wait and its commit receiver | handler step 7 | `min(intake + 1 s, deadline)` | start, exit, launch replaced, channel closed, or the bound; the request dropped |
| `IntakeTicket` | handler step 5 | one per request | the request ending before submission; the writer dropping the closure that owns it |
| Binding-order wait | handler step 8 | the deadline | every lower ordinal of the launch released; the request dropped |
| Delivery job and its observer | handler step 9 through `ctx.write` | one per request | the writer's reply (store §5); PR 7's shutdown |
| `IntakeOrder` entry | first hook of a launch in a run | one counter and a pending set per launch | daemon exit |
| Buffered delivery | row 12 | 32 and 8 MiB per launch; 30 s | binding (promotion), overflow, expiry, `resolve_unbindable` at exit or restart |
| Descendant row | job step 4 | 1024 per launch, then the incompleteness marker | kept (store D9) |
| Emitter process | the CLI, per hook | 3 s | `exit(0)` after the reply or a final status, or the watchdog |
| Emitter stdin thread | emitter step 2 | 4 MiB + 1 bytes, until `T0 + 1.5 s` | EOF, error, or the process exit |

**Endings of a hook request** (each row of §2.11 is one): a hyper parse rejection or slow-head EOF; a guard rejection; a header rejection; the body budget; the body deadline, size, or early end; a pre-read failure or replay; the walk or the binding order cut by the deadline; the job's outcome (recorded, replayed, refused, expired, busy, rolled back, panicked, commit unknown, writer gone, failed, no reply by the grace); a client disconnect before submission (nothing written; the ticket and permit released) or after it (the job finishes on its observer, holding the permit); daemon shutdown (registration refused → 503 `shutting_down`; an admitted job is answered by the writer, http §5).

**Waits.** Head: hyper's `header_read_timeout`. Body, pre-read, walk, pid wait, binding order, job admission: the request deadline (5 s from the first head byte). Job reply: deadline + 100 ms. Pid wait: also 1 s from intake. The intake mutex: a map operation and a non-blocking probe. The emitter: the fixed instants of §2.13.

**Windows and macOS.** The daemon side has no `cfg`: OS differences are in `pty_runtime::process` (Toolhelp and handles vs `proc_pidinfo`) and in K2's reset behaviour, which B-8 records per OS.

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked on a running daemon (PR 8's binary once it exists; until then the test server of `crates/daemon/tests/support` with the hook mount, or the server built in `crates/cli/tests/hook_e2e.rs`). `$O` is an owner token, `$H` a launch's hook token, `$P` the port.

| ID | Criterion (EARS) | Source | How to observe |
|---|---|---|---|
| PR9-P-1 | When a Codex-shaped launch's CLI fires `UserPromptSubmit` before `SessionStart` through `chaldea hook-emit`, the thread stream shall show `session_bound` followed by the earlier hook's events, in the order the daemon received them. | mvp §2.9; K11 | FakeCli `hooks` with the steps `emit UserPromptSubmit`, `wait_file`, `emit SessionStart` and a test adapter; `GET /api/events?stream_id=thread:<t>` with `$O`. |
| PR9-P-2 | If a nested CLI inheriting the launch's environment emits hooks through the unmodified emitter, then the launch shall stay bound to its own session and the nested hooks shall appear as `raw_unknown` `foreign_session`. | prior-art row 6c; spike B-11 | FakeCli `hooks` with a `nested` step held before the parent's `SessionStart`; the stream. |
| PR9-P-3 | If a `/hook` request is malformed at the HTTP level, unauthenticated, misdirected, or too large, then the daemon shall reject it and the thread and host streams and the delivery table shall not change; if its body is not a readable hook payload, then the daemon shall accept it and keep the body as `raw_unknown`. | invariant 5; spike B-8; arch §1 | `curl` probes of §2.11; `head_seq` of both streams before and after; a `not json` body. |
| PR9-P-4 | When `chaldea hook-emit` runs, it shall exit 0 within 3 s with empty stdout, whether the daemon is down, slow, rejecting, or accepting. | arch §2; CL-23 | `time chaldea hook-emit codex < payload.json > out; echo $?` against each daemon condition. |
| PR9-P-5 | When a delivery has reached a terminal routing (`appended` or `raw_unknown`), a post with the same `X-Chaldea-Emit-Id` shall add no event. | mvp §2.1 | Two `curl` posts with the same header; the stream. |
| PR9-P-6 | If a hook body is not JSON or lacks the hook fields, then the thread shall show it verbatim as `raw_unknown`. | arch §1; P-14 | Post `not json` and `{"x":1}`; `GET /api/blobs` or the inline `raw`. |
| PR9-P-7 | When the B-8 suite runs on `windows-latest` and `macos-latest`, every case shall end with its listed observation, and the per-OS outcomes of the in-flight-body cases shall be in the CI log. | arch §5 open item | The `workspace` job's B-8 observation step (§1). |

### 6.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Test that falsifies it |
|---|---|---|---|
| PR9-CL-1 | When a hook delivery is recorded, its first reply, receipt ordinal, and routing shall commit in one job before the reply is sent, and a retry with the same `(launch_id, emit_id)` shall get the first reply and change nothing. | mvp §4 PR 9; mvp CL-10; store §2.3 | `hook_ingest::retry_replays_first_reply`; `hook_ingest::concurrent_retries_dedup_in_the_job` (writer held so both posts pass the pre-read; one delivery, one event, one adapter call); `hook_ingest::reply_waits_for_commit` |
| PR9-CL-2 | A retry of a buffered delivery after its launch became bound shall get the stored first reply, and the stored routing shall be the promoted one, with no new event. | mvp §4 PR 9 | `hook_ingest::retry_after_binding_keeps_first_reply` (checks `hook_delivery.first_reply` and `.routing` separately, and the stream's event count) |
| PR9-CL-3 | Distinct `emit_id`s with identical bodies shall each create a delivery and an event. | mvp CL-10 | `hook_ingest::identical_bodies_are_two_deliveries` (two deliveries, two events) |
| PR9-CL-4 | A buffered hook shall be durable before 200 is returned. | mvp §4 PR 9; store D5 | `hook_ingest::buffered_durable_before_200` (with the writer held, no 200 arrives; after its release the 200 arrives and a read sees the buffered row; a committed job's durability is store D5's, tested by PR 6) |
| PR9-CL-5 | Concurrent hooks of one launch shall receive distinct receipt ordinals in the order of their intake, with intake facts taken in the same order, and ordinals shall continue from the stored maximum after a restart. | store §2.3; §2.10 | `hook_ingest::concurrent_ordinals_unique` (64 concurrent posts, a probe recording its call order); `hook_ingest::ordinals_continue_after_restart` (stored ordinals 1–3, a new `HookIngest` over the same store: next is 4) |
| PR9-CL-6 | A `SessionStart` shall not bind while a lower receipt ordinal of its launch is pending; buffered hooks received before it shall be promoted before later hooks are appended; of two eligible `SessionStart`s of different sessions, the one received first shall bind. | mvp §2.9; K10; §2.4 | `hook_ingest::binding_waits_for_earlier_hook` (hold the first hook's walk; `SessionStart` 2 and `Stop` 3 proceed; release: stream is `session_bound`, `UserPromptSubmit` 1, `Stop` 3); `…::earlier_session_start_wins` (two sessions, the earlier held: it binds, the later is `foreign_session`); `…::released_gap_does_not_block` (ordinal 1 disconnects: 2 binds at once); unit `store triggers::session_bound_once` |
| PR9-CL-7 | A launch shall bind only through §2.5's rule: Claude by its expected session, Codex by an eligible chain whose first link runs the emitter binary; a nested CLI that inherits the environment and runs the unmodified `chaldea hook-emit` shall not bind. A process holding the hook token that forges headers is inside the trust boundary and is not covered. | mvp CL-17; prior-art row 6c; trust boundary | `hook_ingest::claude_binds_by_expected_session`, `…::codex_binds_by_ancestry`, `…::nested_chain_does_not_bind`, `…::non_emitter_first_link_is_not_eligible` (synthetic chains); `hook_e2e::nested_fake_cli_does_not_bind` (real processes, both OSes) |
| PR9-CL-8 | A later hook of a bound Codex launch shall be selected only when its own chain, walked at its receipt, is eligible. | mvp §4 PR 9 | `hook_ingest::later_hooks_revalidate_ancestry` |
| PR9-CL-9 | A `SessionStart` with another session for a bound launch shall be `raw_unknown` `foreign_session` and leave the binding unchanged. | mvp §4 PR 9 | `hook_ingest::conflicting_session_start_is_foreign` |
| PR9-CL-10 | A hook whose `session_id` is not the bound session (or, while unbound: not Claude's expected session, or for Codex from an ineligible chain) shall never reach `interpret` and shall be `raw_unknown` `foreign_session`. | formal §1; mvp §2.9 | `hook_ingest::foreign_session_never_interpreted` (a counting adapter; bound and unbound cases) |
| PR9-CL-11 | When a selected hook of an unbound launch is buffered, its intake shall append no event and change no projection; at binding its events shall be applied with its intake facts. | formal §3 hole d | `hook_ingest::unbound_intake_changes_nothing` (an adapter returning `readiness_observed` with a `WriteBasis` annex from the intake facts: every `proj_*` row and both heads equal before binding; after binding the launch's `write_basis` equals the intake basis) |
| PR9-CL-12 | At binding, the buffered hooks shall be applied after `session_bound` and its status events, in receipt order, each with its own intake facts and `binding_ordinal`, and only those whose session equals the bound one and whose stored chain is eligible; `interpret` of the binding `SessionStart` shall see the bound launch; an error in any of it shall roll back the binding. | mvp §2.9; K10 | `hook_ingest::promotion_in_receipt_order` (receipt ordinals 1, 2 committed 2, 1), `…::promotion_carries_intake_facts`, `…::promotion_rechecks_ancestry` (a stored chain naming another start time → `foreign_session`), `…::binding_interpret_sees_bound_state`, `…::failed_promotion_rolls_back_binding` (an adapter repeating a name: launch unbound, buffer intact), `…::binding_raw_is_kept` (`MvpHooks`: the `SessionStart` body is a `raw_unknown` after `session_bound`) |
| PR9-CL-13 | A buffer that would exceed 32 deliveries or 8 MiB, or whose earliest `received_at` is more than 30 s before the job's time, shall become `raw_unknown` `unbound` as a whole, in receipt order; a clock step back shall expire nothing. | mvp §2.9 | `hook_ingest::buffer_count_overflow`, `…::buffer_bytes_overflow` (exact bound and one over), `…::buffer_ttl_expires` (store clock advanced past 30 s; at exactly 30 s nothing expires), `…::ttl_uses_earliest_received_at` (commit order 2, 1), `…::clock_back_expires_nothing` |
| PR9-CL-14 | A hook of a launch that is not the thread's current launch, that has exited, or that has no recorded pid by the end of its wait shall be `raw_unknown` `unbound`; `resolve_unbindable` shall turn a launch's buffer into `raw_unknown` `unbound`. | mvp §2.9; §2.1 rule 5 | `hook_ingest::not_current_launch_is_unbound`, `…::exit_before_bind_then_after_bind` (exit first: buffer resolved, a later `SessionStart` `unbound`; bind first: promotion once, a later hook `unbound`, `last_turn_start_seq` unchanged), `…::resolve_unbindable_clears_buffer` |
| PR9-CL-15 | The pid wait shall see a `launch_started` committed before its subscription, wake on one committed during it, and a wait that ran out shall decide `unbound` even if `launch_started` commits before the job. | mvp §2.9 "Early hooks" | unit `hook::tests::pid_wait_timings` (sync points: committed before subscribe, during the wait, after the bound and before the job) |
| PR9-CL-16 | A body that is not JSON, or is truncated, shall be `raw_unknown` `unparseable` (truncated: with the annex marker); JSON without the hook fields shall be `schema_mismatch`; both with the exact bytes. | arch §1; P-14 (exception: truncation, §8 D5) | `hook_ingest::unparseable_kept_verbatim` (invalid UTF-8 → base64), `…::truncated_is_marked`, `…::schema_mismatch_kept_verbatim`, `…::large_raw_is_a_blob` (200 KiB) |
| PR9-CL-17 | Every rejection of §2.11 rows P, G, and H shall produce its listed observation and create no delivery, event, descendant row, or projection change, each case within its deadline. | mvp CL-16; mvp §2.5 | `hook_b8::parser_layer`, `…::guard_layer`, `…::handler_layer` (tables, §7.1) |
| PR9-CL-18 | A rejection decided before the body shall reach a client that withholds its body until it reads the response. | mvp §4 PR 9 "response ordering" | `hook_b8::ordering_body_withheld` |
| PR9-CL-19 | A hook's budget shall run from its connection's first head byte: a request whose head is complete within the parser deadline but whose body completes after the 5 s request deadline shall get 408 and record nothing. | mvp §2.5; http §2.2 | `hook_b8::deadline_from_first_head_byte` (head over 4.0 s, body over 1.5 s), `…::body_held_is_408` (408 between 4.8 s and 7.5 s), `…::observed_slow_body` |
| PR9-CL-20 | With body bytes in flight at a rejection, the client shall observe the decided status, EOF, or a reset, never a success, and nothing shall be created; the observation and the sender's error shall be recorded per OS. | K2; http H5 | `hook_b8::observed_body_in_flight` (1 MiB sent up front with a revoked token; and with 413 declared) |
| PR9-CL-21 | Two equal `Content-Length` headers shall be accepted as one, and `Transfer-Encoding` in any combination shall be 400 with nothing created. | mvp §2.5; K1 | `hook_b8::duplicate_equal_length_accepted`, `hook_b8::guard_layer` rows G6 |
| PR9-CL-22 | Each store outcome of the delivery job shall map to its §2.11 J row through PR 7's `write_failure`, and an outcome-unknown reply shall be followed by a read that tells whether the delivery committed. | http §2.9; mvp §2.5 | unit `hook::tests::store_outcomes_map` (PR 7's fault points and the store writer's: expired, `Busy`, `RolledBack(Busy)`, disk full, queue full, closing registry, commit unknown, writer gone, failed, grace exceeded; for J7 the test re-reads and then retries with the same `emit_id`) |
| PR9-CL-23 | The emitter shall exit 0, write nothing to stdout or stderr, and finish within 3 s, whatever the daemon does. | mvp CL-23 | `hook_emit::{refused, no_reply, slow_write, status_408, status_503, cut_before_status, rejecting, accepting}` (exit 0, both outputs empty, elapsed below 3.5 s literal); unit `hook_emit::watchdog::fires_at_3000_ms` (injected exit and clock: fires at `W`, not before; fails with the watchdog removed) |
| PR9-CL-24 | The emitter shall send at most 3 attempts with one `emit_id` and absolute per-attempt deadlines, retrying only after a transport error, 408, or 503. | mvp §4 PR 9 | `hook_emit::no_reply_gets_a_second_attempt` (first connection cut at ~1.8 s, a second with the same id and `X-Chaldea-Attempt: 2`), `…::retries_503_with_one_emit_id` (503, 503, 200), `…::no_retry_on_401`, `…::retries_after_cut_before_status`; unit `hook_emit::client::status_line_parse`, `…::zero_remaining_is_timeout` |
| PR9-CL-25 | The emitter shall not send when the URL or token is missing or not exact, or when stdin stays open past its deadline, and shall send the first 4 MiB marked truncated when stdin is longer. | spike B-9 | `hook_emit::no_request_without_env` (each variable alone), `…::no_request_with_open_stdin`, `…::truncates_over_4_mib` |
| PR9-CL-26 | A process shall be recorded as a launch's descendant only through a chain whose every link from the emitter to the launch was proven within one process generation; an unprovable chain shall record nothing; reaching the cap shall mark the list incomplete. | mvp §4 PR 9 / 10 | unit `process::tests::{macos_reread_detects_new_parent, macos_same_parent_is_proven, windows_snapshot_mismatch_is_unproven, windows_signalled_handle_is_unproven, windows_newer_parent_is_unproven, windows_consistent_chain_is_proven}` (synthetic mixed-generation tables, run on both OSes, with positive and negative controls); `hook_ingest::descendants_only_through_the_launch`, `…::descendants_cap_marks_incomplete`; `process_native::ancestry_of_a_child_reaches_this_process` |
| PR9-CL-27 | A `hook` token shall be accepted only with its thread and launch, the launch belonging to that thread; every target of the job shall come from the token re-verified in the job, and a revocation committed after the pre-read shall give 401 and no change. | mvp §2.5 | `hook_ingest::hook_token_checks` (no launch, another thread's launch, revoked); unit `hook::tests::revoked_after_preread` (sync point between pre-read and job) |
| PR9-CL-28 | A `raw_unknown` of a hook shall be in the token's thread stream with the token's launch; an `emit_id` already used in the thread by another launch shall be refused with 400 and nothing stored. | mvp §2.1 | asserted in every `raw_unknown` case of `hook_ingest` through one helper; `hook_ingest::emit_id_reused_by_another_launch` |
| PR9-CL-29 | No hook body and no hook token shall appear in the daemon's logs, on a recorded hook, a `raw_unknown`, a rejection, a store failure, or a panicking adapter. | mvp §2.5 "Secrets"; http §2.10 | `http_logs::hook_bodies_and_tokens_stay_out` (capturing subscriber; body and token markers; the cases listed; the process panic hook's stderr is PR 8's test) |
| PR9-CL-30 | A delivery recorded by a retry shall carry `attempt` and `intake_reliable: false` to the adapter; a retry whose first attempt committed shall replay it without a second `interpret`. | §2.6; mvp §2.3 | `hook_ingest::retry_after_uncommitted_first_is_unreliable` (first attempt cut before its job: the delivery's adapter call sees attempt 2, `intake_reliable` false, the retry's facts); `…::retry_after_lost_reply_replays` (first committed, its reply dropped by the client: the retry gets 200, one delivery, one adapter call, facts of attempt 1) |

Every PR 9 obligation maps to a claim: `hook_deliveries` reply, ordinal, and routing (PR9-CL-1, -2, -5); retries and intake facts (PR9-CL-30); the emitter's retry policy (PR9-CL-24); B-8 per layer (PR9-CL-17); budgets and response ordering (PR9-CL-18, -19); oversized heads (PR9-CL-17, rows P2); duplicate `Content-Length` and `Transfer-Encoding` (PR9-CL-21); held, sent, and slow bodies (PR9-CL-19, -20); positive controls (§7.1); a retry after binding (PR9-CL-2); nested FakeCli (PR9-CL-7); durable buffering (PR9-CL-4); ancestry re-validation (PR9-CL-8, -12); a conflicting `SessionStart` (PR9-CL-9); hole d (PR9-CL-11, -12); session selection (PR9-CL-10); `extra` attribution (PR9-CL-26); early hooks and buffer bounds (PR9-CL-13–15); concurrent binding and ordinals (PR9-CL-5, -6); logs (PR9-CL-29). mvp CL-10 is PR9-CL-1–3, CL-16 is PR9-CL-17, CL-17 is PR9-CL-7, CL-23 is PR9-CL-23.

## 7. Tests and Completion Commands

All tests run on `macos-latest` and `windows-latest`. Caps and budgets in tests are the literals of this design (5 s, 1 s, 3 s, 4 MiB, 8 MiB, 32, 30 s, 32 MiB, 1024, 16), set through `HookConfig` / `HttpConfig` only where a test must shorten a wait, and then named in the test. Tests that need fault points (PR 7's `http::tests::faults`, the store writer's `Faults`) are unit tests in `crates/daemon/src/hook/tests/`, where those points are reachable.

### 7.1 B-8 (`crates/daemon/tests/hook_b8.rs`)

The test server is started with the product values set explicitly (`short_deadline = 5 s`, `header_read_timeout = 5 s`, `max_body_bytes = 4 MiB`, `max_header_bytes = 8 KiB`), not the harness defaults. Each table row: a name, the raw request (head, then body sent, withheld, trickled, or none), the expected observation, a per-case deadline, and the no-side-effect check. The client is the raw client of `tests/support/http.rs`; its observation is `{ status: Option<(code, ErrorBody code or none)>, end: Eof | Reset | Timeout, send_error: Option<io::ErrorKind> }`, so a reset while still sending is told apart from one after the status. **No side effect** (`support::hook::Snapshot`): `hook_deliveries` rows of the launch, `launch_descendants` rows, `head_seq` of the host and thread streams, and every `proj_*` row are equal before and after the case. Every table ends with its positive control: the same server and launch accept a valid hook (200, one delivery), so a broken setup cannot pass the negative cases.

| Layer | Cases (expected; per-case deadline) |
|---|---|
| Parser | `+5` with body withheld (400, no `ErrorBody`; 8 s); two different lengths (400; 8 s); 9 KiB head (431; 8 s); 65 headers (431; 8 s); HTTP/1.0 with `Transfer-Encoding: chunked` (400; 8 s); head missing its final CRLF (`Eof` after ≥ 4.9 s, before 8 s; 12 s) |
| Guard | bad `Host`, wrong port, two `Host`, no `Host` (421); `GET /hook` (405, `Allow: POST`); `POST /hooks` (404); `Origin: http://127.0.0.1:<port>` (403); no `Authorization`, two, `Basic`, malformed, unknown, revoked, unscoped hook token (401); owner token (403); `Transfer-Encoding: chunked`, `chunked` + `Content-Length`, `Content-Length` + `chunked` (400 `bad_request`); no `Content-Length` (411); `4 MiB + 1` declared (413); each 8 s |
| Handler | no `X-Chaldea-Emit-Id`, two, `not-a-uuid`; two `X-Chaldea-Thread-Id` (400); body budget exhausted by held requests (503 `busy`); each 8 s |
| Ordering | every guard and handler case above with the body withheld until the status is read (status read with zero body bytes sent; 8 s) |
| Deadline | head trickled over 4.0 s (within the parser's 5 s), then the body trickled over 1.5 s (408, nothing recorded; 12 s); head complete, body never sent (408 between 4.8 s and 7.5 s; 12 s); `observed_slow_body`: body trickled past the deadline (408, EOF, or `Reset`, printed; 12 s) |
| In flight | `observed_body_in_flight`: revoked token with a 1 MiB body sent up front (401, EOF, or `Reset`, printed with the OS and the sender's error; 12 s); the same for 413 declared over 4 MiB with 1 MiB sent |
| Accepted | two equal `Content-Length` (200, one delivery); valid hook (200) |

The `observed_*` cases print `B8-OBSERVED <case> <os> <status> <end> <send_error>` lines; the CI step of §1 shows them on success, which is the per-OS record that arch §5's open item asks for.

### 7.2 Ingest (`crates/daemon/tests/hook_ingest.rs`) and units

The tests of §6.2 named `hook_ingest::*`, against the test server with the hook mount, a `TestStore`, launches created by appending `thread_registered`, `launch_reserved`, `launch_started` directly, a synthetic `ProcessProbe` (chains as data, with a gate to hold one walk), a recording `IntakeProbe`, and a test `HookAdapter` that maps `UserPromptSubmit` to `user_message { input_id: null }`, `Stop` to a `status_changed`, and optionally returns `readiness_observed` with a `WriteBasis` from the intake facts, counting its calls. Units in `crates/daemon/src/hook/tests/`: `pid_wait_timings`, `store_outcomes_map`, `revoked_after_preread`, `intake_order` (release on drop before submission, after the closure ran, on expiry, on submit failure). Other units: `hook::payload` (table of bodies → `Decoded`), `hook::ancestry` (eligibility table: nested same name, different agent name, `L` absent, empty chain, non-emitter first link), `hook::request` (header table), `hook::buffer::timestamp` (format, boundaries), store `triggers::session_bound_once` (second binding, `NULL` launch, binding of a non-current launch refused; a legitimate binding of the next launch accepted), `store_descendants::{dedupe, cap_marks_incomplete, immutable}`, `store_open::v1_upgrades_to_v2` (a v1 database with deliveries and projections opens, migrates, keeps its rows, and `verify_projections` is empty), `schema_tests::shipped_fingerprints_match_the_migrations` (v2 added).

**PR 7 fixtures updated** (§1 files): `support/http.rs` `token_of(Hook)` registers and starts a launch and mints a scoped hook token, like `Agent`; `http_layers.rs` hook cases use it, and `/hook` cases that reached the 501 now expect the handler's outcomes (an empty body without an emit id → 400 `bad_request`); `http_matrix.rs` `INVENTORY` marks `POST /hook` mounted; `http_logs.rs` exercises the real hook path for PR9-CL-29.

### 7.3 Process lookup (`crates/pty-runtime`)

`process::tests::*` (synthetic tables, both strategies, on every OS: walk to root, depth limit, `Gone`, `Unreadable`, zombie as `Gone`, the mixed-generation cases of PR9-CL-26); `tests/process_native.rs::ancestry_of_a_child_reaches_this_process` (spawns a child under `with_spawn_lock` that spawns a grandchild; the grandchild's walk is proven through the child to this test process, with identities equal to `lookup` of each) and `::name_is_normalized` (FakeCli's name `chaldea-fake-cli`, within the 31-character macOS limit, without `.exe`).

### 7.4 Emitter and end to end (`crates/cli`)

`tests/hook_emit.rs`: the emitter binary against stub `TcpListener`s on `127.0.0.1:0` (refusing, silent after reading the request, not reading a 4 MiB body, scripted statuses, closing before a status line), the cases of PR9-CL-23–PR9-CL-25; units for URL and status-line parsing, the zero-remaining rule, and the watchdog with an injected exit and clock. `tests/hook_e2e.rs` (real daemon from public APIs, real FakeCli, real emitter, real walk):

- `codex_order_binds_and_promotes`: FakeCli `hooks` steps `emit UserPromptSubmit` → `wait_file` (the test checks a `buffered` delivery exists, then creates the file) → `emit SessionStart`; the stream is `session_bound` then the promoted hook.
- `nested_fake_cli_does_not_bind`: steps `nested [emit SessionStart(other)]` → `wait_file` (the test checks the nested hook is `foreign_session` and the launch unbound) → `emit SessionStart(own)` → `nested [emit SessionStart(other2)]`; the launch binds to its own session once, and both nested hooks are `foreign_session`.
- `claude_session_id_binds`, `descendants_recorded_from_a_real_chain`.

### 7.5 Completion commands

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p chaldea-pty-runtime --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings   # on macOS before pushing; CI runs the Windows clippy natively
cargo test --workspace --locked
cargo test -p chaldea-daemon --test hook_b8 --locked -- observed_ --show-output
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum|hyper|tower|getrandom) ' <<<"$deps"
```

plus the `workspace` job green on both runners with the sentinel and the B-8 step of §1, the `specs` job unchanged (no Quint change: the model already states what PR 9 implements, K10), and the `pty-hooks` job unchanged. A regression test counts only after it was seen failing with its guard removed: in particular the binding order (PR9-CL-6), row 7's ancestry check (PR9-CL-8), the promotion recheck (PR9-CL-12), row 5 (PR9-CL-14), the per-link proofs (PR9-CL-26), the job's dedup (PR9-CL-1, concurrent case), and the emitter's watchdog (PR9-CL-23).

## 8. Decisions

**D1. `chaldea hook-emit <agent>` is the only emitter command form, from the first release.** Codex hashes the command string into hook trust (arch §5), so any later change of the arguments costs every user a `Hooks need review`; everything else passes through the environment. Irreversible in practice; it follows mvp §2.7's "fixed command strings".

**D2. A launch binds once in PR 9; session switches are PR 13 / 14's decision.** A `SessionStart` with another session for a bound launch is `raw_unknown` `foreign_session`, and the binding stays. Nothing user-visible depends on it yet: until PR 13 / 14 plug in decoders, every bound hook other than the binding itself is `raw_unknown` anyway. Once adapters exist, the effect is that after a session switch in an attached CLI (Claude `/clear` or in-session `/resume`; Codex `/new` or `/clear`, A1) the thread stops following the CLI: the new session's hooks are `foreign_session`, its prompts and readiness are not applied, the gate does not reopen, the tailer stays on the old transcript, and `chaldea resume` passes the old session. PR 13 and PR 14 must settle this before they merge, from PR 12's fixtures of switch payloads (`source`, new id): either follow the new session (a repeatable `session_bound` per launch as a new turn start for PR 10's gate and a new transcript generation for the tailer, with `events_session_bound_once` dropped by migration) or keep one binding and document the limit. Reversible: stored events do not change meaning either way.

**D3. Claude binds and selects by session id; its ancestry is recorded, not required.** mvp §2.9 and the spike verified the session-id rule for Claude; requiring ancestry for Claude was never run on a real Claude host (the spike's Claude binding used the session id only, K9). PR 13 checks the chain on real hosts; switching Claude to rule `Ancestry` is one row of `HookAdapter::rule`.

**D4. The buffer bounds are 32 deliveries, 8 MiB, and 30 s per launch.** Codex's early hooks are the first turn's few concurrent hooks; 30 s exceeds the 5 s request deadline plus the 1 s pid wait many times, and 8 MiB holds two maximal bodies. Internal; tunable without migration.

**D5. Stdin over 4 MiB is sent truncated and kept as `raw_unknown` `unparseable` with an annex marker.** The spike dropped it (B-9), which is a silent loss (arch §1); the truncated prefix keeps the evidence that the hook fired. This is the one exception to P-14's "verbatim": such an event's `raw` is the first 4 MiB, and its annex `Hook { truncated: true }` records that, internally and rebuildably. Exposing the marker to clients would be a public payload change; it can be added later as an optional field. The 4 MiB cap (mvp §2.5) is unchanged.

**D6. The `X-Chaldea-*` headers other than `X-Chaldea-Emit-Id` are internal.** Both ends are the same binary; a running launch keeps the emitter path it was given, and a daemon restart revokes open launches' tokens, so a version mix lasts at most until the restart. The daemon ignores unknown `X-Chaldea-*` headers and treats an absent or invalid value as described in §2.2.

**D7. A recorded delivery answers 200 with an empty body.** No new public body (mvp §2.5 lists the public bodies); the emitter reads only the status; dispositions are in the stream and in `hook_deliveries`. A disposition body can be added later; it is additive.

**D8. No emitter log file.** A log path would be a new public environment variable (mvp §2), and the emitter is silent by contract; the doctor (PR 21) observes deliveries on the daemon side. A `CHALDEA_HOOK_LOG` can be added later; it is additive.

**D9. B-8 does not drain rejected bodies.** With a large body in flight a client may see a reset instead of the status (K2); the only client retries and needs no status (§2.13), and nothing is created either way (PR9-CL-20). A lingering drain in PR 7's connection loop can be added if a client ever needs the status.

**D10. The Windows parent proof keeps one clock comparison.** Windows keeps the creator's pid in `th32ParentProcessID` and exposes no generation number through the documented API, so after the snapshot and handle cross-check, rejecting a reused parent pid still compares creation times (§2.5). A system clock set back between a creator's exit and the reuse of its pid can make the walk accept a link it should not, and the identity of that later process would be recorded. The walk also requires the chain to continue through proven links to the launch, which a reused pid rarely does. macOS needs no clock comparison.

### Open Questions

None in this document. The session-switch decision (D2) is an obligation of PR 13 and PR 14, to be settled before they merge.

## Related Text in Other Documents

- `docs/architecture.md` §5 "Windows hook receiver": states the daemon's verified expectations (`Content-Length: +5` → hyper's 400, K1; slow head → EOF). `docs/mvp-design.md` §2.5 already states them; its "Proposed Canonical-Doc Changes" bullet for arch §5 is thereby applied.
- `docs/design/http-surface.md` §2.4 ("for `hook`, only revocation (PR 9 adds its launch and binding checks)") and §2.11: PR 9 fills both (§2.8, §2.3); `WriteCtx` gains `read` and `commits` (§1 files).
- `docs/design/event-store.md` §2.4: migration v2 adds `launch_descendants`, `launch_descendants_incomplete`, and `events_session_bound_once`; §2.6: `EventAnnex::Hook`; §2.7: `buffered_summary`, `max_receipt_ordinal`, `descendants` (§2.9, §2.12).
- `docs/design/pty-unix.md` §4 and `docs/design/pty-windows.md` §4 "Recorded descendants": the walk, its per-link proof, and its public lookup are §2.5 and §2.12 here.
