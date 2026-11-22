# MVP Design: Local-First Chaldea

Status: design, not implemented. It plans the local-first MVP of issue #2. Canonical constraints: `AGENTS.md` and `docs/architecture.md` (cited as "arch §N"). Mechanism decisions: `docs/prior-art.md` (cited as "prior-art row N"). Spike evidence: `docs/spikes/pty-hooks-spike.md` (cited as "spike §N" or by its case ids such as "spike A-8", with report ids) and `spikes/pty-hooks/src/`.

**What this document fixes.** Only what PR 1 freezes or what every later PR depends on: the public wire types and payloads, the state machines and their transition rules, the HTTP/CLI surface and its authorization matrix, the receipt and idempotency contract, the sync API contract, and the PR plan. Behaviour owned by one later PR is listed as that PR's **design obligations** (§4) and designed in its own document when the PR starts.

Additional prior art read for this document (shallow clones, default branch on 2026-11-21; paths are relative to each repository root):

| Source | Revision | Read for |
|---|---|---|
| T3 Code (`pingdotgg/t3code`) | `10586f9e` | event store, command receipts, replay/snapshot resume, auth scopes, worktree cleanup |
| Codex app-server protocol (`openai/codex`, sparse `codex-rs/app-server-protocol`) | `17cb2e3` | thread/turn status model, approvals, `ts-rs` export and schema fixture tests |
| herdr (`herdrdev/herdr`) | `e8d7d97` | agent state model, prompt submission, worktree removal |
| Claude Code hooks reference (`code.claude.com/docs/en/hooks`) | read 2026-11-22 | `PermissionRequest` input and decision output, hook timeouts |

Zed is cited only through prior-art rows. Anything not read in source or docs is marked **unverified**.

## 1. Scope and Non-Goals

**Goal (issue #2).** A Rust Sans-IO core (`crates/core`, `crates/pty-runtime`, `crates/daemon`), daemon-owned PTYs, authenticated hook ingest, an SQLite append-only event store, the `chaldea` CLI (`spawn`, `send`, `wait`, `stop`, `mailbox`, `attach`, `doctor`, `hook-emit`), and a local single-origin PWA (`packages/contracts`, `packages/web`), on macOS and native Windows hosts.

**In scope.** The irreversible core of arch §2 (§2 below); the five seams of arch §4 at the depth the milestones of issue #2 need; the minimum UI (thread list, thread view with events and state, plain `<textarea>` prompt input with IME guards, approval card, read-only VT snapshot, emergency VT keypad, usable on mobile viewports); the minimal A2UI catalog (§2.8) and `surface_action`.

**Non-goals.** The Phase 2 relay (`packages/relay`) and therefore Android remote access; the local PWA binds to `127.0.0.1` only, and contracts leave room for the relay without implementing it. Every reversible layer of arch §2 (Tauri / Capacitor, diff view, split panes, visual styling, a separate PTY-owning process, A2UI beyond the minimal set); `docs/design/chaldea.pen` remains wireframes only. A client-side terminal emulator (invariant 2; the PWA shows snapshots, full access is `chaldea attach`). Orchestration strategy in daemon code (invariant 2). Removing `spikes/` (issue #1).

## 2. Irreversible-Core Contracts

Each contract lists the options compared, the decision, and which parts are **public** (generated `ts-rs` types, HTTP API, CLI flags, environment variables) or **internal** (may change in any PR without migration notes).

All public types are Rust types in `crates/core` with `serde` and `ts-rs` derives. Generated TypeScript is committed under `packages/contracts/src/generated/` and checked for drift in CI, as Codex's app-server does for its committed `schema/typescript` and `schema/json` fixtures (`codex-rs/app-server-protocol/src/schema_fixtures_tests.rs:17-41`; `ts-rs` derives at `src/protocol/v2/thread.rs:1821-1823`). JSON Schema is generated alongside with `schemars` for runtime validation in `packages/contracts`.

### 2.1 Event Envelope, Event Kinds, Store

**Streams.** Every event belongs to one stream: `host` (thread registration and closing, worktrees, host-scoped surfaces, unbound `raw_unknown`) or `thread:<thread_id>` (everything about that thread, across all its launches). `seq` is per-stream, gapless, monotonic from 1, assigned inside the append transaction. An internal `global_seq` orders events across streams and is not exposed.

| Option | Precedent | Verdict |
|---|---|---|
| A. One global sequence; clients filter by thread | T3 Code orchestration v2: `sequence INTEGER PRIMARY KEY AUTOINCREMENT` with `thread_id` indexes (`apps/server/src/persistence/Migrations/055_OrchestrationV2.ts:20-38`) | Rejected: gaps inside one thread are not detectable, and a Phase 2 per-thread cache must re-derive order. |
| B. Per-stream version plus global sequence | T3 Code orchestration v1: `stream_id`, `stream_version`, unique `(aggregate_kind, stream_id, stream_version)`, global `sequence` (`Migrations/001_OrchestrationEvents.ts:8-28`) | **Adopted** as `stream_id` + `seq`; it matches arch §4.1's `stream_id + after_seq` directly. |
| C. Per-item notifications with no replay cursor | Codex app-server `thread/status/changed`, `turn/started`, `item/completed` (`src/protocol/common.rs:2087`, `:2116`, `:2127`) | Rejected for storage (a live protocol, not a log); its vocabulary informs §2.2. |

**Envelope** (public):

| Field | Type | Meaning |
|---|---|---|
| `stream_id` | string | `host` or `thread:<uuid>` |
| `seq` | u64 | per-stream, gapless, from 1 |
| `event_id` | UUID | unique per event; generated by the daemon and passed into core |
| `schema_version` | u32 | version of the event schema set |
| `idempotency_key` | string | dedupe key, unique per stream |
| `kind` | string | event kind; an open set on the wire |
| `thread_id`, `launch_id`, `command_id` | UUID or null | the thread, the launch (one process lifetime of a thread), and the causing command |
| `source` | object | `{ kind: "hook" \| "transcript" \| "process" \| "command" \| "daemon" \| "extension", agent, cli_version, transcript: { generation, path, file_id, offset_end } \| null }` |
| `occurred_at`, `recorded_at` | RFC 3339 UTC, ms | source time (or `recorded_at` when the source states none); append time |
| `payload` | object | kind-specific |

**Open envelope and forward-compatible decoding.** Readers validate the envelope first, with `kind` as a string and `payload` as raw JSON, then decode the payload by kind. An unknown `kind`, a `schema_version` above the reader's, or a payload that fails its schema becomes a `GenericEvent` (envelope plus raw payload), rendered as "unknown event" in the PWA and printed as JSON by the CLI; it never fails a page or a stream. Rust: `EventEnvelope { kind: EventKindTag, payload: Box<RawValue>, … }` with `TypedEvent::decode(&EventEnvelope) -> Result<TypedEvent, GenericEvent>`. TypeScript: `packages/contracts` validates the open envelope with the generated JSON Schema, then maps it to the generated union of known kinds or to `GenericEvent`. No `#[serde(flatten)]` is used on the envelope.

**`Content`** (public) is the type of every payload field that can be large (`text`, `prompt`, `output`, `raw`, VT grids): `{ "inline": string, "encoding": "utf8" | "base64" } | { "blob": { "sha256", "size", "media_type" }, "preview": string }`. `utf8` carries text, `base64` carries bytes that are not valid UTF-8. Above 64 KiB the content moves to a blob and `preview` holds its first 1 KiB on a UTF-8 boundary; the rest of the payload stays inline (arch §4.2 size-bounded outputs).

**`schema_version`** is one integer for the whole schema set, starting at 1. Additive changes (an optional field, a new kind) do not bump it; a change of meaning or type of an existing field does. Per-kind versions were rejected: no client can use the extra precision.

**Idempotency keys** are deterministic so that replaying a source yields the same key; the store rejects duplicates (`UNIQUE (stream_id, idempotency_key)`):

| Source | Key |
|---|---|
| Hook payload | `hook:<emit_id>`; `emit_id` is a UUID the emitter generates once per invocation (`X-Chaldea-Emit-Id`) and reuses for its own retries. A CLI that runs a hook again starts a new emitter with a new `emit_id`, and that delivery is a second event. Content is never deduplicated: two identical prompts are both real. |
| Transcript line | `transcript:<generation>:<offset>` of the line start |
| Process lifecycle | `launch:<launch_id>:<started\|exited>` |
| Command | `command:<command_id>:<n>` for the n-th event of the command |
| Derived event | `derived:<cause key>:<name>` |
| Extension `POST /events` | `ext:<caller key>`, required from the caller |

**Event kinds** (public):

| Kind | Stream | Produced by / payload |
|---|---|---|
| `thread_registered` / `thread_closed` | host | spawn / `chaldea close` (§2.11); agent, cwd, parent, role, initial task |
| `launch_started` | thread | `{ launch_id, pid, start_time, pgid_or_job, agent, cli_version, argv_digest, relaunch_of }` |
| `launch_exited` | thread | `{ launch_id, code, signal, reason: "exit" \| "stopped" \| "spawn_failed" \| "daemon_restart" \| "unknown_after_crash" }` |
| `session_bound` | thread | `{ launch_id, session_id, transcript_path }` (§2.9) |
| `user_message` | thread | `UserPromptSubmit`; `{ prompt: Content, input_id \| null }` |
| `assistant_message` | thread | Claude transcript line; Codex `Stop` payload when it carries the last message, else the Codex transcript when `transcript_path` is non-null (presence and format **unverified**, PR 12 fixtures) |
| `tool_activity` | thread | hooks only: `PreToolUse` (`started`), `PostToolUse` (`completed`), with `tool_use_id` when the payload has one. Transcript tool lines are not turned into `tool_activity`. |
| `status_changed` | thread | an axis or the attention set changed (§2.2); `{ field, from, to, cause }` |
| `approval_requested` / `approval_resolved` | thread | §2.10 |
| `input_queued` / `input_state_changed` | thread | §2.3; `input_queued` carries `{ input_id, launch_id \| null, text: Content, encoding, origin }`, `input_state_changed` carries `{ input_id, from, to, reason, sent_after_seq, by }` (the last two when set) |
| `mailbox_message` | thread (recipient) | `chaldea mailbox send` |
| `task_set` | thread | `{ to, by, summary }` |
| `surface_update` | host or thread | A2UI emit (§2.8) |
| `vt_snapshot_taken` | thread | `{ grid: Content, cols, rows, cursor, modes }` |
| `worktree_changed` | host | §2.6; full record on every change |
| `raw_unknown` | thread if bound, else host | `{ source, reason: "unparseable" \| "unknown_event" \| "foreign_session" \| "unbound" \| "schema_mismatch" \| "partial_line", agent, cli_version, content_type, raw: Content }`; nothing is dropped (arch §1; spike B-11) |

A diff kind (`artifact_diff`, arch §4.2) is not defined in the MVP: the diff view is a reversible layer, and adding a kind later does not bump `schema_version`.

**Store** (internal). SQLite in WAL mode with `synchronous=FULL` and one writer task. `events` holds the envelope columns plus `payload_bytes` and a per-stream running `cum_bytes` (an O(1) replay size). Update and delete triggers abort, `recursive_triggers=ON` makes a conflicting `INSERT OR REPLACE` hit the delete trigger, and the store never issues `REPLACE` or `UPSERT` on `events`. Blobs live in `blobs`, with `blob_refs (stream_id, sha256)` recording which stream references which blob. `command_receipts` and `tokens` are their own tables.

**Projections** (`threads`, `launches`, `inputs`, `approvals`, `worktrees`) change only with the event that changes them, in its transaction, and dropping them and replaying `events` reproduces them. Each projected column comes from a payload:

| Projected column | Event payload |
|---|---|
| thread agent, cwd, parent, role, closed | `thread_registered`, `thread_closed` |
| axes, attention set | `status_changed` (plus `task_set`) |
| current launch, pid, start time, exit reason | `launch_started`, `launch_exited` |
| `session_id`, `transcript_path` | `session_bound` |
| input text, encoding, launch, state, `sent_after_seq`, attribution | `input_queued`, `input_state_changed`, `user_message.input_id` |
| approval card state, correlation, outcome (incl. expired and closed) | `approval_requested`, `approval_resolved` |
| worktree record and owner | `worktree_changed` |

`transcript_cursors` is an **internal checkpoint**, excluded from rebuild, because a cursor can advance past lines that produce no event. Its recovery rule: the cursor of a thread is the `source.transcript` of its last transcript event; rereading from there is safe because transcript idempotency keys drop the duplicates. Tokens and receipts are not projections.

**Transcript cursor rules** (internal, every tailer follows them). A path is accepted only from a hook bound to the thread's current session (`session_bound`); never from directory guessing (arch §4.2). `file_id` (device + inode on Unix, volume serial + file index on Windows) comes from the same open handle the tailer reads. `offset` is just after the last complete line. The generation increments when the path or `file_id` changes or the file is shorter than `offset`; a new generation restarts at 0. A batch of events and the cursor advance commit in one transaction that first checks the stored `(generation, file_id, offset)` still matches; a mismatch aborts the batch. A trailing partial line after `launch_exited` becomes `raw_unknown` (`partial_line`). `transcript_path: null` (Codex, arch §5) creates no cursor. Alternatives rejected: a cursor without generation (a replaced file mixes histories), a line-count cursor (rescans, breaks on partial lines), a last-line hash (identical lines repeat).

**Public vs internal.** Public: envelope, kinds, payloads, `Content`, `GenericEvent`, `schema_version` rules, `X-Chaldea-Emit-Id`, the `ext:` key format. Internal: SQL schema, projections, `global_seq`, blob layout, other key formats.

### 2.2 Thread State: Three Axes and Attention

Three independent axes (arch §2, §4.2) plus an attention set. Every change has an explicit, typed cause; the reducer's input type has no variant built from PTY output, so screen text cannot drive state (invariant 1). VT snapshots and DECSET 2004 are read only to gate input and to show a human what the CLI shows.

| Axis | States | Driven by |
|---|---|---|
| `process_state` | `starting`, `running`, `stopping`, `exited` | launch intent persisted (§2.9) or relaunch; `launch_started`; `stop` accepted; `launch_exited` |
| `turn_state` | `unknown`, `idle`, `running`, `awaiting_approval` | `unknown` before a launch's first signal and after `exited`; `idle` on Claude `SessionStart` and on `Stop`; `running` on a bound `UserPromptSubmit` and when the last open approval closes; `awaiting_approval` on `approval_requested` |
| `task_state` | `none`, `active`, `blocked`, `completed`, `failed`, `cancelled` | set at registration, then only by `task_set` (thread itself, its parent, or a human; §8 Q2) |

**Attention** is a set of causes, separate from `turn_state`, so clearing a cause never has to guess the turn. Each cause has its own clearing event:

| Cause | Raised on | Cleared on |
|---|---|---|
| `session_start_timeout` | no `SessionStart` 20 s after `launch_started` (spike §4.7) | `session_bound`, or `launch_exited` |
| `startup_stall` | an expected Codex `Hooks need review` after a CLI upgrade (arch §5) | `session_bound`, or `launch_exited` |
| `input_unconfirmed(input_id)` | that input entered `unconfirmed` | that input left `unconfirmed` |
| `input_gate_timeout(input_id)` | that input `queued` for 60 s | that input left `queued` |
| `resume_blocked` | a resume refused because the old launch's death is not confirmed (§2.9) | the confirmation |
| `extension(hook_id)` | an `attention` extension hook | `attention_cleared` for that cause |

Every raise and clear is a `status_changed` event; timers are explicit inputs (`Tick(now)` fed by the daemon). A **relaunch** sets `process` to `starting` and `turn` to `unknown`, keeps `task`, and emits no `thread_registered`. Process exit and relaunch never change `task_state`; an orchestrator sees "exited, task still active" and decides (invariant 2).

| Option | Precedent | Verdict |
|---|---|---|
| One status enum | herdr `AgentState { Idle, Working, Blocked, Unknown }` from screen detection (`src/detect/mod.rs:11-20`) | Rejected: mixes the axes and is fed by screen scraping. |
| Thread status + turn status | Codex app-server `ThreadStatus { NotLoaded, Idle, SystemError, Active { active_flags: [WaitingOnApproval, WaitingOnUserInput] } }` (`src/protocol/v2/thread.rs:1810-1827`), `TurnStatus { Completed, Interrupted, Failed, InProgress }` (`v2/turn.rs:34-39`) | Partly adopted: its flags map to `awaiting_approval` and attention. No task axis, because it has no orchestration outcome. |
| Three axes + attention set | arch §2 | **Adopted.** |

Public: field names, state and cause values, `status_changed` / `task_set` payloads. Internal: reducer, timer defaults.

### 2.3 Input State Machine

An input is one prompt sent through `chaldea send`, the PWA, or `surface_action`.

```
queued ──(gate open; attempt persisted)──▶ sending ──(text + submit written)──▶ sent ──(attributed ack)──▶ acked
  │                                          └───────(write outcome unknown, daemon restart, launch_exited)────┐   │
  │                                                                     (window expired, ack not attributable)─┴─▶ unconfirmed
  ├─(validation failed, zero bytes)──▶ rejected                                  late attributed ack ──▶ acked ◀─┤
  └─(cancel, launch exited, daemon restart; zero bytes)──▶ cancelled             input resolve ──▶ acked (by human) | dismissed
```

- **Gate** (`queued` → `sending`): the current launch is bound; `turn_state = idle`; no other input of the **current launch** is `sending`, `sent`, or `unconfirmed` (inputs of an earlier launch never block); the attach block is clear; for bracketed paste, `TermMode::BRACKETED_PASTE` is set (prior-art row 4a). Attention causes not tied to an input do not close the gate. After 60 s in `queued` the input raises `input_gate_timeout` with a `vt_snapshot_taken` and stays `queued` with zero bytes written.
- **`sending`**: the transaction that records `sending` and `sent_after_seq` (the thread stream's head) commits before the bytes go to the PTY writer.
- **Ack**: the first `UserPromptSubmit` of the same launch with `seq > sent_after_seq`, attributable only if the PTY writer recorded no other write (attach bytes, a `key`) between the start of this input's write and that event. Position, not wall time, decides. An ack event that arrives while the input is still `sending` is kept as its candidate and evaluated when `sent` is recorded. `fidelity: ok | differs` against the adapter's expected form is recorded and never changes the ack (spike §4.4 oracle split).
- **`unconfirmed`**: the 30 s window (spike §4.7) expired, the candidate is not attributable (the event stays a `user_message` without `input_id`), the write outcome is unknown, the daemon restarted, or the launch exited while `sending`/`sent`. **Never resent** (arch §4.3). `chaldea input resolve <id> --delivered | --dismiss` moves it to `acked` (`by: human`) or `dismissed`, keeps the history, and clears `input_unconfirmed`.
- **`rejected`** (ESC or C0 other than TAB/CR/LF, prior-art row 4b; Codex-on-Windows lossy input or empty prompt, spike F-1d, §8 Q6) and **`cancelled`** (a human cancel, the launch exited, or a daemon restart while `queued`) are terminal with zero bytes written.
- **Codex positional first prompt**: the input is created directly in `sent` at `launch_started` (`sent_after_seq` = that event's seq), without passing `queued`. Its ack window is suspended while `session_start_timeout` or `startup_stall` is active.

**Writer arbitration.** One writer per PTY (in `pty-runtime`) serializes attach bytes, `key` writes, and input writes, and records the stream position and origin of each. The attach block is set by any attach input write and cleared on `Stop`, on `SessionStart`, and on detach while `turn_state` is not `running`; while it is set, programmatic inputs stay `queued`.

**Ack per adapter.** Claude: `UserPromptSubmit` of the launch bound by `--session-id`; its `prompt` is Claude's rendering (TAB → 4 spaces, NFC, `<pasted_content>` wrapper; arch §5). Codex: `UserPromptSubmit` of the launch bound by §2.9; input only after the previous turn's `Stop` (arch §5).

Alternatives: auto-resend (rejected by arch §4.3; an extra Return can accept a popup, arch §5); ack by screen echo (invariant 1); ack by time window (a human's attach input inside it is indistinguishable); an SDK protocol with turn acks (T3 Code; invariant 1).

Public: state names, payloads, `send`, `input resolve`. Internal: window and timeout defaults. The states beyond the architecture's four are §8 Q3.

### 2.4 Sync API and Commands

**Replay.** `GET /api/events?stream_id=<id>&after_seq=<N>&limit=<L>[&epoch=<E>]` (arch §4.1). `limit` 1–1000, default 200 (else 400). `epoch` is a random id stored at database creation; the client omits it on its first fetch and sends the last one it saw afterwards. Response (public `EventsResponse`):

```json
{ "mode": "replay", "stream_id": "thread:…", "epoch": "…", "events": [ … ], "head_seq": 812, "has_more": false }
{ "mode": "snapshot", "stream_id": "thread:…", "epoch": "…", "snapshot": { … }, "as_of_seq": 812, "reason": "epoch_changed" }
```

Snapshot fallback when the given `epoch` differs from the server's, `after_seq > head_seq`, or the replay would exceed 10 000 events or 8 MiB (from `cum_bytes`). The decision is one pure function, `decide_replay`, as T3 Code's `decideThreadResume` (`apps/server/src/orchestration-v2/ThreadStream.ts:99-118`, bounds at `:18-20`). The client also checks the response's `epoch` against the one it holds and, on a difference, drops its local copy and starts from a snapshot. Snapshot and `as_of_seq` come from one read transaction. A thread snapshot is its projection plus the last 50 events; a host snapshot is the thread list. A stream that does not exist or that the token may not read is 404; an empty stream returns `replay` with `events: []`, `head_seq: 0`.

**Backward history.** `GET /api/events?stream_id=<id>&before_seq=<N>&limit=<L>` returns up to `L` events with `seq < N`, ascending, with `has_more_before` (in the response type). `after_seq` together with `before_seq` is 400.

**Live notifications.** `POST /api/ws-ticket` (Bearer) returns a ticket; `GET /api/ws?ticket=<t>` upgrades. Server → client: `{ "type": "hello", "epoch", "server_time" }`, `{ "type": "stream_advanced", "stream_id", "head_seq" }`, `{ "type": "heads", "heads": [{ "stream_id", "head_seq" }] }`; client → server: `{ "type": "subscribe", "stream_ids": [...] }`. Notify-then-pull: the client fetches with `GET /api/events`. The server sends current heads on every `subscribe` and a `heads` message every 30 s, so a lost notification on a live connection is recovered within 30 s, and the client refetches on every reconnect. Inline event push is deferred; it can be added as an optional field.

**Commands** (public). Every mutating request carries a client-generated `command_id`: `spawn`, `resume`, `send`, `key`, `stop`, `close`, `approval decide`, `attention clear`, `input resolve`, `task set`, `mailbox send`, `worktree remove`, `surface_action`. Receipts follow T3 Code's `orchestration_v2_command_receipts` keyed by `command_id` (`Migrations/055_OrchestrationV2.ts:42-50`), with:

- **Insert.** The receipt is inserted `in_progress` in the transaction that records the command's intent; the primary key makes a concurrent duplicate fail its insert, so only one request can act.
- **Completion.** `send` completes when `input_queued` commits and returns `input_id`; delivery is then the input's state, not the receipt's. Synchronous side-effect commands (`key`, `spawn`, `resume`, `stop`, `worktree remove`) stay `in_progress` until the effect finished or failed.
- **`ReceiptLookup`** (core): `New`; `InProgress` (409 with `retry_after`); `Replay(result)` (same token and request hash); `Conflict` (other hash or other token; 409 revealing nothing); `UnknownAfterCrash`.
- **Restart.** Every `in_progress` receipt becomes `unknown_after_crash` and is never re-executed. Inputs are recovered from the `inputs` table, independent of receipts: `queued` → `cancelled` (zero bytes), `sending`/`sent` → `unconfirmed` (unknown). Launches and worktrees follow §2.9 and §2.6.

**Offline.** When the daemon is unreachable, the PWA keeps the last state, locks input, and shows the time of the last successful sync (arch §4.1).

`stream_id`, `epoch`, and `command_id` are public now so the Phase 2 relay can cache per stream and forward commands unchanged. A `stateright` model (PR 17) checks startup crash, offline buffering, and retry after a lost response (arch §6).

### 2.5 HTTP Surface and Authorization

The daemon listens on `127.0.0.1:<port>` only, on `axum` + `hyper` + `tower-http` (arch §3), so request parsing is hyper's (prior-art row 5a's reason). Token comparison uses `subtle` (prior-art row 5d); tokens are stored as SHA-256 hashes (`sha2`, row 5c).

**Common checks.** `Host`: one header, `127.0.0.1:<port>` or `localhost:<port>`, else 421 (DNS rebinding; spike B-8). `Origin`, where checked: absent, or exactly `http://` + the received `Host`, else 403. Bearer: one `Authorization: Bearer` header, else 401. A 5 s whole-request deadline applies to `/hook` and to short non-streaming routes; WebSockets, attach, and `spawn` (which may wait for `before_spawn`) have their own bounds.

**Routes** (public):

| Route | Origin | Credential | Object check |
|---|---|---|---|
| `GET /`, `/assets/*` (PWA) | — | none | serves no data; CSP `default-src 'self'`, `connect-src` own origins |
| `POST /api/pair` | present and matching | single-use pair code | issues an `owner` token |
| `POST /api/ws-ticket` | absent or matching | Bearer | ticket bound to the token and a purpose (`events` or `attach:<thread>`) |
| `GET /api/ws` | absent or matching | ticket | subscriptions limited to readable streams |
| `GET /api/attach/<thread>` (WS) | absent or matching | ticket `attach:<thread>` | owner only |
| `GET /api/events`, `/api/blobs/<sha256>?stream_id=` | absent or matching | Bearer | readable stream; for blobs also `(stream_id, sha256)` in `blob_refs` |
| `GET /api/threads`, `/api/worktrees` | absent or matching | Bearer | filtered to threads the token can read and worktrees those threads own |
| `GET /api/adapters` | absent or matching | Bearer | any owner, agent, or rescue token |
| `POST /api/commands/<name>` | absent or matching | Bearer | action matrix; receipt owned by the token |
| `POST /hook` | must be absent | `hook` token | its own launch |
| `POST /events` | must be absent | `owner`, `rescue`, or `extension` token | owner: any scope; rescue: any scope; extension: its configured scope; kinds `surface_update`, `raw_unknown` |

Tickets are single use, valid 30 s, and die with their token. Blob responses use the stored type only if it is `text/plain; charset=utf-8` or `application/json`, else `application/octet-stream`, with `X-Content-Type-Options: nosniff`, `Content-Disposition: attachment`, `Cache-Control: no-store`.

**Token kinds** (public names and env vars):

| Kind | Holder | Lifetime |
|---|---|---|
| `owner` | the user's CLI (token file, 0600 / user-only ACL) and the paired PWA | until revoked |
| `agent` | a thread's CLI tree, `CHALDEA_AGENT_TOKEN` | one launch |
| `rescue` | a thread the owner spawns with `--role rescue`, `CHALDEA_RESCUE_TOKEN`; it gets no agent token | until revoked |
| `hook` | the launch's emitter, `CHALDEA_HOOK_TOKEN` (with `CHALDEA_HOOK_URL`, `CHALDEA_THREAD_ID`) | one launch |
| `extension` | one one-shot extension hook run, `CHALDEA_EXTENSION_TOKEN` | one run |

All processes also get `CHALDEA_URL`; inherited `CHALDEA_*` variables are scrubbed first (spike §4.3).

**Actions** ("self+" = the thread itself and its descendants):

| Action | owner | agent | rescue | extension |
|---|---|---|---|---|
| read stream / blob / thread list | all | self+ | all | — |
| spawn | ✓ (incl. `--role rescue`) | children of itself, never `--role rescue` | — | — |
| send, stop, resume, close, task set | ✓ | self+ | — | — |
| key, input resolve | ✓ | — | ✓ | — |
| attach, approval decide, attention clear | ✓ | — | — | — |
| mailbox send | ✓ | to its parent and children | — | — |
| worktree remove | ✓ (`--force` allowed) | worktrees it owns, no `--force` | — | — |
| `POST /events` | any scope | — | any scope | its scope |
| `surface_action` | ✓, when the surface's scope contains the target (Thread: that thread; Workspace: a thread whose workspace is that one; Host: any) | — | — | — |
| doctor results | ✓ | — | ✓ | — |

A receipt is visible only to the token that created it. Hook binding also requires the emitter ancestry rule (§2.9; prior-art row 6c).

**Secrets.** Logs redact `Authorization`, the `ticket` parameter, and pair codes, and never contain `/hook` or `/api/pair` bodies. `chaldea open` opens `http://127.0.0.1:<port>/#pair=<code>` (single use, 60 s); the fragment never reaches the server, the page exchanges it via `POST /api/pair`, stores the `owner` token in IndexedDB, and removes the fragment with `history.replaceState`. Rejected: an `HttpOnly` cookie (needs CSRF defences on every route; invariant 5 names Bearer). T3 Code uses the same shape: scoped sessions, single-use pairing links (`Migrations/031_AuthAuthorizationScopes.ts:4`, `:12-48`), `timingSafeEqual` (`apps/server/src/auth/utils.ts:83-89`). herdr's socket API has no token (`src/ipc.rs:10-11`); rejected because Chaldea's API is HTTP. CF Access and host Service Tokens (arch §2) are Phase 2; the token enum leaves room.

**Rejection expectations (spike B-8 on hyper).** Decisions are made in the order Host → Origin → Bearer → scope → framing → size, before any body byte is used. Expected statuses follow RFC 9112: an invalid `Content-Length` such as `+5` is 400 (RFC 9112 §6.3 requires 400; the spike's 411 is replaced), a missing `Content-Length` on POST is 411, a body over 4 MiB is 413, an expired request deadline is 408, and a slow head is a connection closed within its budget, observed by the client as EOF; a client timeout or `status: 0` is never taken as that observation. No rejection creates an event, fixture, or readiness change.

### 2.6 Worktree Ownership

`chaldea spawn --worktree [--base <ref>]` creates a git worktree with the git CLI (as herdr and T3 Code do), through `process-wrap` with `CREATE_NO_WINDOW` on Windows (prior-art rows 1c, 9). Metadata (public, in `worktree_changed` and `GET /api/worktrees`): `worktree_id` (8 hex, also the directory name), `repo`, `path`, `branch` (`chaldea/<id>` unless given), `base_ref`, `base_commit` (resolved SHA), `owner_thread_id`, `created_by` (`human` or `thread:<id>`), `state` (`creating`, `active`, `released`, `removing`, `removed`, `failed`).

- **Short base path.** `worktree_base` is configurable per workspace; default `<state_dir>/wt`. `chaldea doctor` warns on Windows when it exceeds 40 characters, because MSVC `link.exe` fails beyond 260 (arch §5).
- **Ownership.** On `thread_closed`, ownership passes to the parent thread, or to the human owner when there is none.
- **Crash safety.** The `creating` / `removing` intent commits before git runs; restart reconciles against git and never deletes anything.
- **Never auto-delete.** No exit, close, restart, or timer removes a worktree. `chaldea worktree remove <id>` is refused unless: no tracked or untracked changes; no ignored files unless `--discard-ignored` (owner only, §8 Q7); no commit reachable from the worktree's `HEAD` (detached included) or its registered branch that is not on a remote-tracking ref (`git rev-list --max-count=1 HEAD <branch> --not --remotes`, the check T3 Code runs before forced removal, `apps/server/src/storageCleanup.ts:1004-1022`); no live launch uses it. A git error refuses and changes nothing. `--force` (owner only) skips the content checks but never the in-use check, and is recorded. herdr relies on git's dirty refusal (`src/worktree.rs:194-215`), which misses unpushed commits.
- **Branches are never deleted** (herdr does the same, `src/worktree.rs:175-192`); an explicit opt-in is a later, reversible addition.

### 2.7 Adapter Capability Table

A const table in `crates/core`, keyed by `(agent, os)`; no `cfg` decides it (spike F-1f). `GET /api/adapters` exposes a read-only summary (public type).

| Capability | Claude Code (macOS, Windows) | Codex CLI macOS | Codex CLI Windows | Evidence |
|---|---|---|---|---|
| Hook injection | `--settings <run>/claude-settings.json` + `--session-id <uuid>`; global settings untouched | `-c hooks.<Event>=[…]`, fixed command strings and order, timeouts fixed from the first release; per-session values in env | same | arch §5; prior-art rows 6a, 6b |
| Hook events | `SessionStart`, `UserPromptSubmit`, `Stop`, `SessionEnd`, `PermissionRequest`, `PreToolUse`, `PostToolUse`, `Notification` | `SessionStart`, `UserPromptSubmit`, `Stop`, `PermissionRequest`, `PreToolUse`, `PostToolUse` | same | spike §3 (names in binaries); payloads beyond the spike's four **unverified** until PR 12 |
| Session binding | predetermined `--session-id` | §2.9 | same | spike §4.3 |
| `session_start` | `eager` | `first_turn` | `first_turn` | arch §5 |
| First prompt | PTY input after `SessionStart` | positional argument (§8 Q5) | same | arch §5 |
| Input encoding | bracketed paste only while DECSET 2004 is set; newline `cr` | bracketed paste; `cr` | `win_keys` | arch §5; prior-art rows 4a, 4d; spike A-6, F-1f |
| Submit | one CR 300 ms after the text | same | one Return after 300 ms, ready composer only | prior-art row 4e; arch §5 |
| Extra flags | — | `--no-daemon` | `--no-daemon`, `-c tui.disable_paste_burst=true` | arch §5 |
| Resume | `--resume <session_id>` (**unverified**) | `codex resume <session_id>` (**unverified**) | same | PR 12 fixtures |
| Transcript | JSONL from the bound `transcript_path` | hooks only when null; tailed for assistant text when `Stop` lacks it (format **unverified**) | same | arch §4.2, §5 |
| Hook trust | n/a | record CLI version with trust state; `Hooks need review` after an upgrade is `startup_stall` | same | arch §5; prior-art row 6b |
| Startup landmines | trust prompt defaults to "No, exit": never send Enter blindly | update, folder trust, hook review; digits pick options | same | arch §5 |
| Env scrub | `CLAUDECODE`, `CLAUDE_CODE_*`, `CODEX_THREAD_ID`, `CODEX_SESSION_ID`, other `CODEX_*` session variables, inherited `CHALDEA_*` | same | same | prior-art row 6c |

**`win_keys`** (Codex on Windows, first MVP test, arch §5): reject ESC/C0 other than TAB/CR/LF, trimmable leading/trailing whitespace, TAB, and empty prompts (§8 Q6); NFC-compose each grapheme cluster holding a combining or extending character, keeping CJK compatibility ideographs; one newline per CRLF/CR/LF; one leading and one trailing space; ASCII as UTF-8, **every other BMP scalar** as a win32-input-mode key-down + key-up record (`ESC[0;0;<utf16>;1;0;1_` + `ESC[0;0;<utf16>;0;0;1_`), astral scalars as UTF-8; each newline as a Shift+Enter record pair; every escape sequence in one write; one Return after 300 ms. The same rule applies to `send` and to pastes the attach client recognises (PR 11). The spike sent records only for Mn/Me/Cf; the encoding widens that because U+2764 sent as UTF-8 was lost (arch §5). herdr instead keeps paste-burst detection on and appends a Right key so Codex flushes the burst before Enter (`src/app/api/agents.rs:14-33`, 300 ms at `:13`); it addresses the submit race but not lost BMP characters, so it is the fallback if PR 15 shows `tui.disable_paste_burst=true` is not enough.

### 2.8 Extension & Rescue Surface (Minimal)

- One-shot hooks `before_spawn`, `after_close`, `attention` get one JSON document on stdin and `CHALDEA_URL`, `CHALDEA_EXTENSION_TOKEN`, `CHALDEA_THREAD_ID`; 10 s timeout; only `before_spawn`'s `{ "decision": "deny", "reason" }` is read. Failure policy: §8 Q4.
- A2UI surfaces: `{ surface_id, scope: { kind: "host" } | { kind: "workspace", workspace_id } | { kind: "thread", thread_id }, components }` with `text`, `markdown`, `vt_snapshot`, `button`, `text_input`, `list`. A `button` carries `surface_action { action_id, kind: "send_input" | "send_key" | "stop" | "clear_attention" | "open_attach", args }`, run only when a human with an owner token clicks it, through the command API.

### 2.9 Launch Contract

- **Intent before spawn.** One transaction persists the launch (`launch_id`, thread, agent, argv digest, hashes of the launch's hook and agent tokens) and the receipt; only then is `PtyRuntime::spawn` called. `launch_started` records pid, start time, and pgid or job.
- **Early hooks.** A hook with a valid token whose launch has no recorded pid waits, bounded and inside the request deadline, for the pid; then the ancestry rule runs; past the bound it is `raw_unknown` (`unbound`).
- **Binding.** Claude binds by its `--session-id`. Codex hook processes run concurrently, so `UserPromptSubmit` can arrive before `SessionStart`: authenticated hooks of an unbound launch are buffered (bounded count, size, and time); the first `SessionStart` whose emitter's nearest CLI ancestor is the launched pid binds it, buffered events with that `session_id` are appended in source order, the rest become `raw_unknown` (`foreign_session`), and an overflow or timeout turns the buffer into `raw_unknown` (`unbound`). Rejected: binding from the first hook of any type, because only the `SessionStart` rule was verified (spike §4.3, B-11) and a common `session_id` on every Codex payload is not.
- **Daemon restart.** Before accepting commands, the daemon records `launch_exited` (`daemon_restart`, or `unknown_after_crash` without a recorded pid) for every open launch and revokes its tokens. A **resume is refused**, with `resume_blocked` raised, until the old launch's death is confirmed. Reason: a resumed CLI and a surviving old one would write the same session; refusing is safe and can be relaxed later.
- **Resume** (`chaldea resume <thread>`) is an explicit relaunch: a new `launch_id` and tokens, the bound `session_id` passed to the adapter's resume flag, and the transcript cursor carried over. For Codex, which fires no hook until a turn runs (arch §5), the resume passes a prompt when the resume command accepts one (**unverified**, PR 12); otherwise the launch stays unbound with `session_start_timeout`, and the human recovers through attach, whose first turn binds it.

### 2.10 Approval Cards

An approval card has an `approval_id` and a correlation key. Claude's `PermissionRequest` input has `tool_name` and `tool_input` "like PreToolUse hooks, but without `tool_use_id`" (Claude hooks reference, `PermissionRequest input`). The key is therefore the `tool_use_id` of the launch's latest `PreToolUse` with equal `tool_name` and `tool_input` before the request, when exactly one matches (that ordering is an inference, **unverified** until PR 12 fixtures); otherwise the card is uncorrelated. Codex's `PermissionRequest` payload is **unverified**.

- **Open:** `approval_requested`; `turn_state` → `awaiting_approval`.
- **Resolved** only on evidence correlated to that card: the decision returned by the hook (§8 Q1 option A), or a `PostToolUse` with the card's `tool_use_id` (`outcome: ran`).
- **Closed** on `Stop` (`outcome: unknown`; turn → `idle`) or `launch_exited` (`outcome: expired`), for every card still open.
- Actions from a resolved or closed card are rejected (409). An uncorrelated card only closes.

### 2.11 Thread Close

`chaldea close <thread> [--stop]` (owner, or an agent for self+) closes a thread whose process is `exited` (with `--stop`, it stops it first). It emits `thread_closed`, runs `after_close`, passes worktree ownership on (§2.6), and revokes the thread's tokens. Closed threads stay readable.

## 3. Workspace Layout

```
Cargo.toml, rust-toolchain.toml (1.96.0)    # workspace, shared lints, exact-pinned deps (spike policy)
crates/core/        # Sans-IO: types, envelope, reducers, input machine, encoders, capability table, replay decision,
                    # receipt lookup, auth policy, verdict model. No tokio, no std::fs/net/process, no clock.
crates/pty-runtime/ # PtyRuntime trait, FakePty, unix (portable-pty =0.8.1), windows (own windows-sys ConPTY),
                    # single writer, alacritty_terminal VT responder and snapshot, process-tree kill, drain
crates/daemon/      # tokio + axum + rusqlite: store, HTTP, hook ingest, transcript tailer, supervisor, git
crates/cli/         # binary `chaldea`: client subcommands, `daemon run`, `attach`, `doctor`, `hook-emit`
crates/testkit/     # publish = false: `chaldea-fake-cli` and fixture helpers (spike FakeCli modes)
packages/contracts/ # generated TS (ts-rs) + JSON Schema (schemars) + validators
packages/web/       # React + TypeScript strict + Vite + Tailwind PWA, served by the daemon
fixtures/           # real-CLI corpus: fixtures/<agent>/<cli-version>/<case>/
fuzz/               # cargo-fuzz targets (nightly, outside the stable workspace)
spikes/pty-hooks/   # unchanged until issue #1 removes it
```

**Dependency direction.** `core` ← `pty-runtime` ← `daemon` ← `cli`; `testkit` depends on `core`. `core` depends on `serde`, `serde_json` (`raw_value`), `ts-rs`, `schemars`, `uuid`, `unicode-normalization`, `unicode-segmentation`, `subtle`, `sha2`. Sans-IO is enforced by `crates/core/clippy.toml` (disallowing `std::fs`, `std::net`, `std::process`, `Instant::now`, `SystemTime::now`, `std::thread::spawn`) and a CI check that `core` has no `tokio`.

**Single binary.** `chaldea hook-emit` dispatches before any runtime or config initialization and uses std networking only (arch §2). Console subsystem on Windows; no GUI-subsystem variant (prior-art row 9, spike C-5 `go`).

| Option | Precedent | Verdict |
|---|---|---|
| One crate with modules | herdr, `src/platform/{unix_common,macos,windows}.rs` | Rejected: Sans-IO and the Windows CI gate are easier at crate boundaries. |
| Protocol crate + server crate | Codex `codex-rs/app-server-protocol` owning the generated schema | **Adopted** in spirit: `core` owns every public type. |
| Separate store crate | — | Rejected for the MVP: one user. |

**Spike modules.** Copied, not moved; `spikes/` and its CI job stay until issue #1.

| Spike module | Destination | Decision |
|---|---|---|
| `src/os/drain.rs`, `src/vt.rs` | `pty-runtime` | Lift. |
| `src/os/unix.rs` | `pty-runtime::unix` | Lift kill and enumeration; drop console and probe helpers. |
| `src/os/windows.rs` | `pty-runtime::windows`; parent job check in `daemon` | Lift the backend, handle wrappers, teardown order; drop the window observer, calibration, keys probe, registry reads. |
| `src/pty.rs` | `pty-runtime` | Rewrite behind `PtyRuntime`, keeping single-writer ordering and the revocable gate. |
| `src/paste.rs`, `src/marks.rs` | `core::input` | Lift the pure encoders, widen `unit_records` to every non-ASCII BMP scalar; rewrite the writer-driving parts in the daemon. |
| `src/receiver.rs` | `daemon` | Rewrite on axum; keep the binding rules and B-8 cases. |
| `src/emitter.rs`, `src/http_client.rs` | `cli` | Lift; add `X-Chaldea-Emit-Id` and one retry. |
| `src/session.rs` | `daemon` | Rewrite; lift `scrub_env_name` and the kill ordering. |
| `src/verdict.rs` | `core::verdict` | Lift the outcome model; change the duplicate and plan rules (§7). |
| `src/fake_cli.rs` | `testkit` | Lift `echo`, `queries`, `stall`, `banner`, `attachable`, `spawn-tree`, `sleeper`. |
| `src/scenario/*`, `report.rs`, `cli.rs` | — | Not lifted. |

**Tooling.** CI adds a `workspace` job (macOS + Windows): `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`, the contract drift check, the `core` dependency check; `lefthook.yml` mirrors it. Node and pnpm join `mise.toml` with PR 18; the JS job runs typecheck, Vitest with fast-check, and Playwright.

## 4. PR Plan

Each PR keeps `core` + `FakePty` tests green on macOS and Windows CI; macOS first within each PR, its Windows code in the same PR.

| PR | Scope | Depends on | Milestone |
|---|---|---|---|
| 1 | Workspace, CI, `core` contracts (§5) | — | Ingest, Sync (types); Verification (PBT) |
| 2 | `PtyRuntime` trait, `FakePty`, single writer with origin log, drain, VT responder + snapshot | 1 | PTY |
| 3 | Unix backend (portable-pty `=0.8.1`, `killpg` HUP→TERM→KILL, `getsid` scan, recorded-pid fallback); `testkit` | 2 | PTY (macOS) |
| 4 | Windows backend (flags 0, `kernel32.dll` only, JOB_LIST + `CREATE_SUSPENDED` fallback, job + recorded-descendant kill) | 2, 3 | **closes PTY** |
| 5 | `core::input` encoders, capability table | 1 | Agent Control |
| 6 | Store: append-only events, projections, cursor checkpoint, receipts, blobs | 1 | Ingest |
| 7 | HTTP: route and action matrices, tokens, pairing, sync API, blobs, WS, receipts | 6 | Sync, Extension |
| 8 | Daemon lifecycle (`daemon run/start/stop`, `DETACHED_PROCESS`), parent kill-on-close job check | 7 | Agent Control |
| 9 | `POST /hook`, binding, `raw_unknown`, `chaldea hook-emit`, B-8 cases (**Windows hook receiver** open item) | 7, 3 | Ingest |
| 10 | Supervisor: launch contract, `spawn/stop/wait/resume/close`, send path, attention, restart, `GET /api/adapters` | 3, 4, 5, 8, 9 | Agent Control |
| 11 | `chaldea attach` with the attach input contract | 10 | Agent Control |
| 12 | Fixture corpus: capture (`chaldea doctor capture`) with typed verdict, envelope and payload-presence validation | 9, 10 | Verification |
| 13 | Claude adapter: hooks, bracketed paste send, transcript tailer, approvals, resume | 10, 12 | Agent Control; Ingest (tailer) |
| 14 | Codex adapter: flags, positional prompt, `win_keys` for send and attach paste, trust stall (**Codex hook trust renewal**), assistant text, resume | 13, 11 | Agent Control; **closes Ingest** |
| 15 | **First tests 1 and 2** on a real Windows host (**Codex keyed input**, **human paste through attach**) | 14 | Cross-OS E2E |
| 16 | Mailbox, agent tokens, `task set`, worktree ownership and remove | 10 | **closes Agent Control** once 11, 14, 16 are all merged |
| 17 | `stateright` Sync model | 7 | Verification |
| 18 | PWA read path: validators, pairing, thread list/view, paging, `GenericEvent`, client reducer | 7 | Sync & PWA |
| 19 | PWA input: `<textarea>` IME guards, send, input resolve, approvals, VT snapshot, keypad; Playwright | 18, 13 | **closes Sync & PWA** |
| 20 | Extension & Rescue: one-shot hooks, `POST /events`, rescue role, A2UI, `surface_action` | 16, 19 | **closes Extension & Rescue** |
| 21 | `chaldea doctor` | 14, 16 | Cross-OS E2E |
| 22 | `cargo-fuzz` targets + scheduled CI | 5, 9, 13 | **closes Verification** (with 12, 17) |
| 23 | Real-host run: §6.1 on macOS and Windows; more Windows builds where available (**Windows build coverage**) | all | **closes Cross-OS CI & E2E**; end of the MVP |

**Longest paths** (10 PRs each, all ending at PR 23). Two equal prefixes reach PR 10: 1 → 6 → 7 → 9 → 10 and 1 → 2 → 3 → 4 → 10. From PR 10, three equal tails: 10 → 12 → 13 → 14 → 15 → 23, 10 → 12 → 13 → 14 → 21 → 23, and 10 → 12 → 13 → 19 → 20 → 23. The Windows backend (PR 4) is on a longest path, so the supervisor is written only after both backends exist.

**Why the first tests run in PR 15.** Both are end to end (arch §5, issue #2) and need the Windows backend, the encoder, hook ingest, attach, and the Codex launch path. A failure changes only the encoder, the capability table, and the attach paste path, not a contract. PR 5 tests the encoder on both OSes from the start and PR 14 runs FakeCli attach paste tests on `windows-latest`.

**Open items of arch §5.** Codex keyed input: PR 15 (unit tests in 5). Human paste through attach: PR 15 (contract in 11, FakeCli in 14). Codex hook trust renewal: PR 14, 21. Windows hook receiver: PR 9, 23, with the expectations of §2.5. Daemon parent job check: PR 8 (prior-art row 8). Windows build coverage: PR 23; real-CLI runs on more builds need more hosts and stay open until then. prior-art "Not Verified" items that affect behaviour (inbox conhost on other builds, BMP characters delivered only on Alt release) are covered by PR 15 and 23; the rest do not affect the MVP and stay unverified.

**Design obligations.** One line per scenario that the PR's own design and tests must cover; the PR's document decides how.

- **PR 2:** writer records origin and position of every write; replies of the VT responder in one write each; wide characters on two cells in snapshots.
- **PR 3 / 4:** FakeCli `spawn-tree` survivors and the unrelated negative control; Windows injected setup failures leave no process or handle (spike F-2c); JOB_LIST branch recorded.
- **PR 5:** paste encoder writes zero bytes on rejected input and never splits an escape sequence (spike F-3d); `win_keys` over the spike's corpus F; encoding chosen by `(agent, os)`, never by DECSET 2004.
- **PR 6:** drop-and-rebuild equality for every projection; `INSERT OR REPLACE`, update, and delete on `events` abort; cursor checkpoint recovery from events; stale-cursor batch aborts; blob offload above 64 KiB.
- **PR 7:** `limit` 0 and above max, empty / unknown / unreadable stream, `after_seq = head_seq` and above, append between pages, epoch mismatch in both directions, byte bound, `after_seq` + `before_seq`, `before_seq` at 1, lost notification recovered by `heads`; every route × token kind cell; per-resource filtering; `Origin` vs received `Host`; ticket reuse and expiry; secrets absent from logs.
- **PR 8:** daemon inside a kill-on-close job on Windows CI is detected and handled (breakaway or refusal with a message).
- **PR 9:** B-8 cases with per-case deadline, client observation, and no side effect; response ordering and budgets measured from connection start; 431 vs 400 for oversized heads; duplicate equal `Content-Length`; body held, already sent, and slow; positive controls; same `emit_id` twice → one event, two `emit_id`s → two; nested FakeCli (`echo --nested`) does not bind; buffered early Codex hooks are durable before 200 is returned; later hooks re-validate ancestry; a conflicting second `SessionStart`.
- **PR 10:** restart scan of a launch's session by recorded sid with `launch.start_time ≤ start ≤ restart_now`; the crash window between spawn and saving the pid; recorded descendants that left the session; resume refused until death confirmed; concurrent duplicate commands write once; `sending` across a crash ends `unconfirmed`, `queued` ends `cancelled`; gate timeout with a FakeCli that never sets DECSET 2004; ack arriving while `sending`; an ack after a `key` write is not attributable; removing bind, idle, and DECSET 2004 one at a time keeps the input `queued`; a fake banner never changes state.
- **PR 11:** outer-terminal paste boundaries verified first on both OSes; a paste split across reads; a recognised paste that cannot be framed or encoded is rejected with zero bytes written; terminal modes restored on detach, Ctrl-C, EOF, and panic; attach block set and cleared per §2.3.
- **PR 12:** capture refuses an empty fixture set; envelope and payload presence per event; scrubbing of tokens, home paths, and non-corpus prompts; Claude and Codex `PermissionRequest`, `Stop`, Codex transcript, resume behaviour recorded per CLI version.
- **PR 13 / 14:** real decoders over the fixtures; truncate-then-regrow detection; bind and cursor advance under one update condition; reread tests on real files; approval correlation and closing (§2.10); Codex positional prompt ack with a startup stall; zero submit or queue during an attach paste and exactly one submit after a physical Enter (Codex on Windows, FakeCli).
- **PR 15 / 23:** per-row evidence: P-1 settings diff, P-3 MCP witness attested before stop (spike A-8), P-4 worktree after restart, P-7 settle interval after `Stop` (spike F-1a), P-2 registered vs observed hook events (spike A-5), P-11 real IMEs, P-13 query log, P-14 malformed and nested payloads, P-16 stale card, P-17 per CLI version, P-18 survivor list with the macOS containment limit reported, P-20 probe list, P-23 click-only actions, P-25 a calibrated window observer or owner observation, P-27 viewport screenshots.
- **PR 16:** each refusal case of §2.6; agent `--force` refused; git partial failure (refused before git vs uncertain state after git); reconcile git admin data and the real path; removals serialized per worktree; `--force` keeps the in-use refusal; crash between `git worktree add` and the record.
- **PR 18 / 19:** unknown kind and newer `schema_version` render generically; pair fragment removed from the URL; IME guards (`isComposing`, `keyCode === 229`, Enter after `compositionend`) on desktop and mobile viewports.
- **PR 20:** one-shot hooks get the documented input within the timeout; `POST /events` scope rules; rescue cannot send, stop, attach, or remove worktrees; Workspace-scoped `surface_action` membership check.
- **PR 21:** the ConPTY module check runs after a real PTY spawn; a deliberately broken check reports `fail` or `inconclusive`, never `pass`.

## 5. Detailed Design: PR 1

**Goal.** A Cargo workspace whose `crates/core` holds the contracts of §2.1–§2.4 as types and pure functions, generated TypeScript and JSON Schema committed with a drift check, and a CI job on macOS and Windows.

**Files.**

```
Cargo.toml                                # members = ["crates/core"], resolver = "3", edition 2024, MIT OR Apache-2.0,
                                          # [workspace.lints] unsafe_code = "forbid", clippy pedantic subset
rust-toolchain.toml                       # 1.96.0, rustfmt + clippy
crates/core/Cargo.toml                    # chaldea-core; exact pins: serde, serde_json (raw_value), ts-rs, schemars, uuid; dev: proptest
crates/core/clippy.toml                   # Sans-IO disallowed list (§3)
crates/core/src/{lib,ids,event,payload,state,input,sync,command}.rs
crates/core/tests/contracts_export.rs     # generate TS + JSON Schema to a temp dir, diff with committed files
crates/core/tests/golden/                 # golden JSON per event kind
packages/contracts/src/generated/, packages/contracts/schema/   # committed output; package.json arrives in PR 18
.github/workflows/ci.yml, lefthook.yml    # `workspace` job (macos-latest, windows-latest) and hooks
```

**Types and functions** (names may change in review, shapes may not):

```rust
pub const SCHEMA_VERSION: u32 = 1;
pub enum StreamId { Host, Thread(ThreadId) }                              // "host" / "thread:<uuid>"

pub struct EventEnvelope {
    pub stream_id: StreamId, pub seq: u64, pub event_id: EventId, pub schema_version: u32,
    pub idempotency_key: IdempotencyKey, pub kind: EventKindTag,
    pub thread_id: Option<ThreadId>, pub launch_id: Option<LaunchId>, pub command_id: Option<CommandId>,
    pub source: Source, pub occurred_at: Timestamp, pub recorded_at: Timestamp,
    pub payload: Box<serde_json::value::RawValue>,
}
pub enum TypedEvent { ThreadRegistered(..), ThreadClosed(..), LaunchStarted(..), LaunchExited(..), SessionBound(..),
    UserMessage(..), AssistantMessage(..), ToolActivity(..), StatusChanged(..), ApprovalRequested(..),
    ApprovalResolved(..), InputQueued(..), InputStateChanged(..), MailboxMessage(..), TaskSet(..),
    SurfaceUpdate(..), VtSnapshotTaken(..), WorktreeChanged(..), RawUnknown(..) }
pub struct GenericEvent { pub envelope: EventEnvelope, pub why: GenericReason }  // UnknownKind | NewerSchema | PayloadInvalid
impl TypedEvent { pub fn decode(e: &EventEnvelope) -> Result<TypedEvent, GenericEvent>; }
pub enum Content { Inline { inline: String, encoding: Encoding }, Blob { blob: BlobRef, preview: String } }

pub enum StateInput {                                                     // no variant carries PTY output
    Registered { task: bool }, Relaunched, Spawned, SpawnFailed, SessionBound { eager: bool }, StopRequested,
    Exited { reason: ExitReason }, PromptSubmitted, TurnStopped, ApprovalOpened, ApprovalsAllClosed,
    AttentionRaised(AttentionCause), AttentionCleared(AttentionCause), TaskSet { to: TaskState, by: Actor },
}
pub struct ThreadState { pub process: ProcessState, pub turn: TurnState, pub task: TaskState, pub attention: BTreeSet<AttentionCause> }
pub fn reduce(s: &ThreadState, input: &StateInput) -> Result<(ThreadState, Vec<StatusChanged>), InvalidTransition>;

pub enum InputState { Queued, Sending, Sent, Acked, Unconfirmed, Rejected, Cancelled, Dismissed }
pub enum InputInput { GateOpened { sent_after_seq: u64 }, WriteCompleted, WriteOutcomeUnknown, Rejected(RejectReason),
    Cancelled, LaunchExited, DaemonRestarted, AckAttributed, AckNotAttributable, AckWindowExpired, Resolved { delivered: bool } }
pub fn step(state: InputState, input: &InputInput) -> Result<InputState, InvalidTransition>;

pub fn decide_replay(q: &EventsQuery, head: StreamHead, epoch: &Epoch) -> Result<ReplayPlan, QueryError>;
pub enum ReceiptLookup { New, InProgress, Replay(CommandResult), Conflict, UnknownAfterCrash }
pub fn check_receipt(existing: Option<&CommandReceipt>, token: TokenId, request_sha256: &[u8; 32]) -> ReceiptLookup;
```

**Tests** (macOS and Windows):

- `reduce` proptests: never panics; after `Registered`, `task` changes only on `TaskSet` (not on `Relaunched` or `Exited`); after `Exited`, `turn` is `unknown`; each attention cause is cleared only by its own clearing input; every change appears in exactly one `StatusChanged`.
- `step` proptests: `Sending` is entered at most once; nothing leads from `Sending`, `Sent`, or `Unconfirmed` back to `Queued` or `Sending`; `Rejected` and `Cancelled` only from `Queued`; `DaemonRestarted` and `LaunchExited` map `Sending`/`Sent` to `Unconfirmed` and `Queued` to `Cancelled`; `Dismissed` and `Resolved { delivered: true }` only from `Unconfirmed`.
- `decide_replay` table: each fallback reason, `after_seq = head`, empty stream, absent epoch, `after_seq` with `before_seq`, `limit` bounds.
- `check_receipt`: every `ReceiptLookup` case, including another token with the same hash.
- Decoding: unknown kind, newer `schema_version`, and invalid payload each give `GenericEvent`; golden round trips for every kind, both `Content` forms and encodings, `EventsResponse` both modes, WS messages.
- `contracts_export`: committed TS and JSON Schema equal the generated output; the regenerate command is documented in the test.

**Completion commands.**

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! printf '%s\n' "$deps" | grep -Eq '^(tokio|rusqlite|axum) '
```

plus the `workspace` job green on both runners and the `pty-hooks` job unchanged.

## 6. Purpose Layer and Plan Layer

Purpose rows are `P-n`; plan claims are `CL-n` (so they do not collide with the spike's set C ids `C-1`…`C-5`).

### 6.1 Purpose Layer: Acceptance Criteria

Checked on the real thing (real hosts, real CLIs, the built binary and PWA). Evidence details per row are obligations of PR 15 and 23 (§4). "Owner evaluation" marks subjective items; "unverified" marks items with no means of checking in the MVP.

| ID | Criterion (EARS) | Source | Observable result / failure | How to check |
|---|---|---|---|---|
| P-1 | When a user runs `chaldea spawn --agent claude` in a trusted repository, Claude Code shall run in a daemon-owned PTY with the user's skills, hooks, and permissions working, and global hook settings unchanged. | arch §1; issue #2 | Pass: native TUI in attach, a user skill works, settings unchanged. Fail: a feature missing, a settings diff. | Real Claude on macOS and Windows. |
| P-2 | When a user runs `chaldea spawn --agent codex --prompt …`, Codex shall run with `--no-daemon`, its hooks shall deliver without a review dialog after one initial trust, and its MCP servers shall stay inside the launched tree. | arch §5 | Pass: bound `SessionStart`, no dialog on relaunch, MCP pid under the CLI. Fail: dialog, MCP under a shared daemon. | Real Codex, 3 launches. |
| P-3 | When `chaldea stop` is run for a thread, no process of its tree (MCP servers included) shall be alive 5 s later, and an unrelated process shall survive. | arch §4.5; spike A-8, C-3 | Pass: no survivor. Fail: survivor. | Real CLIs with an MCP sleeper. |
| P-4 | When an orchestrator thread spawns a child with `--worktree` and waits for it, it shall receive the child's state and mailbox messages, and the child's worktree with uncommitted changes shall survive the child's exit and a daemon restart. | arch §1 | Pass: `wait` returns, message listed, files present. Fail: lost files or outcome. | Scripted real run, macOS. |
| P-5 | If removal is requested for a worktree with uncommitted changes, ignored files, unpublished commits (branch or detached `HEAD`), or a git error, then Chaldea shall refuse and leave worktree and branch unchanged. | arch §1; invariant 3 | Pass: refusal with reason. Fail: anything removed. | Real repo per case. |
| P-6 | When a prompt with Japanese, emoji, decomposed marks, and newlines is sent to Claude Code through `chaldea send` or the PWA, the prompt Claude records shall equal the expected form. | arch §1; invariant 4 | Pass: equal. Fail: other difference. | Spike corpus, both hosts. |
| P-7 | When the corpus is sent to Codex on Windows through `chaldea send`, each prompt shall arrive as exactly one `UserPromptSubmit` equal to the expected form, followed by its turn's `Stop`. | arch §5 first test | Pass: one equal submit. Fail: split, joined, marks lost, early or second submit. | PR 15, real Windows host. |
| P-8 | When the corpus is sent to Codex on macOS through `chaldea send`, each prompt shall arrive as one `UserPromptSubmit` equal to the expected form. | arch §1; spike A-6 | Pass: equal. Fail: difference. | Real Codex, macOS. |
| P-9 | When a human pastes multi-line Japanese through `chaldea attach` into Codex on Windows, it shall arrive as one prompt with its newlines and not submit or queue before the human presses Enter. | arch §5 | Pass: one prompt on the human's Enter. Fail: early submit, Tab queueing, lost newlines. | PR 15, Windows Terminal. |
| P-10 | When a user attaches to a thread that ran without a viewer, attach shall show the live screen, follow a resize, and on detach leave the thread running and the outer terminal restored. | arch §4.5; spike C-4 | Pass: current screen, resize, thread running, terminal usable. Fail: otherwise. | Second terminal, both hosts. |
| P-11 | While composing Japanese in the PWA prompt input, pressing Enter to confirm a conversion shall not send the prompt. | arch §2.4; invariant 4 | Pass: no send. Fail: send. | Real IMEs on macOS and Windows; Android IME unverified until Phase 2. |
| P-12 | When a CLI stalls on a startup prompt and no `SessionStart` arrives within the timeout, the thread shall show attention with a legible VT snapshot (CJK included), and the keypad shall clear the prompt. | arch §1, §2 | Pass: snapshot, keys reach the CLI, thread proceeds. Fail: otherwise. | Fresh untrusted folder; legibility is owner evaluation. |
| P-13 | When a CLI queries the terminal at startup, it shall get an answer and not stall. | arch §4.5; spike B-5 | Pass: reaches `SessionStart`. Fail: stall with an unanswered query. | Real CLIs, both hosts. |
| P-14 | If a hook payload or transcript line cannot be parsed or bound, then Chaldea shall keep it verbatim as `raw_unknown` with the CLI version. | arch §1 | Pass: event with raw bytes. Fail: dropped. | Malformed and nested payloads. |
| P-15 | If an input is not acknowledged within the ack window, then Chaldea shall mark it `unconfirmed`, raise attention, and never send it again on its own. | arch §4.3 | Pass: one write, `unconfirmed`. Fail: a second submit. | Dialog open over the composer. |
| P-16 | When the CLI requests an approval, the PWA shall show it and let the user resolve it, and a card the CLI no longer waits on shall be closed. | issue #2; arch §1 | Pass: card, resolution reaches the CLI, closed card disabled. Fail: otherwise. | Real Claude permission (§8 Q1). |
| P-17 | When a Claude or Codex turn ends, the thread shall show the assistant's final message. | arch §1 | Pass: `assistant_message`. Fail: missing; a CLI version that offers it in neither `Stop` nor transcript fails and is reported, never filled from the screen. | Real CLIs per version. |
| P-18 | When the daemon is killed and restarted, running threads shall show `exited` (`daemon_restart`) with history intact and no surviving old process except the documented macOS containment limit (a `setsid` grandchild whose parent exited, spike §4.6), which is reported, and a resume shall continue the session. | arch §2 | Pass: equal history, survivors none or reported as that limit, resumed context. Fail: lost or duplicate events, other survivor, resume fails. | Kill by PID mid-turn. |
| P-19 | When the PWA reconnects after the daemon was unreachable, it shall show the time of last sync while offline, catch up without duplicate or missing events, and keep older history readable. | arch §4.1 | Pass: banner, equal list, paging works. Fail: otherwise. | Stop and start the daemon. |
| P-20 | If a request has a wrong `Host`, a missing or wrong token, or a scope it lacks, then the daemon shall reject it and change nothing. | invariant 5 | Pass: 421/401/403/404, no event. Fail: accepted. | `curl` probes. |
| P-21 | When a tool output larger than 64 KiB is recorded, the thread shall show its preview and offer the full content as a download. | arch §4.2 | Pass: preview, equal download. Fail: silent truncation. | Real large output. |
| P-22 | When a configured one-shot hook runs, it shall receive the documented JSON, and events it posts to `POST /events` shall appear in the scoped stream. | arch §4.4 | Pass: input logged, surface visible. Fail: otherwise. | Sample hook script. |
| P-23 | When a Rescue Agent emits a Thread-scoped panel with a button, the action shall run only after a human clicks it, and only on that thread. | arch §1, §4.4 | Pass: action with the human's command id. Fail: otherwise. | Real Claude as Rescue Agent. |
| P-24 | Where a workspace sets a short worktree base path on Windows, an MSVC Rust project shall build inside a Chaldea worktree. | arch §5 | Pass: build succeeds. Fail: `LNK1104`. | Real repo under `C:\wt`. |
| P-25 | While Chaldea spawns CLIs and runs hooks on Windows, no console window shall appear. | prior-art row 9; spike C-5 | Pass: no window. Fail: a flash. | Owner observation. |
| P-26 | When `chaldea doctor` runs, it shall report each check as pass, fail, or inconclusive with evidence, and inconclusive (never pass) for any check it could not run. | arch §2, §6 | Pass: matches the host, broken check not passing. Fail: pass without evidence. | Both hosts, hook port blocked once. |
| P-27 | Chaldea shall be usable on a phone-sized viewport for reading threads, short prompts, approvals, and the keypad. | arch §1 | Owner evaluation; Android unverified until Phase 2. | Playwright mobile viewport + manual review. |

### 6.2 Plan Layer: Claims the Contracts Make

| ID | Claim (EARS) | Source | Falsifier |
|---|---|---|---|
| CL-1 | The thread state reducer shall accept only typed explicit inputs, none built from PTY output. | invariant 1; §2.2 | A `StateInput` variant carrying PTY bytes; PR 10 fake banner changing state. |
| CL-2 | After registration, `task_state` shall change only on `task_set`. | §2.2 | PR 1 proptest. |
| CL-3 | Each attention cause shall be cleared only by its own clearing event, without changing `turn_state`. | §2.2 | PR 1 proptest. |
| CL-4 | An input shall enter `sending` at most once and never return to `queued` or `sending`. | arch §4.3; §2.3 | PR 1 proptest; PR 10 write count. |
| CL-5 | A daemon restart or launch exit shall turn `sending`/`sent` inputs into `unconfirmed` and `queued` into `cancelled`, and only inputs of the current launch shall block the gate. | §2.3 | PR 1 proptest; PR 10 restart test. |
| CL-6 | An ack shall be attributed only by stream position within one launch and only when the writer recorded no other write in between. | §2.3 | PR 10 `key` and attach interleaving tests. |
| CL-7 | One `command_id` shall cause at most one effect, and a repeat by its token shall return the stored result. | arch §4.1; §2.4 | PR 7 receipts; PR 10 concurrent duplicates. |
| CL-8 | Each stream's `seq` shall be gapless and monotonic, and no statement shall update or delete an event. | §2.1 | PR 6 tests; PR 17 model. |
| CL-9 | A replayed source shall not create a duplicate event; distinct `emit_id`s shall each create one. | §2.1 | PR 6 key test; PR 9 HTTP test. |
| CL-10 | Projections rebuilt from events shall equal the maintained projections, and the cursor checkpoint shall be recoverable from events. | §2.1 | PR 6 drop-and-rebuild. |
| CL-11 | Transcript events and the cursor shall commit together, only from the bound session's path and only when the stored cursor matches. | arch §4.2; §2.1 | PR 6 / 13 fault injection. |
| CL-12 | A client following `decide_replay`, `before_seq`, subscribe-time heads, and periodic `heads` shall end with the server's events, without duplicates, after any disconnects, restarts, and lost responses. | arch §4.1, §6 | PR 7 edge cases; PR 17 model; PR 18 fast-check reducer. |
| CL-13 | Readers shall render unknown kinds and newer schema versions as `GenericEvent`, and generated TS and JSON Schema shall equal what the Rust types generate. | §2.1 | PR 1 decode and drift tests; PR 18. |
| CL-14 | Every route shall enforce the route and action matrices, including object-level checks. | §2.5 | PR 7 matrix tests; PR 20 rescue tests. |
| CL-15 | Rejected requests shall get the status of §2.5 (or EOF for a slow head) before any body byte is used, and create nothing. | invariant 5; spike B-8 | PR 9 B-8 cases on both OSes. |
| CL-16 | A hook shall bind only through §2.9's rule; a nested CLI inheriting the environment shall not bind. | prior-art row 6c; spike B-11 | PR 9 nested FakeCli. |
| CL-17 | A resume shall not start while the old launch's death is unconfirmed. | §2.9 | PR 10 restart test. |
| CL-18 | The input encoding shall be chosen by `(agent, os)`, never by DECSET 2004, and bracketed paste shall be written only while 2004 is set and the thread is bound and idle. | spike F-1f; prior-art row 4a | PR 5 table test; PR 10 gate tests. |
| CL-19 | An approval card shall resolve only on evidence correlated to it, and close on `Stop` or launch exit. | §2.10 | PR 13 FakeCli sequences. |
| CL-20 | No code path shall remove a worktree except `worktree remove` passing its checks or owner `--force`, which keeps the in-use check. | arch §4.3; §2.6 | PR 16 tests. |
| CL-21 | The PWA prompt input shall not submit while `isComposing`, on `keyCode === 229`, or on the Enter right after `compositionend`. | invariant 4; arch §2.4 | PR 19 Playwright. |
| CL-22 | A hook emitter run shall exit 0, write nothing to stdout, and finish within 3 s, whatever the daemon does. | arch §2; spike B-9 | PR 9 emitter tests. (§8 Q1 option A would narrow this for `PermissionRequest`.) |
| CL-23 | `core` shall build and pass its tests without tokio, filesystem, network, process, or clock access. | §3 | CI dependency check; clippy list. |

## 7. Verification Plan

| arch §6 item | What | PR |
|---|---|---|
| Real-CLI contract corpus (`fixtures/`) | `fixtures/<agent>/<cli-version>/<case>/`: hook payloads (`<Event>-<n>.json` + allowlisted `.meta.json`, spike §4.3) and transcript excerpts, captured by `chaldea doctor capture` and scrubbed. PR 12 validates envelopes and payload presence; the adapter PRs decode every fixture, and a known event decoding to `raw_unknown` fails. New CLI versions add a directory. | 12; 13, 14 |
| PBT (`proptest` / `fast-check`) | reducer and input machine (1); encoders (5); projection rebuild (6); client reducer (18). | 1, 5, 6, 18 |
| `stateright` | Sync Seam only: crash during append, offline client buffering, retry after a lost response. Properties: no gap, no duplicate, one effect per `command_id`. | 17 |
| `cargo-fuzz` | JSONL line parser, hook payload decoder, VT responder (replies in one write). Nightly, scheduled, seeded from `fixtures/`. | 22 |
| Cross-OS CI | macOS + Windows from PR 1; FakePty / FakeCli in every PR; Windows injections in PR 4. | 1 onward |
| `chaldea doctor` | Host/token round trip; hook round trip through `hook-emit`; FakeCli in a PTY with a VT query, paste echo, and tree kill; Windows parent job; ConPTY module after that spawn; worktree base length; CLI versions and Codex trust state. | 21 |
| Playwright | Desktop and mobile viewports; IME composition sequences; send, approval, keypad against FakeCli. | 18, 19 |
| Real-host E2E | §6.1 on macOS and Windows with versions recorded. | 15, 23 |

**Verdicts of evidence-collecting harnesses.** `chaldea doctor`, `doctor capture`, and the real-host runs decide their verdict in one function, `core::verdict::evaluate(plan: &CheckPlan, evidence: &[Check]) -> Verdict`, lifted from `spikes/pty-hooks/src/verdict.rs`. Outcomes are typed: `Pass`, `PlatformFail { cause }` (positive evidence of what the CLI or OS did), `HarnessFail { cause }`, `Missing`, `NotExercised { reason }`.

- The plan (required evidence keys and expected context: run, launch, OS build, CLI version) is fixed before observation. An empty plan is `Inconclusive`.
- Each required key must appear exactly once, with its content checked; a second observation of the same key, even with the same outcome, is `HarnessFail`. This replaces the spike's "last check per key wins" (`verdict.rs:116-135`). Evidence outside the plan is recorded and does not affect the verdict, also a change from the spike.
- A context mismatch or null evidence is `HarnessFail`.
- Completeness first: any required key `Missing`, `HarnessFail`, or `NotExercised` → `Inconclusive`; else `Pass` when all passed, else `Fail`. Missing evidence, an observation failure, or a check that did not run is never `Pass` (spike §1).

## 8. Open Questions

Only items that change public behaviour or are irreversible.

**Q1. How approvals are answered.**

| Option | Gains | Loses | Reversibility | Cost |
|---|---|---|---|---|
| A. Hook decision: for `PermissionRequest`, `hook-emit` waits for the user's decision and prints the CLI's decision JSON (Claude: `hookSpecificOutput.decision.behavior` allow/deny, per the Claude hooks reference) | One-tap approve/deny; resolution is exact. | The emitter blocks for human time, so CL-22's 3 s / empty-stdout rule must exclude `PermissionRequest`; Claude's default command-hook timeout is 600 s (hooks reference), and the hook's configured timeout bounds the wait; Codex's decision schema is **unverified**. | Medium. | Fixtures per CLI version. |
| B. Keypad only: the card shows tool and arguments plus the VT snapshot; the user answers with keys | No emitter change; same mechanism for both CLIs. | Correlation: Claude's payload has no `tool_use_id`, so cards correlate only through the inferred `PreToolUse` match (**unverified**), and Codex's is **unverified**; outcomes after a denial are "unknown"; slower on a phone. | High. | Low. |

Re-review cost: Codex keys hook trust by source, event, and position and hashes the normalized handler (prior-art row 6b), so moving from B to A costs no new review if the `PermissionRequest` entry, its command string, and its timeout are already registered unchanged; adding the entry later, or changing its timeout, costs one review per host (whether the timeout is in the hash is **unverified**). Recommendation: B in the MVP, with the Codex `PermissionRequest` hook registered from PR 14 with its final command string and timeout, so A can follow for Claude once PR 12 shows its schema. Changes if: the owner requires one-tap approval in the MVP; then A for Claude goes into PR 13.

**Q2. CLI surface for `task_state`.** This design adds these commands beyond issue #2's list: `open`, `resume`, `close`, `input resolve`, `worktree remove`, `daemon run/start/stop`, `doctor capture`, and one for task state. Options: (A) `chaldea task set <state> [--summary] [--thread]` — explicit and greppable in agent instructions, one more command; (B) `chaldea mailbox send --task-state <state>` — no new command, but couples state to a message. Both are public CLI flags: adding is easy, removing is not; cost low either way. Recommendation: A. Changes if: issue #2's command list is meant to be closed.

**Q3. Input states.**

| Option | Gains | Loses | Reversibility | Cost |
|---|---|---|---|---|
| A. The eight states of §2.3 public | Crash recovery and human resolution are visible exactly; `unconfirmed` means only "may have reached the CLI". | Larger public enum. | Removing states later breaks clients. | As designed. |
| B. The architecture's four, with pre-write failures as `unconfirmed` reasons | Smallest surface. | Validation failures look like possible deliveries and block the queue; restart cannot say "zero bytes". | Widening later is additive. | Low, but recovery logic in the UI becomes guesswork. |
| C. Persist `sending` internally; public `queued`, `sent` (covering `sending`), `acked`, `unconfirmed`, and a terminal `closed { reason: rejected \| cancelled \| dismissed }` | Close to the architecture's list; recovery still exact internally. | A mapping layer between internal and public states; `closed` hides which side ended it unless the reason is read. | Splitting `closed` later is additive. | A mapping function in core and its tests; no store migration, since the internal states are stored either way. |

Recommendation: A. Changes if: the owner wants the public set to stay close to the architecture's four, then C.

**Q4. Failure policy of `before_spawn`.** (A) Fail-open: a timeout or crash allows the spawn and records the failure; a broken extension never blocks work. (B) Fail-closed: denies; suits policy hooks, but a broken hook blocks all spawns. (C) Fail-open by default with a per-hook `fail_closed = true` key: both uses, at the cost of one config key and a doctor line listing fail-closed hooks and their last failure. The default is public behaviour and hard to flip later; costs are low. Recommendation: C. Changes if: `before_spawn` is meant for security policy first, then B.

**Q5. Codex launch without a first prompt.**

| Option | Gains | Loses | Reversibility | Cost |
|---|---|---|---|---|
| A. First prompt mandatory for Codex (`spawn` and `resume` need `--prompt`) | Every launch binds and is controllable from turn 1 (arch §5). | The prompt is in argv and shows in process listings; Windows limits a command line to 32,767 UTF-16 units (`CreateProcessW` docs), and the usable prompt length after quoting and Codex's other arguments is unmeasured; no flow for users who start Codex and type later, resume without a new turn, or inspect settings before any model call. | Adding B later is additive. | Low. |
| B. Also an unbound manual mode (`--interactive`): no prompt; no programmatic input until a human's first turn through attach binds the session | Covers those users. | No hook-based state until the first turn; `send` refused until bound; more UI states. | Additive; removing it later breaks users. | Medium. |

Recommendation: A now, B when one of those uses is needed. Changes if: the PWA must offer "new Codex thread" without a prompt in the MVP.

**Q6. Codex-on-Windows prompts with leading/trailing whitespace or TAB** (Codex trims them, and with paste-burst detection off a TAB submits early; arch §5). The rule is the same for `send` and for pastes the attach client recognises. (A) Reject with zero bytes written (spike F-1d); the user edits the text. (B) Offer an explicit transform per send or paste (`--transform trim,tab-to-spaces`, or a confirmation in the PWA and attach) and compare fidelity against the transformed text. A never sends something other than what was written but refuses pasted code with tabs; B is convenient and adds a public flag. Adding B later is additive; cost low. Recommendation: A in the MVP. Changes if: rejections turn out frequent.

**Q7. Ignored files on a normal worktree remove.** `git worktree remove` without `--force` refuses untracked changes but deletes ignored files, which can include `.env` files or local data. (A) Refuse when ignored files exist unless the owner passes `--discard-ignored` (agents cannot); nearly every built worktree (`target/`, `node_modules/`) then needs the flag. (B) Delete ignored files, as git does. The default can be changed later in either direction, but files deleted under B are unrecoverable. Cost low. Recommendation: A, under the never-lose rule (arch §1, invariant 3). Changes if: the owner treats ignored files as regenerable by policy.

**Decided here (not asked).**

- Agents get no raw `key` and no `input resolve`; only owner and rescue tokens do. Reason: a raw key can approve a dialog or submit another thread's composer, and resolving an input is a human judgment; widening later is additive.
- A resume is refused until the old launch's death is confirmed (§2.9). Reason: two live CLIs on one session corrupt it; refusing is safe and can be relaxed.
- B-8 expectations follow RFC 9112: `Content-Length: +5` → 400; a slow head → connection closed within its budget, observed as EOF, never inferred from a client timeout or `status: 0` (§2.5). Reason: the spike's 411 contradicts RFC 9112 §6.3, and an incomplete request has no response to observe.

## Proposed Canonical-Doc Changes

Not applied here; proposals for `docs/architecture.md`, `docs/prior-art.md`, `AGENTS.md`.

- **arch §4.1**: `epoch` checked in both directions, `mode: replay | snapshot` with `as_of_seq`, `before_seq` paging, notify-then-pull with subscribe-time and periodic heads, WS tickets plus `Origin` check, receipt states including `unknown_after_crash`.
- **arch §4.2**: the event kinds of §2.1 (adding launch, session, input, task, worktree, and snapshot kinds; `artifact_diff` deferred), two stream kinds, open-envelope decoding, attention as a cause set beside the three axes.
- **arch §4.3**: if Q3 A or C is accepted, the input states and ack attribution by stream position.
- **arch §2 / AGENTS.md invariant 5**: the token kinds (`owner`, `agent`, `hook`, `rescue`, `extension`) next to "regular vs Rescue".
- **arch §5 open items**: the Windows hook receiver item concerns the axum receiver; keep `deadline_slow_head` (EOF, no side effect) and `non_numeric_content_length` (400) as checks to run on CI and the real host.
- **prior-art**: rows for event store and replay (T3 Code v1/v2, `decideThreadResume`), command receipts, scoped auth and pairing (T3 Code), thread/turn status and schema fixtures (Codex app-server), worktree removal checks (T3 Code vs herdr), herdr's Codex-on-Windows paste-boundary key (`src/app/api/agents.rs:14-33`), and Claude `PermissionRequest` lacking `tool_use_id`.
- **README status**: "MVP implementation has not started" changes when PR 1 merges.
