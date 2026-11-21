# MVP Design: Local-First Chaldea

Status: design, not implemented. It plans the local-first MVP of issue #2. Canonical constraints: `AGENTS.md` and `docs/architecture.md` (cited as "arch §N"). Mechanism decisions: `docs/prior-art.md` (cited as "prior-art row N"). Spike evidence: `docs/spikes/pty-hooks-spike.md` (cited as "spike §N" with report ids) and `spikes/pty-hooks/src/`.

Additional prior art read for this document (shallow clones, default branch on 2026-11-21; paths are relative to each repository root):

| Source | Revision | Read for |
|---|---|---|
| T3 Code (`pingdotgg/t3code`) | `10586f9e` | event store, command receipts, replay/snapshot resume, auth scopes, worktree cleanup |
| Codex app-server protocol (`openai/codex`, sparse `codex-rs/app-server-protocol`) | `17cb2e3` | thread/turn status model, approvals, `ts-rs` export and schema fixture tests |
| herdr (`herdrdev/herdr`) | `e8d7d97` | agent state model, prompt submission, worktree removal |

Zed is cited only through prior-art rows. Anything not read in source or docs is marked **unverified**.

## 1. Scope and Non-Goals

**Goal (issue #2).** A Rust Sans-IO core (`crates/core`, `crates/pty-runtime`, `crates/daemon`), daemon-owned PTYs, authenticated hook ingest, an SQLite append-only event store, the `chaldea` CLI (`spawn`, `send`, `wait`, `stop`, `mailbox`, `attach`, `doctor`, `hook-emit`), and a local single-origin PWA (`packages/contracts`, `packages/web`), on macOS and native Windows hosts.

**In scope.**

- The irreversible core of arch §2, fixed in §2 below.
- The five seams of arch §4, at the depth needed by the milestones of issue #2.
- The minimum UI: a thread list, a thread view (events and 3-axis state), a prompt input (plain `<textarea>` with IME guards), an approval card, a read-only VT snapshot, and the emergency VT keypad. Mobile viewports are supported by the same layout.
- The minimal A2UI catalog (5–6 components, §2.8) and `surface_action` execution.

**Non-goals.**

- The Phase 2 relay (`packages/relay`, Cloudflare Workers + Durable Objects, Cloudflare Access) and therefore Android remote access. The local PWA binds to `127.0.0.1` only. Contracts leave room for the relay (§2.4, §2.5) without implementing it.
- Every reversible layer of arch §2: Tauri / Capacitor shells, diff view, desktop split panes, visual styling, a separate PTY-owning process, A2UI components beyond the minimal set. UI layout and styling stay undecided; `docs/design/chaldea.pen` remains wireframes only.
- A client-side terminal emulator (invariant 2). The PWA shows `alacritty_terminal` snapshots as text grids; full terminal access is `chaldea attach`.
- Orchestration strategy in daemon code (invariant 2). The daemon records and relays; agents decide.
- Removing `spikes/` (issue #1).

## 2. Irreversible-Core Contracts

Each contract lists the options compared, the decision, and which parts are a **public contract** (generated `ts-rs` types, HTTP API, CLI flags, environment variables, on-disk formats other programs read) or **internal** (may change in any PR without migration notes).

All public types are Rust types in `crates/core` with `serde` and `ts-rs` derives. Generated TypeScript is committed under `packages/contracts/src/generated/` and checked for drift in CI, as Codex's app-server does for its committed `schema/typescript` and `schema/json` fixtures (`codex-rs/app-server-protocol/src/schema_fixtures_tests.rs:17-41`; `ts-rs` derives at `src/protocol/v2/thread.rs:1821-1823`). JSON Schema is generated alongside with `schemars` for runtime validation in `packages/contracts` (§3).

### 2.1 Event Envelope, Event Kinds, SQLite Schema

**Streams.** Every event belongs to exactly one stream. Two stream kinds:

- `host` — one per host: thread registration and closing, workspace and worktree changes, host-scoped surfaces, `raw_unknown` not bound to a thread. Stream id `host`.
- `thread` — one per thread: everything about that thread. Stream id `thread:<thread_id>`.

`seq` is a per-stream, gapless, monotonic integer starting at 1, assigned inside the append transaction. A global `global_seq` (SQLite `INTEGER PRIMARY KEY AUTOINCREMENT`) orders events across streams for internal use (projection rebuild, WS fan-out) and is not exposed.

Options compared:

| Option | Precedent | Verdict |
|---|---|---|
| A. One global sequence; clients filter by thread | T3 Code orchestration v2: `sequence INTEGER PRIMARY KEY AUTOINCREMENT` with `thread_id` indexes (`apps/server/src/persistence/Migrations/055_OrchestrationV2.ts:20-38`) | Rejected. Gaps inside one thread's history are not detectable from the numbers, and a Phase 2 read cache per thread has to re-derive per-thread order. |
| B. Per-stream version plus global sequence | T3 Code orchestration v1: `stream_id`, `stream_version`, unique `(aggregate_kind, stream_id, stream_version)`, global `sequence` (`Migrations/001_OrchestrationEvents.ts:8-28`) | **Adopted** (as `stream_id` + `seq`, with `global_seq` internal). It matches arch §4.1's `stream_id + after_seq` API directly. |
| C. Per-item notifications with no replay cursor | Codex app-server: `thread/status/changed`, `turn/started`, `item/completed` notifications (`src/protocol/common.rs:2087`, `:2116`, `:2127`) | Rejected for storage: it is a live protocol over a connection, not a replayable log. Its item and status vocabulary informs §2.2. |

**Envelope** (public, `ts-rs`):

| Field | Type | Meaning |
|---|---|---|
| `stream_id` | string | `host` or `thread:<uuid>` |
| `seq` | u64 | per-stream, gapless, from 1 |
| `event_id` | UUID | unique per event; generated by the daemon (core receives it as input, so core stays deterministic) |
| `schema_version` | u32 | version of the event schema set (see below) |
| `idempotency_key` | string | dedupe key, unique per stream (see below) |
| `kind` | string enum | event kind (below) |
| `thread_id` | UUID or null | set for thread streams and for host events about one thread |
| `command_id` | UUID or null | the command that caused the event, if any (§2.4) |
| `source` | object | `{ "kind": "hook" \| "transcript" \| "process" \| "command" \| "daemon" \| "extension", "agent": "claude" \| "codex" \| null, "cli_version": string \| null }` |
| `occurred_at` | RFC 3339 UTC, ms | source time when the source states one, else equal to `recorded_at` |
| `recorded_at` | RFC 3339 UTC, ms | daemon append time |
| `payload` | object | kind-specific; at most 64 KiB serialized, larger content moves to a blob and the payload carries `{ "blob": { "sha256", "size", "media_type" } }` (arch §4.2 "size-bounded/referenced large tool outputs") |

`schema_version` is one integer for the whole event schema set, starting at 1. Additive changes (a new optional field, a new kind) do not bump it; a change that alters the meaning or type of an existing field does. A client that receives a higher `schema_version` than it knows renders the event generically and shows that the client is outdated. Per-kind versions were considered and rejected: they multiply compatibility checks without a client that could use the extra precision.

**Idempotency keys** are deterministic so that a crash-and-replay of the same source produces the same key, and the store rejects the duplicate (`UNIQUE (stream_id, idempotency_key)`):

| Source | Key |
|---|---|
| Hook payload | `hook:<emit_id>`, where `emit_id` is a UUID the emitter generates once per invocation and sends as `X-Chaldea-Emit-Id`. A body hash is not used: two identical prompts produce byte-identical `UserPromptSubmit` bodies. |
| Transcript line | `transcript:<generation>:<offset>` of the line start (§2.1 cursor) |
| Process lifecycle | `process:<launch_id>:<spawned\|exited>` |
| Command | `command:<command_id>:<n>` for the n-th event the command produced |
| Derived event (state change caused by another event) | `derived:<cause idempotency_key>:<name>` |
| Extension `POST /events` | `ext:<caller key>`, required from the caller |

**Event kinds** (public). The kinds of arch §4.2 plus the ones the other seams need:

| Kind | Stream | Produced by |
|---|---|---|
| `user_message` | thread | `UserPromptSubmit` hook (prompt as the CLI recorded it) |
| `assistant_message` | thread | transcript line (Claude); `Stop` hook payload where the CLI includes the last message (Codex; field presence **unverified**, settled by the fixture corpus, PR 11) |
| `tool_activity` | thread | `PreToolUse` / `PostToolUse` hooks, transcript tool lines |
| `status_changed` | thread | daemon, whenever an axis of §2.2 changes; payload `{ axis, from, to, cause }` |
| `approval_requested` / `approval_resolved` | thread | `PermissionRequest` hook / approval decision (§8 Q1) |
| `input_state_changed` | thread | input state machine (§2.3) |
| `mailbox_message` | thread (recipient) | `chaldea mailbox send` |
| `artifact_diff` | thread | reserved; not produced in the MVP (diff view is a reversible layer) |
| `surface_update` | host or thread | A2UI emit (§2.8) |
| `vt_snapshot_taken` | thread | daemon on attention (payload references a blob holding the grid) |
| `thread_registered` / `thread_closed` | host | spawn / close |
| `worktree_changed` | host | worktree create / release / remove (§2.6) |
| `raw_unknown` | thread if bound, else host | any input that could not be parsed or bound |

`raw_unknown` payload: `{ "source", "reason": "unparseable" \| "unknown_event" \| "foreign_session" \| "unbound" \| "schema_mismatch", "agent", "cli_version", "content_type", "raw" }`, where `raw` is the verbatim body (UTF-8 text, or base64 with `"encoding": "base64"` when not UTF-8), moved to a blob above 64 KiB. Nothing is dropped (arch §1 self-healing; spike B-11 kept unbound events as `raw_unknown`/`foreign_session`).

**SQLite schema sketch** (internal; the database file is not a public format):

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);          -- schema migration level, host_id, epoch
CREATE TABLE streams (stream_id TEXT PRIMARY KEY, kind TEXT NOT NULL, thread_id TEXT, head_seq INTEGER NOT NULL);
CREATE TABLE events (
  global_seq INTEGER PRIMARY KEY AUTOINCREMENT,
  stream_id TEXT NOT NULL REFERENCES streams(stream_id),
  seq INTEGER NOT NULL,
  event_id TEXT NOT NULL UNIQUE,
  schema_version INTEGER NOT NULL,
  idempotency_key TEXT NOT NULL,
  kind TEXT NOT NULL,
  thread_id TEXT, command_id TEXT,
  source_json TEXT NOT NULL, occurred_at TEXT NOT NULL, recorded_at TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  UNIQUE (stream_id, seq), UNIQUE (stream_id, idempotency_key)
);
CREATE TRIGGER events_append_only_u BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT, 'append-only'); END;
CREATE TRIGGER events_append_only_d BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT, 'append-only'); END;
CREATE TABLE blobs (sha256 TEXT PRIMARY KEY, size INTEGER NOT NULL, media_type TEXT NOT NULL, data BLOB NOT NULL);
-- Projections: derived from events in the same transaction; rebuildable by replaying events.
CREATE TABLE threads (thread_id TEXT PRIMARY KEY, agent TEXT, cli_version TEXT, session_id TEXT, launch_id TEXT,
  process_state TEXT, turn_state TEXT, task_state TEXT, cwd TEXT, worktree_id TEXT, parent_thread_id TEXT, head_seq INTEGER);
CREATE TABLE inputs (input_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, command_id TEXT NOT NULL UNIQUE, state TEXT NOT NULL,
  encoding TEXT NOT NULL, text_blob TEXT NOT NULL, queued_at TEXT, sent_at TEXT, resolved_at TEXT);
CREATE TABLE transcript_cursors (thread_id TEXT NOT NULL, generation INTEGER NOT NULL, path TEXT NOT NULL,
  file_id TEXT, offset INTEGER NOT NULL, PRIMARY KEY (thread_id));
CREATE TABLE command_receipts (command_id TEXT PRIMARY KEY, kind TEXT NOT NULL, request_sha256 TEXT NOT NULL,
  accepted_at TEXT NOT NULL, status TEXT NOT NULL, result_json TEXT NOT NULL);
CREATE TABLE worktrees (...);   -- §2.6
CREATE TABLE tokens (...);      -- §2.5
```

Projections (`threads`, `inputs`) change only together with the event that changes them, in one transaction; a rebuild from `events` must reproduce them (a property test in PR 6). WAL mode, `synchronous=FULL` for the append path, one writer task in the daemon.

**Transcript cursor** (internal). Explicit paths only: a transcript path comes from a hook payload's `transcript_path`, never from guessing a directory (arch §4.2). The cursor is `(generation, path, file_id, offset)`:

- `offset` is the byte offset just after the last complete line (ending in `\n`) that was ingested; a trailing partial line is left for the next read.
- `generation` increments when `path` changes, when the file identity (`file_id`: inode + device on Unix, volume serial + file index on Windows) changes, or when the file is shorter than `offset` (truncation). A new generation restarts at offset 0.
- The events produced from a batch of lines and the new cursor are committed in **one transaction**, so a crash either keeps both or neither (arch §4.2). The idempotency key `transcript:<generation>:<offset>` is the second line of defence.
- `transcript_path: null` (Codex, arch §5) creates no cursor; that thread is ingested from hooks only. A later non-null path for the same session starts generation 1.

Options compared: an `(inode, offset)` cursor without generation (rejected: a rotated or replaced file at the same path silently mixes histories); a line-count cursor (rejected: needs a rescan and breaks on CRLF/partial lines); a content hash of the last line (rejected: identical lines repeat). The generation counter is the architecture's decision and is kept.

**Public vs internal.** Public: the envelope, `kind` values, every payload type, the `raw_unknown` payload, `schema_version` semantics, idempotency key formats for `POST /events` callers. Internal: SQL schema, projection tables, `global_seq`, blob storage layout.

### 2.2 3-Axis Thread State

Three independent axes (arch §2, §4.2). Every transition is caused by an explicit, typed input; the reducer's input type has no variant built from PTY output, so screen text cannot drive state (invariant 1). The VT snapshot and DECSET 2004 are read only to gate input (§2.3) and to show a human what the CLI shows.

**`process_state`** — the OS process of the launch.

| State | Entered on |
|---|---|
| `starting` | spawn accepted (`thread_registered`) |
| `running` | `PtyRuntime::spawn` returned a live child (`process:<launch>:spawned`) |
| `stopping` | `stop` command accepted |
| `exited` | child exit observed (`process:<launch>:exited`, payload `{ code, signal, reason: "exit" \| "stopped" \| "spawn_failed" \| "daemon_restart" }`) |

Session binding (`session_id` known) is a thread field, not a process state: Claude binds at startup, Codex only with its first turn (arch §5), so the same process state would mean different things per agent.

**`turn_state`** — the CLI's conversational turn.

| State | Entered on |
|---|---|
| `unknown` | before the first explicit signal, and after `exited` |
| `idle` | Claude `SessionStart` (eager); `Stop` hook (both agents) |
| `running` | `UserPromptSubmit` hook bound to the thread; `approval_resolved` |
| `awaiting_approval` | `PermissionRequest` hook (`approval_requested`) |
| `needs_attention` | `attention_raised`: SessionStart timeout from launch (default 20 s, spike §4.7, arch §2 landmine), an input that became `unconfirmed` (§2.3), an `attention` extension hook, a Codex `Hooks need review` stall expected after an upgrade (arch §5 open item) |

`needs_attention` clears on the next explicit turn signal (`SessionStart`, `UserPromptSubmit`, `Stop`) or on an explicit `attention_cleared` command from a human. Timers are explicit inputs: the daemon feeds `Tick(now)` to core, and core emits `attention_raised { reason: "session_start_timeout" }`; it is an event with a cause, not an inference.

**`task_state`** — the outcome of the work the thread was given, for orchestrators (arch §1 "inspect child outcomes").

| State | Entered on |
|---|---|
| `none` | spawn without a task |
| `active` | spawn with a task, or an explicit set |
| `blocked`, `completed`, `failed`, `cancelled` | an explicit set by the thread itself (agent token), its parent, or a human (§8 Q2 for the CLI surface) |

The daemon never infers `task_state`; process exit does not change it. An orchestrator therefore sees "process exited, task still active" and decides; that judgment stays in agent instructions (invariant 2). This is the reason for three axes instead of one.

Options compared:

| Option | Precedent | Verdict |
|---|---|---|
| One status enum | herdr `AgentState { Idle, Working, Blocked, Unknown }` derived from screen detection (`src/detect/mod.rs:11-20`) | Rejected: mixes process, turn, and task, and is fed by screen scraping. |
| Thread status + turn status | Codex app-server `ThreadStatus { NotLoaded, Idle, SystemError, Active { active_flags: [WaitingOnApproval, WaitingOnUserInput] } }` (`src/protocol/v2/thread.rs:1810-1827`) and `TurnStatus { Completed, Interrupted, Failed, InProgress }` (`v2/turn.rs:34-39`) | Partly adopted: its approval flag maps to `awaiting_approval`. It has no task axis because it has no orchestration outcome. |
| Three axes | arch §2 | **Adopted.** |

Public: axis names, state values, `status_changed` payload. Internal: the reducer, timer defaults.

### 2.3 Input State Machine

An input is one prompt sent through `chaldea send`, the PWA, or `surface_action`. States:

```
queued ──(writer completed text + submit)──▶ sent ──(ack event)──▶ acked
   │                                          │
   │                                          └─(ack window expired, or submit write outcome unknown)──▶ unconfirmed ──(late ack)──▶ acked
   ├─(validation failed, zero bytes written)──▶ rejected
   └─(cancel, or thread exited, before any byte)──▶ cancelled
```

- `queued`: persisted, no byte written. Delivery waits for the gate: thread bound, `turn_state = idle`, no earlier input in `sent` or `unconfirmed`, and, for bracketed paste, `TermMode::BRACKETED_PASTE` set (prior-art row 4a; spike §4.4). One input is in flight per thread.
- `sent`: the text and the one submit key were written. The ack window (default 30 s, spike §4.7 "UserPromptSubmit 30 s real") starts.
- `acked`: the first `UserPromptSubmit` bound to the thread after `sent_at`. Its `prompt` is compared with the adapter's expected form and the result is recorded as `fidelity: ok | differs` on the input; fidelity never changes the ack (spike §4.4 "oracle split").
- `unconfirmed`: the window expired, or the submit write started but did not report completion (spike §4.4: such a write may have reached the PTY). **Never resent** (arch §4.3). The thread goes `needs_attention`; later queued inputs wait until a late ack arrives or a human releases the thread (`attention_cleared`), because the composer may still hold the text.
- `rejected` (validation: ESC or C0 other than TAB/CR/LF, prior-art row 4b; Codex-on-Windows lossy input or empty prompt, spike F-1d) and `cancelled` are terminal states before any byte is written; they are safe to retry by hand. They extend the architecture's `queued -> sent -> acked / unconfirmed` and keep `unconfirmed` meaning "may have been delivered" (§8 Q3).

Ack per adapter (capability `ack_event`):

| Adapter | Ack | Notes |
|---|---|---|
| Claude Code | `UserPromptSubmit` bound by `--session-id` | `prompt` is Claude's own rendering: TAB → 4 spaces, NFC, `<pasted_content>` wrapper (arch §5); fidelity accounts for these transforms. |
| Codex CLI | `UserPromptSubmit` bound by `session_id` from the launch's `SessionStart` | Send only after the previous turn's `Stop` (arch §5). The first prompt is positional and is acked by the first turn's `UserPromptSubmit`. |

Alternatives: auto-resend after a timeout (rejected by arch §4.3; an extra Return can accept a popup or run a slash command, arch §5); ack by screen echo (rejected, invariant 1); an SDK or app-server protocol with explicit turn acks (T3 Code's approach; rejected by invariant 1, no ACP lock-in).

Public: state names, `input_state_changed` payload, the send API. Internal: window defaults.

### 2.4 Sync API

**Replay.** `GET /api/events?stream_id=<id>&after_seq=<N>&limit=<L>&epoch=<E>` (arch §4.1). `limit` defaults to 200, max 1000. Response (public type `EventsResponse`):

```json
{ "mode": "replay", "stream_id": "thread:…", "epoch": "…", "events": [ … ], "head_seq": 812, "has_more": false }
{ "mode": "snapshot", "stream_id": "thread:…", "epoch": "…", "snapshot": { … }, "as_of_seq": 812 }
```

Snapshot fallback when: `epoch` differs from the database's (the database was recreated; `epoch` is a random id stored in `meta` at creation), `after_seq > head_seq`, `after_seq` is older than the retained range (no compaction in the MVP, reserved for later), or the replay would exceed a bound (10 000 events or 8 MiB). The client then continues with `after_seq = as_of_seq`. The decision is one pure function in core, `decide_replay`, as in T3 Code's `decideThreadResume` (`apps/server/src/orchestration-v2/ThreadStream.ts:99-118`, bounds at `:18-20`). The snapshot of a thread stream is its projection (axes, session, worktree, pending input, pending approval, last N events); of the host stream, the thread list.

**Live notifications.** `GET /api/ws?ticket=<t>` upgrades to a WebSocket. Browsers cannot set `Authorization` on a WebSocket, so the client first calls `POST /api/ws-ticket` with its Bearer token and gets a single-use ticket valid for 30 s; the upgrade also checks `Origin` (§2.5). Messages (public):

```json
// server → client
{ "type": "hello", "epoch": "…", "server_time": "…" }
{ "type": "stream_advanced", "stream_id": "thread:…", "head_seq": 813 }
// client → server
{ "type": "subscribe", "stream_ids": ["host", "thread:…"] }
```

Notify-then-pull: the WS carries only heads, and the client fetches with `GET /api/events`. One code path then serves catch-up and live updates, and a lost notification costs latency, not correctness. Inline event push was considered and deferred: it can be added later as an optional field without breaking clients.

**Commands and idempotency.** Every mutating request carries a client-generated `command_id` (UUID): spawn, send, key, stop, approval decision, attention clear, task set, mailbox send, worktree remove, `surface_action`. The daemon stores a receipt (`command_receipts`, as T3 Code's `orchestration_v2_command_receipts` keyed by `command_id` with a result sequence, `Migrations/055_OrchestrationV2.ts:42-50`). A repeat with the same `command_id` and the same request hash returns the stored result; a different hash returns `409`. The PTY write of a key or input happens at most once per `command_id`.

**Offline.** When the daemon is unreachable, the PWA keeps the last fetched state, locks input, and shows the time of the last successful sync (arch §4.1 read-only lock with freshness indicator).

The Phase 2 relay can cache per stream and forward commands unchanged; that is why `stream_id`, `epoch`, and `command_id` are in the public contract now. A `stateright` model of startup crash, offline buffering, and retry after a lost response (arch §6) checks this design in PR 19.

Public: all request/response/message shapes above. Internal: replay bounds, ticket lifetime.

### 2.5 Auth

The daemon listens on `127.0.0.1:<port>` only. Every request passes, in order and before any body byte is read:

1. `Host` exactly `127.0.0.1:<port>` or `localhost:<port>`, one header, else `421` (DNS rebinding; spike B-8).
2. For browser requests that change state and for WS upgrades: `Origin` absent or equal to the daemon's own origin, else `403`.
3. `Authorization: Bearer <token>`, one header, compared in constant time (`subtle`, prior-art row 5d), else `401`.
4. Scope check for the route, else `403`.
5. Body limits (4 MiB, `413`) and an absolute request deadline (`408`), as in spike B-8.

The HTTP stack is `axum` + `hyper` + `tower-http` (arch §3), so request parsing is hyper's, not ours (prior-art row 5a's reason). The spike receiver's B-8 cases become the daemon's rejection corpus (PR 8).

**Token kinds** (public: kinds, scopes, environment variable names; internal: storage):

| Kind | Holder | Scope | Lifetime |
|---|---|---|---|
| `owner` | the user's CLI (token file, 0600 / user-only ACL) and the paired PWA | everything | until revoked |
| `agent` | a thread's CLI process tree, as `CHALDEA_AGENT_TOKEN` | spawn children (owned by it), send/wait/stop/mailbox for itself and its children, set its own `task_state`, read its own and its children's streams | until the thread closes |
| `hook` | the launch's hook emitter, as `CHALDEA_HOOK_TOKEN` | `POST /hook` for that launch only | one launch |
| `rescue` | the Rescue Agent | read every stream, `raw_unknown`, VT snapshots, doctor results; emit surfaces; **no** input, key, stop, or worktree removal (those run only from a human's `surface_action`) | until revoked |
| `extension` | one one-shot extension hook run, as `CHALDEA_EXTENSION_TOKEN` | `POST /events` for the named scope | one hook run |

Tokens are 256-bit random values, stored as SHA-256 hashes (`sha2`, prior-art row 5c) with kind, scope, and expiry; the cleartext exists only in the holder's file or environment. Hook tokens are per launch, so a nested CLI that inherits the environment cannot bind as the parent: binding also requires the emitter ancestry rule (spike §4.3, prior-art row 6c) and inherited identity variables are scrubbed at spawn.

**PWA pairing.** `chaldea open` starts the daemon if needed and opens `http://127.0.0.1:<port>/#pair=<code>` with a single-use code (60 s). The fragment is never sent in a request; the page exchanges it via `POST /api/pair` for an `owner` token kept in IndexedDB. Rejected alternative: an `HttpOnly` session cookie. It hides the token from page scripts but needs CSRF defences on every route, and the invariant names Bearer auth (AGENTS.md invariant 5). Single-origin serving keeps the page and API on one origin (arch §2.4).

Comparison: T3 Code stores role-bearing sessions with explicit `scopes` and pairing links with expiry and consumption (`Migrations/031_AuthAuthorizationScopes.ts:4`, `:12-48`) and compares secrets with `timingSafeEqual` (`apps/server/src/auth/utils.ts:83-89`); Chaldea adopts scoped tokens and single-use pairing for the same reasons. herdr uses local sockets with no token for its API (`src/ipc.rs:10-11`, prior-art row 8); rejected, because Chaldea's API is HTTP and reachable by browsers.

Browser CF Access vs host Service Tokens (arch §2) belong to Phase 2 and are not built; the token-kind enum leaves room for them.

### 2.6 Worktree Ownership

`chaldea spawn --worktree [--base <ref>]` creates a git worktree for the new thread with the git CLI (as herdr and T3 Code do), spawned as a non-PTY child through `process-wrap` with `CREATE_NO_WINDOW` on Windows (prior-art rows 1c, 9).

Metadata (public in the `worktree_changed` payload and `GET /api/worktrees`):

| Field | Meaning |
|---|---|
| `worktree_id` | 8 hex characters; also the directory name |
| `repo` | the repository's common git directory |
| `path` | `<base_path>/<worktree_id>` |
| `branch` | `chaldea/<worktree_id>` unless given |
| `base_ref`, `base_commit` | what it was created from (`base_commit` is the resolved SHA) |
| `owner_thread_id` | the thread that owns it; ownership passes to the parent when a thread closes |
| `created_by` | `human` or `thread:<id>` |
| `state` | `active`, `released` (no live owner), `removed` |

**Short base path.** `base_path` is configurable per workspace (`worktree_base` in the workspace config). Default: `<state_dir>/wt` (macOS `~/Library/Application Support/chaldea/wt`, Windows `%LOCALAPPDATA%\chaldea\wt`). `chaldea doctor` warns when `base_path` is longer than 40 characters on Windows, because MSVC `link.exe` fails beyond 260 characters (arch §5); a user then sets e.g. `C:\wt`.

**Never auto-delete.** The daemon never removes a worktree on its own: not on thread exit, close, daemon restart, or any timer. Removal happens only through `chaldea worktree remove <id>` (owner or owning agent token) and is refused unless all hold:

- no tracked or untracked changes (`git status --porcelain=v1 -z --untracked-files=all` empty; ignored files do not count);
- no commit reachable from the worktree's `HEAD` or branch that is not on a remote-tracking ref (`git rev-list --max-count=1 <branch> --not --remotes`), the check T3 Code runs before forced removal (`apps/server/src/storageCleanup.ts:1004-1022`);
- no live process owns it.

`--force` (owner token only, human-only in the PWA) overrides the first two and is recorded. The branch is never deleted, as in herdr (`src/worktree.rs:175-192`, test at `:822`). herdr relies on git's own dirty-check refusal (`src/worktree.rs:194-215`), which misses unpushed commits; Chaldea adds the unpublished check.

### 2.7 Adapter Capability Table

A const table in `crates/core`, keyed by `(agent, os)`, read by the daemon; no `cfg` decides it (spike F-1f). The table is internal; `GET /api/adapters` exposes a read-only summary (public type) so the UI can show what an adapter supports.

| Capability | Claude Code (macOS, Windows) | Codex CLI macOS | Codex CLI Windows | Evidence |
|---|---|---|---|---|
| Hook injection | `--settings <run>/claude-settings.json` + `--session-id <thread uuid>`; global settings untouched | `-c hooks.<Event>=[…]` with fixed command strings, fixed order; per-session URL/token/thread id in env | same | arch §5; prior-art rows 6a, 6b |
| Hook events | `SessionStart`, `UserPromptSubmit`, `Stop`, `SessionEnd`, `PermissionRequest`, `PreToolUse`, `PostToolUse`, `Notification` | `SessionStart`, `UserPromptSubmit`, `Stop`, `PermissionRequest`, `PreToolUse`, `PostToolUse` | same | event names: spike §3 (observed in binaries); the extra events beyond the spike's four are **unverified** until the fixture corpus (PR 11) |
| Session binding | predetermined `--session-id` | first `SessionStart` whose emitter's nearest CLI ancestor is the launched pid | same | spike §4.3 |
| `session_start` | `eager` | `first_turn` | `first_turn` | arch §5 |
| First prompt | PTY input after `SessionStart` | positional argument, **required** at spawn (nothing is written to the PTY before binding) | same | arch §5 |
| Input encoding | bracketed paste, only while DECSET 2004 is set; newline `cr` | bracketed paste; newline `cr` | `win_keys` (below) | arch §5; prior-art rows 4a, 4d; spike F-1f; macOS A-6 passed for both CLIs (spike status line) |
| Submit | one CR, 300 ms after the text write | one CR after 300 ms | one Return after 300 ms, only to a ready composer | prior-art row 4e; arch §5 |
| Ack | `UserPromptSubmit` | `UserPromptSubmit` | `UserPromptSubmit` | arch §4.3 |
| Extra launch flags | — | `--no-daemon` | `--no-daemon`, `-c tui.disable_paste_burst=true` | arch §5 |
| Resume | `--resume <session_id>` (**unverified** in the spike) | `codex resume <session_id>` (**unverified**) | same | PR 12 / PR 13 fixtures |
| Transcript | JSONL from the hook's `transcript_path` | hooks only when `transcript_path` is null; non-null paths are not ingested in the MVP (format not surveyed) | same | arch §4.2, §5 |
| Hook trust | n/a | trust per source/event/position; record CLI version with trust state; a `Hooks need review` stall after an upgrade is expected | same | arch §5 open item; prior-art row 6b |
| Startup landmines | trust prompt defaults to "No, exit": never send Enter blindly | update, folder trust, hook review; digit keys pick options | same | arch §5 |
| Env scrub at spawn | `CLAUDECODE`, `CLAUDE_CODE_*`, `CODEX_THREAD_ID`, `CODEX_SESSION_ID`, other `CODEX_*` session variables, inherited `CHALDEA_*` | same | same | prior-art row 6c; spike §4.3 |

**`win_keys` encoding** (Codex on Windows; first MVP test, arch §5): reject ESC/C0 other than TAB/CR/LF; reject trimmable leading/trailing whitespace and TAB unless explicitly allowed, and empty prompts; NFC-compose each grapheme cluster holding a combining or extending character, keeping CJK compatibility ideographs; map CRLF/CR/LF to one newline; pad with one leading and one trailing space; send ASCII as UTF-8, **every other BMP scalar** as a win32-input-mode key-down + key-up record (`ESC[0;0;<utf16>;1;0;1_` + `ESC[0;0;<utf16>;0;0;1_`), astral scalars as UTF-8; each newline as a Shift+Enter record pair; every escape sequence inside one write; then one Return after 300 ms. The spike's `--unicode-records marks` mode sent records only for Mn/Me/Cf; the planned encoding widens that to every non-ASCII BMP scalar because U+2764 written as UTF-8 was lost (arch §5).

New prior-art evidence, not adopted: herdr keeps Codex's paste-burst detection on and appends one non-character key (Right) after the text on Windows, so Codex flushes the burst before the Enter (`src/app/api/agents.rs:14-33`, 300 ms submit delay at `:13`). It addresses the submit race but not the lost BMP characters, which Chaldea's encoding targets; it stays the fallback to try if PR 14's end-to-end test shows `tui.disable_paste_burst=true` is not enough.

### 2.8 Extension & Rescue Surface (Minimal)

- One-shot JSON hooks `before_spawn`, `after_close`, `attention`: a configured command gets one JSON document on stdin and the environment `CHALDEA_URL`, `CHALDEA_EXTENSION_TOKEN`, `CHALDEA_THREAD_ID`; 10 s timeout; stdout is ignored except for `before_spawn`, whose optional `{ "decision": "deny", "reason" }` blocks the spawn. A timeout or crash of `before_spawn` allows the spawn and records the failure (§8 Q4).
- `POST /events` (extension token): appends events of kind `surface_update` or `raw_unknown` with a caller idempotency key.
- A2UI surfaces: `{ surface_id, scope: { "kind": "host" } | { "kind": "workspace", "workspace_id" } | { "kind": "thread", "thread_id" }, components: [...] }` with the minimal catalog `text`, `markdown`, `vt_snapshot`, `button`, `text_input`, `list`. A `button` carries a `surface_action { action_id, kind: "send_input" | "send_key" | "stop" | "clear_attention" | "open_attach", args }` that runs only when a human with an owner token clicks it, through the command API with a `command_id`.

## 3. Workspace Layout

```
Cargo.toml                 # [workspace] members = crates/*; shared lints; exact-pinned deps (spike policy)
rust-toolchain.toml        # 1.96.0, as the spike
clippy.toml                # disallowed methods/types for crates/core (Sans-IO)
crates/core/               # Sans-IO: types, envelope, state reducers, input state machine, encoders, capability table,
                           # replay decision, auth scope policy, verdict model. No tokio, no std::fs/net/process, no clock.
crates/pty-runtime/        # PtyRuntime trait, FakePty, unix (portable-pty =0.8.1), windows (own windows-sys ConPTY),
                           # alacritty_terminal VT responder and snapshot, process-tree kill, output drain
crates/daemon/             # tokio + axum + rusqlite: store, HTTP API, hook ingest, transcript tailer, supervisor, git
crates/cli/                # binary `chaldea`: client subcommands, `daemon run`, `attach`, `doctor`, `hook-emit`
crates/testkit/            # publish = false: `chaldea-fake-cli` binary and fixtures helpers (FakeCli modes from the spike)
packages/contracts/        # generated TS (ts-rs) + JSON Schema (schemars) + runtime validators
packages/web/              # React + TypeScript strict + Vite + Tailwind PWA, built into the daemon's static assets
fixtures/                  # real-CLI contract corpus: fixtures/<agent>/<cli-version>/<case>/
fuzz/                      # cargo-fuzz targets (nightly, outside the stable workspace)
spikes/pty-hooks/          # unchanged until issue #1 removes it
```

**Dependency direction.** `core` ← `pty-runtime` (uses `core` types: `Key`, `Snapshot`, `InputChunk`) ← `daemon` ← `cli`. `testkit` depends on `core` only. `core` depends on `serde`, `serde_json`, `ts-rs`, `schemars`, `uuid` (parsing only), `unicode-normalization`, `unicode-segmentation`, `subtle`, `sha2`. Sans-IO is enforced: `crates/core/clippy.toml` disallows `std::fs`, `std::net`, `std::process`, `std::time::{Instant, SystemTime}::now`, `std::thread::spawn`, and CI fails on a `tokio` dependency in `core` (`cargo tree -p chaldea-core -e normal` checked in CI).

**Single binary.** `chaldea hook-emit` is a subcommand of the one `chaldea` binary (arch §2). Its dispatch runs before any runtime or config initialization and uses std networking only, so a hook costs one process start. On Windows the binary is console-subsystem; the GUI-subsystem emitter is not needed (prior-art row 9, C-5 `go`).

Options compared for the split:

| Option | Precedent | Verdict |
|---|---|---|
| One crate with modules | herdr: one crate, `src/platform/{unix_common,macos,windows}.rs` | Rejected: Sans-IO and the Windows CI gate (core + FakePty) are easier to enforce at crate boundaries. |
| Protocol crate + server crate | Codex `codex-rs/app-server-protocol` separate from the server, owning the generated schema | **Adopted** in spirit: `core` owns every public type and the generated contracts. |
| Separate store crate | — | Rejected for the MVP: only the daemon uses the store; split later if a second user appears. |

**Spike modules: lift or rewrite.**

| Spike module | Destination | Decision |
|---|---|---|
| `src/os/drain.rs` | `pty-runtime` | Lift (OS-agnostic drain rules, F-2c). |
| `src/os/unix.rs` (killpg, getsid scan, `proc_listallpids`) | `pty-runtime::unix` | Lift the kill and enumeration parts; drop raw-console and probe helpers. |
| `src/os/windows.rs` (raw ConPTY backend, JOB_LIST + `CREATE_SUSPENDED` fallback, jobs, Toolhelp, `DETACHED_PROCESS`, parent job info) | `pty-runtime::windows` (backend, jobs); `daemon` (parent job check) | Lift the backend, handle-owning wrappers, and teardown order; drop the window observer, calibration, keys probe, registry reads. |
| `src/pty.rs` (single writer channel, guards) | `pty-runtime` | Rewrite behind `PtyRuntime`, keeping the single-writer ordering and the revocable gate. |
| `src/vt.rs` | `pty-runtime::vt` | Lift (responder, snapshot walker). |
| `src/paste.rs`, `src/marks.rs` | `core::input` | Lift the pure encoders (validate, newline mapping, escape-safe chunking, `compose_marks`, win_keys, lossy checks) and change `unit_records` use to every non-ASCII BMP scalar; the writer-driving `paste()` / `type_keys()` are rewritten in the daemon. Corpora move to tests. |
| `src/receiver.rs` | `daemon` | Rewrite on axum; keep the binding rules and the B-8 cases as tests. |
| `src/emitter.rs`, `src/http_client.rs` | `cli` (`hook-emit`) | Lift (std networking, limits, exit-0 rules), add `X-Chaldea-Emit-Id`. |
| `src/session.rs` | `daemon` supervisor | Rewrite; lift `scrub_env_name` and the kill ordering. |
| `src/verdict.rs` | `core::verdict` | Lift the outcome model for `doctor` and fixture capture (§7). |
| `src/fake_cli.rs` | `testkit` | Lift the modes used by tests (`echo`, `queries`, `stall`, `banner`, `attachable`, `spawn-tree`, `sleeper`). |
| `src/scenario/*`, `report.rs`, `cli.rs` | — | Not lifted (spike harness). |

Lifted code is copied, not moved: `spikes/` stays buildable and its CI job stays until issue #1.

**Tooling.** CI adds a `workspace` job (macOS + Windows matrix) next to the spike job: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`, the generated-contract drift check, and the `core` dependency check. `lefthook.yml` gets the same commands for the workspace. Node is added to `mise.toml` when `packages/` lands (PR 16), with pnpm; the JS job runs typecheck, Vitest (with fast-check), and Playwright.

## 4. PR Plan

Each PR keeps `core` + `FakePty` tests green on macOS and Windows CI. macOS is implemented first within each PR; the Windows code of the same PR lands in the same PR, not in a later architecture change.

| PR | Scope | Depends on | Milestone(s) closed or advanced |
|---|---|---|---|
| 1 | Workspace, CI job, `core` contracts: ids, envelope, kinds, 3-axis reducer, input state machine, replay decision, command receipt types, `ts-rs` + `schemars` export with drift check (§5) | — | Ingest (types), Sync (types), Verification (PBT for both machines) |
| 2 | `pty-runtime`: `PtyRuntime` trait, `FakePty`, single-writer channel, drain, VT responder + snapshot | 1 | PTY Seam (trait, FakePty, VT) |
| 3 | `pty-runtime::unix`: portable-pty `=0.8.1`, `setsid` + `killpg` HUP→TERM→KILL, `getsid` scan, recorded-pid fallback; `testkit` FakeCli | 2 | PTY Seam (macOS) |
| 4 | `pty-runtime::windows`: own ConPTY (flags 0, `kernel32.dll` only), JOB_LIST + `CREATE_SUSPENDED` fallback, job kill + recorded-descendant fallback, failure-path teardown | 2 (3 for `testkit`) | PTY Seam (Windows) — **closes PTY Seam** |
| 5 | `core::input`: bracketed paste encoder, escape-safe chunking, `win_keys` encoder (planned encoding), lossy checks; adapter capability table | 1 | Agent Control (capability table, encoders) |
| 6 | Daemon store: migrations, append-only triggers, atomic append with projections and transcript cursor, idempotency, receipts, blobs, rebuild property | 1 | Ingest (event log, cursor) |
| 7 | Daemon HTTP: axum on 127.0.0.1, Host/Origin/Bearer/scope middleware, tokens, `GET /api/events`, snapshot, WS ticket + notifications, command receipts; daemon lifecycle (`chaldea daemon run/start/stop`, `DETACHED_PROCESS`), **parent kill-on-close job check** | 6 | Sync (API), Extension (token separation, partial) |
| 8 | Hook ingest: `POST /hook`, per-launch tokens, binding rules, `raw_unknown`; `chaldea hook-emit`; B-8 rejection corpus against the axum receiver on macOS and Windows CI (**Windows hook receiver** open item) | 7 | **Closes Ingest** |
| 9 | Supervisor: `chaldea spawn/stop/wait` with the FakeCli adapter, process_state events, SessionStart timeout + VT snapshot, crash-only restart reconciliation | 3, 4, 5, 8 | Agent Control (spawn/stop/wait) |
| 10 | `chaldea attach`: WS PTY byte stream, raw console on macOS and Windows, resize, detach keys | 9 | Agent Control (attach) |
| 11 | Fixture corpus: layout, capture command (`chaldea doctor capture`) with typed verdict, contract test runner over `fixtures/` | 8, 9 | Verification (corpus) |
| 12 | Claude adapter: settings file, session id, bracketed paste send path, input state machine wired, transcript ingest, resume | 9, 11 | Agent Control (Claude) |
| 13 | Codex adapter: `--no-daemon`, fixed `-c hooks`, positional first prompt, `win_keys` path, `tui.disable_paste_burst=true`, hook-trust stall handling with CLI version (**Codex hook trust renewal** open item), resume | 12 | Agent Control (Codex) — **closes Agent Control** with 10, 15 |
| 14 | **First test 1**: Codex-on-Windows keyed input end to end through `chaldea send` on a real Windows host (**Codex on Windows keyed input** open item) | 13 | Cross-OS E2E (real-host Codex) |
| 15 | Mailbox, `agent` tokens, `task_state` set, worktree ownership (`spawn --worktree`, `worktree remove` checks, short base path) | 9 | Agent Control (mailbox, worktrees) |
| 16 | PWA foundation: `packages/contracts` validators, `packages/web` served by the daemon, pairing, thread list/view via sync API + WS, client reducer, `<textarea>` with IME guards, send | 7 (1 for types) | Sync & PWA (partial) |
| 17 | PWA approvals, VT snapshot view, emergency keypad; Playwright desktop + mobile, IME composition tests | 16, 12 | **Closes Sync & PWA** |
| 18 | **First test 2**: attach applies `win_keys` to human pastes for Codex on Windows (outer-terminal bracketed paste re-encoded, Shift+Enter records), verified end to end on a real Windows host (**human paste through attach** open item) | 10, 13 | Cross-OS E2E |
| 19 | Extension & Rescue: one-shot hooks, `POST /events`, rescue token, A2UI minimal catalog, `surface_action`; `stateright` Sync model | 15, 17 | **Closes Extension & Rescue** |
| 20 | `chaldea doctor` round-trip checks (§7) | 13, 15 | Cross-OS E2E (doctor) |
| 21 | `cargo-fuzz` targets (JSONL line parser, hook payload decoder, VT responder) + scheduled CI run | 5, 8, 12 | **Closes Verification** |
| 22 | Real-host verification run: macOS and Windows checklist from §6 purpose layer, Claude + Codex, recorded with versions; additional Windows builds where available (**Windows build coverage** open item) | all | **Closes Cross-OS CI & E2E** |

**Critical path.** 1 → 2 → 4 → 9 → 12 → 13 → 14. In parallel: 3 (macOS backend) and 4 after 2; 5 and 6 after 1; 6 → 7 → 8 before 9; 11 before 12; 16 after 7. Windows (PR 4) sits on the critical path deliberately: the supervisor (PR 9) is not written until both backends exist behind the trait.

**Why the first tests are not earlier.** Both first tests are end to end by definition (arch §5, issue #2): they need the Windows backend, the encoder, hook ingest, and the Codex launch path. A failure would change only the encoder and the capability table in `core` (PR 5), not a contract, so running them after PR 13 risks no rework of earlier PRs. PR 5 carries the encoder's unit and property tests on both OSes from the start.

**Open items of arch §5.**

| Open item | PR | Note |
|---|---|---|
| Codex on Windows keyed input | 14 | First test 1. |
| Human pastes through `chaldea attach` to Codex on Windows | 18 | First test 2. |
| Codex hook trust renewal after upgrades | 13, 20 | Adapter records CLI version + trust state; doctor reports a version change since the last trusted launch. |
| Windows hook receiver (B-8 `deadline_slow_head`, `non_numeric_content_length`) | 8 | The spike receiver is replaced by axum/hyper; the 22 B-8 cases are rerun against it on `windows-latest` CI and in PR 22 on the real host. The spike's two failures are not assumed to carry over or to be fixed. |
| Daemon parent job check | 7 | At daemon start on Windows: `IsProcessInJob` + `QueryInformationJobObject`; a kill-on-close parent job is reported and the daemon relaunches itself with `CREATE_BREAKAWAY_FROM_JOB` when allowed, else refuses to start in background mode with a message (prior-art row 8). |
| Windows build coverage | 22 | CI runners give one more Windows build for FakeCli-level ConPTY behavior; real-CLI runs on more builds need more hosts and stay open until then. |
| prior-art "Not Verified" items | 22 or none | The ones that affect behavior (inbox conhost with flags 0 on other builds, BMP characters delivered only on Alt release) are covered by PR 14 and 22; the others (ureq proxy, VS Code, Zellij) do not affect the MVP and stay unverified. |

## 5. Detailed Design: PR 1

**Goal.** A Cargo workspace whose `crates/core` holds the irreversible contracts of §2.1–§2.4 as types and pure functions, generated TypeScript and JSON Schema committed with a drift check, and a CI job running on macOS and Windows.

**Files.**

```
Cargo.toml                                   # [workspace] members = ["crates/core"], resolver = "3",
                                             # [workspace.package] edition = "2024", license = "MIT OR Apache-2.0"
                                             # [workspace.lints] rust.unsafe_code = "forbid" (core), clippy pedantic subset
rust-toolchain.toml                          # channel = "1.96.0", components = ["rustfmt", "clippy"]
crates/core/Cargo.toml                       # name = "chaldea-core"; deps exact-pinned: serde, serde_json, ts-rs, schemars, uuid (serde)
                                             # dev-deps: proptest
crates/core/clippy.toml                      # disallowed-methods / disallowed-types (Sans-IO list, §3)
crates/core/src/lib.rs
crates/core/src/ids.rs                       # ThreadId, EventId, CommandId, InputId, LaunchId (UUID newtypes), StreamId, Seq, Epoch
crates/core/src/event.rs                     # EventEnvelope, NewEvent, EventKind, Source, SourceKind, SCHEMA_VERSION, IdempotencyKey
crates/core/src/payload.rs                   # payload structs per kind (status_changed, input_state_changed, raw_unknown, …)
crates/core/src/state.rs                     # ProcessState, TurnState, TaskState, ThreadAxes, StateInput, reduce()
crates/core/src/input.rs                     # InputState, InputInput, InputRecord, step()
crates/core/src/sync.rs                      # EventsQuery, EventsResponse, ReplayPlan, decide_replay(), WsServerMsg, WsClientMsg
crates/core/src/command.rs                   # CommandEnvelope<T>, CommandReceipt, ReceiptLookup, check_receipt()
crates/core/tests/contracts_export.rs        # generates TS + JSON Schema into a temp dir and diffs with committed files
crates/core/tests/golden/                    # golden JSON for one event of each kind
packages/contracts/src/generated/            # committed ts-rs output
packages/contracts/schema/                   # committed schemars output
.github/workflows/ci.yml                     # new `workspace` job (matrix macos-latest, windows-latest)
lefthook.yml                                 # workspace fmt / clippy (pre-commit), test (pre-push)
```

`packages/contracts` gets no `package.json` until PR 16; the generated files exist from PR 1 so their drift is checked from the first contract.

**Types and functions** (signatures; names may be adjusted in review, shapes may not):

```rust
pub const SCHEMA_VERSION: u32 = 1;

pub enum StreamId { Host, Thread(ThreadId) }                  // serialized as "host" / "thread:<uuid>"

pub struct EventEnvelope {
    pub stream_id: StreamId, pub seq: u64, pub event_id: EventId, pub schema_version: u32,
    pub idempotency_key: IdempotencyKey, pub thread_id: Option<ThreadId>, pub command_id: Option<CommandId>,
    pub source: Source, pub occurred_at: Timestamp, pub recorded_at: Timestamp,
    #[serde(flatten)] pub body: EventBody,                      // tagged by "kind", content in "payload"
}
pub enum EventBody { UserMessage(UserMessage), AssistantMessage(AssistantMessage), ToolActivity(ToolActivity),
    StatusChanged(StatusChanged), ApprovalRequested(..), ApprovalResolved(..), InputStateChanged(..),
    MailboxMessage(..), ArtifactDiff(..), SurfaceUpdate(..), VtSnapshotTaken(..), ThreadRegistered(..),
    ThreadClosed(..), WorktreeChanged(..), RawUnknown(RawUnknown) }
pub struct NewEvent { /* envelope without seq / recorded_at; the store assigns them */ }

impl IdempotencyKey {
    pub fn hook(emit_id: Uuid) -> Self; pub fn transcript(generation: u64, offset: u64) -> Self;
    pub fn process(launch: LaunchId, what: ProcessMark) -> Self; pub fn command(id: CommandId, n: u32) -> Self;
    pub fn derived(cause: &IdempotencyKey, name: &str) -> Self; pub fn extension(caller_key: &str) -> Result<Self, KeyError>;
}

pub enum StateInput {                                          // no variant carries PTY output
    Registered { task: bool }, Spawned, SpawnFailed, SessionBound { eager: bool }, StopRequested,
    Exited { reason: ExitReason }, PromptSubmitted, TurnStopped, ApprovalRequested, ApprovalResolved,
    AttentionRaised { reason: AttentionReason }, AttentionCleared, TaskSet { to: TaskState, by: Actor },
}
pub struct ThreadAxes { pub process: ProcessState, pub turn: TurnState, pub task: TaskState }
pub fn reduce(axes: ThreadAxes, input: &StateInput) -> Result<(ThreadAxes, Vec<StatusChanged>), InvalidTransition>;

pub enum InputState { Queued, Sent, Acked, Unconfirmed, Rejected, Cancelled }
pub enum InputInput { WriteCompleted, WriteOutcomeUnknown, Rejected { reason }, Cancelled, AckObserved, AckWindowExpired }
pub fn step(state: InputState, input: &InputInput) -> Result<InputState, InvalidTransition>;

pub fn decide_replay(q: &EventsQuery, head_seq: u64, epoch: &Epoch, retained_from: u64,
                     replay_events: u64, replay_bytes: u64) -> ReplayPlan;   // Replay { after, through } | Snapshot { reason }

pub fn check_receipt(existing: Option<&CommandReceipt>, request_sha256: &[u8; 32]) -> ReceiptLookup; // New | Replay(result) | Conflict
```

**Tests** (all run on macOS and Windows):

- `state` property tests (`proptest`): for any sequence of `StateInput`, `reduce` never panics; `task` changes only on `TaskSet`; after `Exited`, `turn` is `unknown` until a new `Registered`; every change is reported in exactly one `StatusChanged`.
- `input` property tests: for any input sequence, an input reaches `Sent` at most once; no path leads from `Unconfirmed` to `Sent` or `Queued`; `Rejected` and `Cancelled` are reachable only from `Queued`.
- `decide_replay` table tests for each fallback reason and the replay case.
- `check_receipt`: same hash → replay, other hash → conflict.
- Serde round trip and golden JSON for one event per kind and for `EventsResponse` both modes, `WsServerMsg`, `WsClientMsg`.
- `contracts_export`: generated TS and JSON Schema equal the committed files (fails with the path of the first differing file; regenerating is one documented command, `cargo test -p chaldea-core --test contracts_export -- --ignored regenerate`).
- CI check that `chaldea-core` has no `tokio` in its normal dependency tree.

**Completion commands.**

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo tree -p chaldea-core -e normal --prefix none | grep -c '^tokio' # must print 0
```

plus the `workspace` job green on `macos-latest` and `windows-latest`, and the existing `pty-hooks` job unchanged and green.

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked on the real thing (real hosts, real CLIs, the built `chaldea` binary and PWA). "Owner evaluation" marks subjective items; "unverified" marks items with no means of checking in the MVP.

| ID | Criterion (EARS) | Source | Scene | Observable result / failure | How to check on the real thing |
|---|---|---|---|---|---|
| P-1 | When a user runs `chaldea spawn --agent claude` in a trusted repository, Claude Code shall run in a daemon-owned PTY with the user's own skills, hooks, and permission settings working, and global hook settings unchanged. | arch §1 native fidelity; issue #2 | macOS and Windows host | Pass: `chaldea attach` shows the native TUI; a user skill and `/` command work; `~/.claude/settings.json` hooks unchanged. Fail: a feature missing, a settings diff. | Real Claude on both hosts; diff settings before/after. |
| P-2 | When a user runs `chaldea spawn --agent codex --prompt …`, Codex shall run with `--no-daemon`, its hooks shall deliver without a review dialog after one initial trust, and its MCP servers shall stay inside the launched tree. | arch §5 | both hosts | Pass: bound `SessionStart`, no review dialog on relaunch, MCP server pid under the CLI. Fail: review dialog, MCP server parented by a shared daemon. | Real Codex, 3 launches; process tree listing. |
| P-3 | When `chaldea stop` is run for a thread, no process of its tree (including MCP servers) shall be alive 5 s later, and an unrelated process shall survive. | arch §4.5; spike A-8, C-3 | both hosts | Pass: empty survivor list. Fail: survivor. | Real CLIs with an MCP sleeper; process listing after stop. |
| P-4 | When an orchestrator thread spawns a child with `--worktree` and waits for it, the orchestrator shall receive the child's 3-axis state and mailbox messages, and the child's worktree with uncommitted changes shall still exist after the child exits and after a daemon restart. | arch §1 orchestration | macOS host, real Claude as orchestrator | Pass: `chaldea wait` returns, `chaldea mailbox` lists the message, worktree files present. Fail: lost files, missing outcome. | Scripted run with real CLIs; check the worktree after restart. |
| P-5 | If removal is requested for a worktree with uncommitted changes or commits not on any remote-tracking ref, then Chaldea shall refuse and leave the worktree and branch unchanged. | arch §1, AGENTS.md invariant 3 | both hosts | Pass: refusal with the reason; files and branch intact. Fail: anything removed. | Real repo with a dirty file and with a local-only commit. |
| P-6 | When a prompt containing Japanese, emoji, decomposed marks, and newlines is sent to Claude Code through `chaldea send` or the PWA, the prompt Claude records shall equal the expected form (Claude's documented transforms applied). | arch §1 IME; invariant 4 | both hosts | Pass: `user_message` equals the expected form. Fail: any other difference. | Real Claude with the spike corpus; compare events. |
| P-7 | When the same corpus is sent to Codex on Windows through `chaldea send`, each prompt shall arrive as exactly one `UserPromptSubmit` equal to the expected form, followed by that turn's `Stop`. | arch §5 first test; issue #2 | Windows host | Pass: one submit per prompt, equal. Fail: split, joined, marks lost, early submit. | PR 14 run on a real Windows host, recorded with Codex version. |
| P-8 | When a human pastes multi-line Japanese text through `chaldea attach` into Codex on Windows, the paste shall arrive as one prompt with its newlines and shall not submit or queue before the human presses Enter. | arch §5 open item; issue #2 | Windows host, Windows Terminal as outer terminal | Pass: one prompt, submitted only on the human's Enter. Fail: early submit, Tab queueing, lost newlines. | PR 18 manual run. |
| P-9 | While composing Japanese in the PWA prompt input, pressing Enter to confirm a conversion shall not send the prompt. | arch §1, §2.4; invariant 4 | macOS (Kotoeri / Google IME), Windows (MS-IME) desktop browsers | Pass: no send during composition. Fail: send on conversion Enter. | Manual with real IMEs; Playwright covers the event sequence (P-plan). Android IME: unverified in the MVP (no remote access until Phase 2). |
| P-10 | When a CLI stalls on a startup prompt (trust, update, hook review) and no `SessionStart` arrives within the timeout, the thread shall show `needs_attention` with a legible VT snapshot, and the keypad shall clear the prompt. | arch §1 self-healing, §2 landmine | both hosts, fresh untrusted folder | Pass: snapshot shows the prompt, keypad keys reach the CLI, thread proceeds. Fail: no attention, illegible snapshot, keys lost. | Real CLIs in a fresh folder; legibility is owner evaluation. |
| P-11 | If a hook payload or transcript line cannot be parsed or bound, then Chaldea shall keep it verbatim as `raw_unknown` with the CLI version, visible in the thread or host stream. | arch §1 self-healing | both hosts | Pass: event present with raw bytes. Fail: dropped. | Send a malformed payload with a valid hook token; nested CLI case. |
| P-12 | If an input is not acknowledged within the ack window, then Chaldea shall mark it `unconfirmed`, raise attention, and never send it again on its own. | arch §4.3 | both hosts | Pass: exactly one write in the PTY log; state `unconfirmed`. Fail: a second submit. | Real CLI with a dialog open over the composer. |
| P-13 | When an approval is requested by the CLI, the PWA shall show it in the thread and let the user resolve it. | issue #2 PWA; arch §1 | both hosts | Pass: card appears; resolution reaches the CLI. Fail: no card, decision lost. | Real Claude tool permission (mechanism per §8 Q1). |
| P-14 | When the daemon is killed and restarted, every thread that was running shall show `exited` (`daemon_restart`) with its history intact, and a resume shall continue the CLI session. | arch §2 crash-only | both hosts | Pass: history equal, resumed session remembers context. Fail: lost events, duplicate events, resume fails. | Kill daemon by PID mid-turn; restart; resume. |
| P-15 | When the PWA reconnects after the daemon was unreachable, it shall show the time of last sync while offline and catch up without duplicate or missing events. | arch §4.1 | desktop browser | Pass: banner while offline, event list equals the server's after. Fail: duplicates, gaps. | Stop and start the daemon while the PWA is open. |
| P-16 | If a request has a wrong `Host`, a missing or wrong token, or a scope it lacks (e.g. Rescue sending input), then the daemon shall reject it and change nothing. | AGENTS.md invariant 5 | both hosts | Pass: 421/401/403, no event. Fail: accepted. | `curl` probes against the real daemon. |
| P-17 | When a Rescue Agent emits a Thread-scoped A2UI panel with a button, the button's action shall run only after a human clicks it, and only on that thread. | arch §1, §4.4 | macOS host | Pass: action event with the human's command id. Fail: action without a click, wrong thread. | Real Claude as Rescue Agent. |
| P-18 | While Chaldea spawns CLIs and runs hooks on Windows, no console window shall appear. | prior-art row 9; spike C-5 | Windows host | Pass: no visible window. Fail: a flash. | Owner evaluation by watching the desktop (the spike's observer is not lifted). |
| P-19 | When `chaldea doctor` runs, it shall report each check as pass, fail, or inconclusive with evidence, and report inconclusive (never pass) for any check it could not run. | arch §2 landmine, §6 | both hosts | Pass: report matches the host's state, including a deliberately broken check. Fail: pass without evidence. | Run on both hosts, once with the hook port blocked. |
| P-20 | Chaldea shall be usable on a phone-sized viewport for reading threads, short prompts, approvals, and the keypad. | arch §1 | mobile viewport in a desktop browser | Layout usability is owner evaluation; real Android access is unverified until Phase 2. | Playwright mobile viewport screenshots + manual review. |

### 6.2 Plan Layer: Claims and Falsifiers

| ID | Claim (EARS) | Source | Test that would falsify it |
|---|---|---|---|
| C-1 | The thread state reducer shall accept only typed explicit inputs, none built from PTY output. | invariant 1; §2.2 | Compile-time: a `StateInput` variant holding bytes or text from the PTY (code review + PR 1 type); property test of `reduce`. |
| C-2 | `task_state` shall change only on an explicit `TaskSet`. | §2.2 | PR 1 proptest finding a change on another input. |
| C-3 | An input shall reach `sent` at most once, and never from `unconfirmed`. | arch §4.3; §2.3 | PR 1 proptest; PR 12/13 PTY log showing a second submit. |
| C-4 | Each stream's `seq` shall be gapless and monotonic under concurrent appends and crash recovery. | §2.1 | PR 6 concurrent append test; `stateright` model (PR 19) finding a gap. |
| C-5 | A replayed source (hook retry, transcript reread) shall not create a duplicate event. | §2.1 idempotency | PR 6 test appending the same key twice; PR 9 crash-replay test. |
| C-6 | Projections rebuilt from events shall equal the incrementally maintained projections. | §2.1 | PR 6 property test. |
| C-7 | Transcript events and the cursor shall commit atomically. | arch §4.2 | PR 6/12 fault injection between batch and cursor write, then restart: duplicates or gaps falsify. |
| C-8 | A client following `decide_replay` shall end with the server's events for the stream, without duplicates, after any sequence of disconnects, restarts, and lost responses. | arch §4.1, §6 | `stateright` model (PR 19); fast-check client reducer (PR 16). |
| C-9 | A repeated `command_id` shall cause at most one effect. | arch §4.1 | PR 7 test resending a send/key command; PTY write count. |
| C-10 | Requests with a bad Host, token, scope, framing, or size shall be rejected before any body byte is read and leave no event. | invariant 5; spike B-8 | PR 8 B-8 corpus on macOS and Windows CI. |
| C-11 | A hook from a nested CLI inheriting the environment shall not bind as the parent thread. | prior-art row 6c; spike B-11 | PR 8 FakeCli `echo --nested`. |
| C-12 | The bracketed paste encoder shall write zero bytes for ESC or C0 controls other than TAB/CR/LF, and never split an escape sequence across writes. | prior-art row 4b; spike F-3d | PR 5 unit + proptest; PR 21 fuzz. |
| C-13 | The `win_keys` encoder shall produce records for every non-ASCII BMP scalar, UTF-8 for ASCII and astral scalars, one Shift+Enter record pair per newline, and the spike's composition and padding rules. | arch §5 | PR 5 unit tests over the spike's corpus F; PR 14 real-host run. |
| C-14 | The input encoding shall be chosen from the capability table by `(agent, os)`, never by DECSET 2004. | spike F-1f | PR 5 table test; PR 12/13 Claude-on-Windows uses bracketed paste. |
| C-15 | Bracketed paste shall be written only while the CLI has DECSET 2004 set and the thread is bound and idle. | prior-art row 4a | PR 12 FakeCli without 2004: input stays `queued`. |
| C-16 | Stop shall leave no owned process alive on macOS (`killpg` + `getsid` + fallback) and Windows (job + recorded descendants). | arch §4.5; spike B-7, C-3 | PR 3/4 FakeCli `spawn-tree`; PR 22 real CLIs with MCP witness. |
| C-17 | The Windows backend shall launch with the JOB_LIST branch on supported builds and leave no process or handle behind on every injected setup failure. | prior-art rows 1b, 1d; spike F-2c | PR 4 Windows CI with injections; handle count. |
| C-18 | The daemon shall detect a kill-on-close parent job on Windows at startup. | arch §5 open item; prior-art row 8 | PR 7 test starting the daemon inside a kill-on-close job on Windows CI. |
| C-19 | `core` shall build and pass its tests without tokio, filesystem, network, process, or clock access. | §3 Sans-IO | CI dependency check; clippy disallowed list. |
| C-20 | Generated TS and JSON Schema shall equal what the Rust types generate. | §2 | PR 1 drift test. |
| C-21 | No code path shall remove a worktree except the explicit remove command passing the checks or `--force` with an owner token. | arch §4.3 | PR 15 tests: thread exit, close, restart leave worktree; remove with dirty file / local-only commit refused. |
| C-22 | The PWA prompt input shall not submit while `isComposing`, on `keyCode === 229`, or on the Enter right after `compositionend`. | invariant 4 | PR 17 Playwright composition event tests (desktop + mobile viewports). |
| C-23 | A hook emitter run shall exit 0, write nothing to stdout, and finish within 3 s whatever the daemon does. | spike B-9 | PR 8 emitter tests (daemon down, slow, rejecting). |
| C-24 | A Rescue token shall not cause input, key, stop, or worktree removal. | arch §4.4 | PR 19 scope tests. |

## 7. Verification Plan

| arch §6 item | What | PR |
|---|---|---|
| Real-CLI contract corpus (`fixtures/`) | `fixtures/<agent>/<cli-version>/<case>/` holding hook payloads (`<Event>-<n>.json` + `.meta.json` with allowlisted fields, as spike §4.3) and transcript excerpts, captured by `chaldea doctor capture`, scrubbed of tokens, paths under the home directory, and prompts other than the corpus. Contract tests decode every fixture through the adapter decoders; an unknown field is fine, a decode into `raw_unknown` of a known event fails the test. New CLI versions add a directory; old ones stay. | 11 (layout, capture, runner); 12, 13 (Claude, Codex fixtures) |
| PBT (`proptest` / `fast-check`) | 3-axis reducer, input state machine (PR 1); encoders (PR 5); projection rebuild (PR 6); client reducer against generated event sequences (PR 16, fast-check). | 1, 5, 6, 16 |
| `stateright` | Scoped to the Sync Seam: startup crash during append, offline buffering in the client, retry after a lost command response. Model actors: store, daemon API, client. Properties: no gap, no duplicate, one effect per `command_id`. | 19 |
| `cargo-fuzz` | Targets: JSONL line parser (transcript), hook payload decoder (per adapter), VT escape responder (`pty-runtime::vt` fed arbitrary bytes, replies checked for one-write sequences). Nightly, scheduled CI, corpus seeded from `fixtures/`. | 21 |
| Cross-OS CI | macOS + Windows matrix for the workspace job from PR 1; FakePty and FakeCli tests in every PR; Windows backend injections in PR 4. | 1 onward |
| `chaldea doctor` | Round trip on the real host: daemon reachable with valid Host/token; hook round trip through `chaldea hook-emit`; FakeCli spawn in a PTY, VT query reply, paste echo, tree kill; Windows parent job; ConPTY module is `kernel32`; worktree base path length; CLI versions and Codex hook trust state vs last trusted launch. | 20 |
| Playwright | Desktop and mobile viewports; IME guards via synthetic `compositionstart/update/end` and `keyCode 229` sequences; send, approval, keypad flows against a daemon running FakeCli. | 16, 17 |
| Real-host E2E | §6.1 checklist on macOS and Windows hosts with versions recorded. | 14, 18, 22 |

**Verdicts of evidence-collecting harnesses.** `chaldea doctor`, `chaldea doctor capture`, and the real-host runs of PR 14, 18, and 22 decide their verdict in one function, `core::verdict::evaluate(required: &[CheckId], checks: &[Check]) -> Verdict`, lifted from the spike (`spikes/pty-hooks/src/verdict.rs`). Each check's outcome is typed: `Pass`, `PlatformFail { cause }` (only with positive evidence of what the CLI or OS did), `HarnessFail { cause }` (setup or observation failure), `Missing` (no result), `NotExercised { reason }`. Completeness is checked first: the verdict is `Inconclusive` when any required check is `Missing`, `HarnessFail`, or `NotExercised`; otherwise `Pass` when all passed, else `Fail`. Missing evidence, an observation failure, or a check that did not run is never `Pass` (spike §1). Doctor's JSON report carries every check with its evidence and the verdict.

## 8. Open Questions

Only items that change public behavior or are irreversible. Everything else is decided above with its reason.

**Q1. How approvals are answered.**

| Option | Gains | Loses | Reversibility | Cost |
|---|---|---|---|---|
| A. Hook decision: for `PermissionRequest`, `chaldea hook-emit` waits (long poll, bounded by the hook timeout) for the user's decision and prints the CLI's decision JSON on stdout. | A real Approve/Deny button; no keys sent blind. | Changes the emitter contract (it now writes stdout and blocks); depends on each CLI's hook decision schema (Claude: documented in its hooks docs, not verified in the spike; Codex: **unverified**). | Medium: the emitter command string is part of Codex's trust key, so changing it later re-triggers `Hooks need review` (arch §5). | Fixture work per CLI version. |
| B. Keypad only: the approval card shows the hook's tool and arguments plus the VT snapshot; the user answers with keypad keys. | No emitter change; works for every CLI. | The user reads the dialog from the snapshot and presses keys; slower on a phone. | High. | Low. |

Recommendation: B in the MVP for both CLIs, and A for Claude only once the fixture corpus shows its decision schema; for Codex, A needs a hook command string that stays fixed, so its design must be settled before Codex hooks ship to users. Changes if: owner requires one-tap approval in the MVP, in which case A for Claude goes into PR 12 behind the fixture test.

**Q2. CLI surface for `task_state`.**

| Option | Gains | Loses | Reversibility | Cost |
|---|---|---|---|---|
| A. `chaldea task set <state> [--summary …] [--thread …]` | Explicit and greppable in agent instructions. | One more command than issue #2 lists. | CLI flags are public; adding is easy, removing is not. | Low. |
| B. `chaldea mailbox send --task-state <state>` | No new command. | Couples a state change to a message; a state change without a message is awkward. | Same. | Low. |

Recommendation: A. Changes if: the command list of issue #2 is meant to be closed.

**Q3. Extra input states `rejected` and `cancelled`.** The architecture names `queued -> sent -> acked / unconfirmed`. Options: (A) add `rejected` and `cancelled` as pre-write terminal states (§2.3); (B) keep four states and express pre-write failures as `unconfirmed` with a reason. A keeps `unconfirmed` meaning "may have reached the CLI", which the never-resend rule depends on; B makes every validation failure look like a possible delivery and blocks the queue. Both are data formats in `input_state_changed`, hard to change later. Recommendation: A, with the arch §4.3 edit below. Changes if: the four-state list is meant to be exhaustive.

**Q4. Failure policy of `before_spawn`.** Options: fail-open (timeout or crash allows the spawn, failure recorded; §2.8) or fail-closed (deny). Fail-open keeps a broken extension from blocking all work; fail-closed suits policy-enforcement hooks. Public behavior of the Extension Seam. Recommendation: fail-open by default with a per-hook `fail_closed = true` option. Changes if: `before_spawn` is meant for security policy first.

## Proposed Canonical-Doc Changes

Not applied here; each is a proposal for `docs/architecture.md`, `docs/prior-art.md`, or `AGENTS.md`.

- **arch §4.1**: add the `epoch` parameter, the `mode: replay | snapshot` response, notify-then-pull WS messages, and WS auth by single-use ticket plus `Origin` check (browsers cannot send `Authorization` on a WebSocket).
- **arch §4.2**: extend the event kind list with `approval_resolved`, `input_state_changed`, `mailbox_message`, `vt_snapshot_taken`, `thread_registered`, `thread_closed`, `worktree_changed`; state the two stream kinds (`host`, `thread:<id>`).
- **arch §4.3**: if Q3 A is accepted, `queued -> sent -> acked / unconfirmed`, plus `rejected` / `cancelled` before any byte is written.
- **arch §2 / AGENTS.md invariant 5**: name the token kinds (`owner`, `agent`, `hook`, `rescue`, `extension`) next to "regular vs Rescue".
- **arch §5 open items**: the Windows hook receiver item now concerns the axum receiver (PR 8); the spike receiver's two cases do not carry over.
- **prior-art**: add rows for event store and replay (T3 Code v1/v2 tables, `decideThreadResume`), command receipts, scoped auth and pairing (T3 Code), thread/turn status and schema fixture tests (Codex app-server), worktree removal checks (T3 Code unpushed check vs herdr git refusal), and herdr's Codex-on-Windows paste-boundary key (`src/app/api/agents.rs:14-33`) as an alternative to `tui.disable_paste_burst=true`.
- **README status**: "MVP implementation has not started" changes when PR 1 merges.
