# MVP Design: Local-First Chaldea

Status: design, not implemented. It plans the local-first MVP of issue #2. Canonical constraints: `AGENTS.md` and `docs/architecture.md` (cited as "arch §N"). Mechanism decisions: `docs/prior-art.md` (cited as "prior-art row N"). Spike evidence: `docs/spikes/pty-hooks-spike.md` (cited as "spike §N" or by its case ids such as "spike A-8", with report ids) and `spikes/pty-hooks/src/`.

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
- `thread` — one per thread: everything about that thread, across all its launches. Stream id `thread:<thread_id>`.

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
| `kind` | string | event kind (below); open set on the wire |
| `thread_id` | UUID or null | set for thread streams and for host events about one thread |
| `launch_id` | UUID or null | the launch (one process lifetime of a thread) the event belongs to |
| `command_id` | UUID or null | the command that caused the event, if any (§2.4) |
| `source` | object | `{ "kind": "hook" \| "transcript" \| "process" \| "command" \| "daemon" \| "extension", "agent": "claude" \| "codex" \| null, "cli_version": string \| null, "transcript": { "generation", "path", "offset_end" } \| null }` |
| `occurred_at` | RFC 3339 UTC, ms | source time when the source states one, else equal to `recorded_at` |
| `recorded_at` | RFC 3339 UTC, ms | daemon append time |
| `payload` | object | kind-specific (below) |

**Forward-compatible decoding.** A reader decodes the envelope first, with `kind` as a string and `payload` as raw JSON, then decodes the payload by kind. An unknown `kind`, a `schema_version` above the reader's, or a payload that fails to decode yields a `GenericEvent` (envelope fields plus raw payload) that the PWA renders as "unknown event" and the CLI prints as JSON; it never fails the page or the stream. In Rust, `EventEnvelope` carries `kind: EventKindTag` (a string newtype) and `payload: Box<serde_json::value::RawValue>`, and `TypedEvent::decode(&EventEnvelope) -> Result<TypedEvent, GenericEvent>` gives the typed view; the generated TypeScript exports a discriminated union of known kinds plus `GenericEvent`. This also avoids `#[serde(flatten)]` over an adjacently tagged enum, whose `ts-rs` and deserialization output would need separate checking.

**Content fields.** Payload fields that can be large (`text`, `prompt`, `output`, `raw`, VT grids) have the public type `Content = { "inline": string } | { "blob": { "sha256", "size", "media_type" }, "preview": string }`. Above 64 KiB the content moves to a blob and `preview` holds its first 1 KiB (UTF-8 boundary). The rest of the payload stays inline, so a reader never fetches a blob to learn what an event is (arch §4.2 "size-bounded/referenced large tool outputs"). Blobs are fetched through the stream that references them (§2.5).

`schema_version` is one integer for the whole event schema set, starting at 1. Additive changes (a new optional field, a new kind) do not bump it; a change that alters the meaning or type of an existing field does. Per-kind versions were considered and rejected: they multiply compatibility checks without a client that could use the extra precision.

**Idempotency keys** are deterministic so that a crash-and-replay of the same source produces the same key, and the store rejects the duplicate (`UNIQUE (stream_id, idempotency_key)`):

| Source | Key |
|---|---|
| Hook payload | `hook:<emit_id>`, where `emit_id` is a UUID the emitter generates once per invocation and sends as `X-Chaldea-Emit-Id` (scope below) |
| Transcript line | `transcript:<generation>:<offset>` of the line start (cursor below) |
| Tool activity | `tool:<launch_id>:<tool_use_id>:<phase>` when the source carries a tool id (shared by hook and transcript, see `tool_activity`) |
| Process lifecycle | `launch:<launch_id>:<started\|exited>` |
| Command | `command:<command_id>:<n>` for the n-th event the command produced |
| Derived event (state change caused by another event) | `derived:<cause idempotency_key>:<name>` |
| Extension `POST /events` | `ext:<caller key>`, required from the caller |

**Hook delivery id scope.** The emitter makes at most two attempts per invocation, both with the same `emit_id`: a retry after a connect failure (no request sent) and a retry after a request sent with no response read, both inside the 3 s budget (spike B-9 budget). Both are deduplicated by the key. Not guaranteed: a CLI that runs the same hook again starts a new emitter process with a new `emit_id`, and that delivery is recorded as a second event. Content is not deduplicated, because two identical prompts produce byte-identical `UserPromptSubmit` bodies and both are real. PR 9 tests both cases over HTTP (same `emit_id` twice → one event, both answered 200; two `emit_id`s with the same body → two events).

**Event kinds** (public). The kinds of arch §4.2 plus the ones the other seams and the projection rebuild need:

| Kind | Stream | Produced by |
|---|---|---|
| `thread_registered` / `thread_closed` | host | spawn / close |
| `launch_started` | thread | `PtyRuntime::spawn` returned; payload `{ launch_id, pid, start_time, pgid_or_job, agent, cli_version, argv_digest, relaunch_of }` |
| `launch_exited` | thread | exit observed or confirmed at restart; payload `{ launch_id, code, signal, reason: "exit" \| "stopped" \| "spawn_failed" \| "daemon_restart" \| "unknown_after_crash" }` |
| `session_bound` | thread | binding rule (§2.9); payload `{ launch_id, session_id, transcript_path }` |
| `user_message` | thread | `UserPromptSubmit` hook (prompt as the CLI recorded it); payload carries `input_id` when attributed to an input (§2.3) |
| `assistant_message` | thread | Claude transcript line; Codex: `Stop` payload when it carries the last message, else the Codex transcript when `transcript_path` is non-null (field presence and format **unverified**, settled by PR 12 fixtures) |
| `tool_activity` | thread | phase `started` from `PreToolUse`, phase `completed` from `PostToolUse` (authoritative); a Claude transcript tool line produces the same idempotency key and is dropped as duplicate when the hook already arrived, so each `(tool_use_id, phase)` exists once. Without a tool id in the payload, only hooks produce `tool_activity`. |
| `status_changed` | thread | daemon, whenever an axis of §2.2 changes; payload `{ axis, from, to, cause }`, where `cause` is typed (e.g. `{ "attention": "session_start_timeout" }`) |
| `approval_requested` / `approval_resolved` | thread | §2.10 |
| `input_queued` | thread | `send` accepted; payload `{ input_id, text: Content, encoding, origin }` |
| `input_state_changed` | thread | input state machine (§2.3) |
| `mailbox_message` | thread (recipient) | `chaldea mailbox send` |
| `task_set` | thread | explicit task state change; payload `{ to, by, summary }` |
| `artifact_diff` | thread | reserved; not produced in the MVP (diff view is a reversible layer) |
| `surface_update` | host or thread | A2UI emit (§2.8) |
| `vt_snapshot_taken` | thread | daemon on attention; payload `{ grid: Content, cols, rows, cursor, modes }` |
| `worktree_changed` | host | worktree intent / create / owner change / release / remove (§2.6) |
| `raw_unknown` | thread if bound, else host | any input that could not be parsed or bound |

`raw_unknown` payload: `{ "source", "reason": "unparseable" \| "unknown_event" \| "foreign_session" \| "unbound" \| "schema_mismatch" \| "partial_line", "agent", "cli_version", "content_type", "raw": Content }` with `raw` verbatim (base64 in `inline` with `"encoding": "base64"` when not UTF-8). Nothing is dropped (arch §1 self-healing; spike B-11).

**SQLite schema sketch** (internal; the database file is not a public format):

```sql
PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA recursive_triggers=ON;  -- so REPLACE fires delete triggers
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);          -- migration level, host_id, epoch, last_alive_at
CREATE TABLE streams (stream_id TEXT PRIMARY KEY, kind TEXT NOT NULL, thread_id TEXT, head_seq INTEGER NOT NULL);
CREATE TABLE events (
  global_seq INTEGER PRIMARY KEY AUTOINCREMENT,
  stream_id TEXT NOT NULL REFERENCES streams(stream_id),
  seq INTEGER NOT NULL,
  event_id TEXT NOT NULL UNIQUE,
  schema_version INTEGER NOT NULL, idempotency_key TEXT NOT NULL, kind TEXT NOT NULL,
  thread_id TEXT, launch_id TEXT, command_id TEXT,
  source_json TEXT NOT NULL, occurred_at TEXT NOT NULL, recorded_at TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  payload_bytes INTEGER NOT NULL, cum_bytes INTEGER NOT NULL,           -- running total per stream: O(1) replay size
  UNIQUE (stream_id, seq), UNIQUE (stream_id, idempotency_key)
);
CREATE TRIGGER events_append_only_u BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT, 'append-only'); END;
CREATE TRIGGER events_append_only_d BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT, 'append-only'); END;
CREATE TABLE blobs (sha256 TEXT PRIMARY KEY, size INTEGER NOT NULL, media_type TEXT NOT NULL, data BLOB NOT NULL);
CREATE TABLE blob_refs (stream_id TEXT NOT NULL, sha256 TEXT NOT NULL, PRIMARY KEY (stream_id, sha256));
-- Projections: changed only with the event that changes them, in its transaction; rebuildable from events.
CREATE TABLE threads (...); CREATE TABLE launches (...); CREATE TABLE inputs (...); CREATE TABLE approvals (...);
CREATE TABLE transcript_cursors (thread_id TEXT PRIMARY KEY, generation INTEGER NOT NULL, path TEXT NOT NULL,
  file_id TEXT NOT NULL, offset INTEGER NOT NULL);
CREATE TABLE worktrees (...);   -- §2.6
-- Not projections (not derivable from events):
CREATE TABLE command_receipts (command_id TEXT PRIMARY KEY, token_id TEXT NOT NULL, kind TEXT NOT NULL,
  request_sha256 TEXT NOT NULL, status TEXT NOT NULL,   -- in_progress | completed | failed | unknown_after_crash
  accepted_at TEXT NOT NULL, result_json TEXT);
CREATE TABLE tokens (...);      -- §2.5; secrets are never in events
```

The store never issues `REPLACE` or `UPSERT` on `events`; with `recursive_triggers=ON` an `INSERT OR REPLACE` that conflicts fires the delete trigger and aborts, which PR 6 tests (without the pragma SQLite does not fire delete triggers for REPLACE).

**Projection rebuild.** Every projected field maps to an event, so dropping the projection tables and replaying `events` in `global_seq` order reproduces them (PR 6 drop-and-rebuild equality test):

| Projected field | Event |
|---|---|
| thread agent, cwd, parent, initial task | `thread_registered` |
| axes (`process`, `turn`, `task`) | `status_changed` (with `task_set` for `task`) |
| current launch, pid, start time, exit reason | `launch_started`, `launch_exited` |
| `session_id`, `transcript_path` | `session_bound` |
| attention cause | `status_changed.cause` |
| pending input body, encoding, state, attribution | `input_queued`, `input_state_changed`, `user_message.input_id` |
| pending approval | `approval_requested`, `approval_resolved` |
| transcript cursor | `source.transcript` of the last transcript event |
| worktree record and owner | `worktree_changed` |

Tokens and command receipts are not projections: tokens are secrets, and receipts store results for idempotent replies; both survive only in their own tables.

**Transcript cursor** (internal). The cursor is `(generation, path, file_id, offset)`:

- **Path origin.** A path is accepted only from a hook bound to the thread's current session (`session_bound`), never by guessing a directory (arch §4.2). A path from an unbound or foreign hook goes to `raw_unknown`.
- **Identity.** The tailer opens the file, checks it is a regular file, and takes `file_id` (device + inode on Unix, volume serial + file index on Windows) from the same open handle it reads from, so a swap between stat and open cannot mix files.
- `offset` is the byte offset just after the last complete line (ending in `\n`); a trailing partial line waits for the next read.
- **Generation.** It increments when the path changes, when `file_id` differs from the cursor's, or when the file is shorter than `offset` (truncation). A new generation restarts at offset 0.
- **Atomic append.** The events from a batch of lines and the new cursor commit in one transaction whose first statement is `UPDATE transcript_cursors SET … WHERE thread_id = ? AND generation = ? AND file_id = ? AND offset = ?`; zero rows changed aborts the batch (a stale or concurrent tailer), so a crash keeps both or neither (arch §4.2). The idempotency key is the second line of defence.
- **Exit.** After `launch_exited`, the tailer reads to EOF once more; a trailing partial line is recorded as `raw_unknown` (`partial_line`), never dropped.
- `transcript_path: null` (Codex, arch §5) creates no cursor; that launch is ingested from hooks only.

Options compared: an `(inode, offset)` cursor without generation (rejected: a rotated or replaced file at the same path silently mixes histories); a line-count cursor (rejected: needs a rescan and breaks on CRLF/partial lines); a content hash of the last line (rejected: identical lines repeat). The generation counter is the architecture's decision and is kept.

**Public vs internal.** Public: envelope, kind names, payload types, `Content`, `GenericEvent`, `raw_unknown` payload, `schema_version` semantics, the `X-Chaldea-Emit-Id` header, idempotency key formats for `POST /events` callers. Internal: SQL schema, projection tables, `global_seq`, blob storage layout, other key formats.

### 2.2 3-Axis Thread State

Three independent axes (arch §2, §4.2). Every transition is caused by an explicit, typed input; the reducer's input type has no variant built from PTY output, so screen text cannot drive state (invariant 1). The VT snapshot and DECSET 2004 are read only to gate input (§2.3) and to show a human what the CLI shows.

**`process_state`** — the current launch's OS process.

| State | Entered on |
|---|---|
| `starting` | spawn or relaunch accepted (launch intent persisted, §2.9) |
| `running` | `launch_started` |
| `stopping` | `stop` command accepted |
| `exited` | `launch_exited` |

Session binding is a thread field (`session_bound`), not a process state: Claude binds at startup, Codex only with its first turn (arch §5), so a shared process state would mean different things per agent.

**`turn_state`** — the CLI's conversational turn.

| State | Entered on |
|---|---|
| `unknown` | before the first explicit signal of a launch, and after `exited` |
| `idle` | Claude `SessionStart` (eager); `Stop` hook (both agents) |
| `running` | `UserPromptSubmit` bound to the thread; `approval_resolved` |
| `awaiting_approval` | `approval_requested` |
| `needs_attention` | an attention cause: SessionStart timeout from launch (default 20 s, spike §4.7, arch §2 landmine), an input `unconfirmed` (§2.3), an input gate timeout (§2.3), an `attention` extension hook, an expected Codex `Hooks need review` stall after an upgrade (arch §5) |

`needs_attention` clears on the next explicit turn signal (`SessionStart`, `UserPromptSubmit`, `Stop`) or on an explicit `attention_cleared` command. Timers are explicit inputs: the daemon feeds `Tick(now)` to core, and core emits the attention cause; it is an event with a cause, not an inference.

**`task_state`** — the outcome of the work the thread was given, for orchestrators (arch §1 "inspect child outcomes").

| State | Entered on |
|---|---|
| `none` | registration without a task |
| `active` | registration with a task, or `task_set` |
| `blocked`, `completed`, `failed`, `cancelled` | `task_set` by the thread itself (agent token), its parent, or a human (§8 Q2 for the CLI surface) |

The daemon never infers `task_state`; process exit and relaunch do not change it. **Relaunch** (resume after exit or daemon restart) is its own input: `process` → `starting`, `turn` → `unknown`, `task` unchanged, no new `thread_registered`. An orchestrator therefore sees "process exited, task still active" and decides; that judgment stays in agent instructions (invariant 2). This is the reason for three axes instead of one.

Options compared:

| Option | Precedent | Verdict |
|---|---|---|
| One status enum | herdr `AgentState { Idle, Working, Blocked, Unknown }` derived from screen detection (`src/detect/mod.rs:11-20`) | Rejected: mixes process, turn, and task, and is fed by screen scraping. |
| Thread status + turn status | Codex app-server `ThreadStatus { NotLoaded, Idle, SystemError, Active { active_flags: [WaitingOnApproval, WaitingOnUserInput] } }` (`src/protocol/v2/thread.rs:1810-1827`) and `TurnStatus { Completed, Interrupted, Failed, InProgress }` (`v2/turn.rs:34-39`) | Partly adopted: its approval flag maps to `awaiting_approval`. It has no task axis because it has no orchestration outcome. |
| Three axes | arch §2 | **Adopted.** |

Public: axis names, state values, `status_changed` and `task_set` payloads. Internal: the reducer, timer defaults.

### 2.3 Input State Machine

An input is one prompt sent through `chaldea send`, the PWA, or `surface_action`.

```
queued ──(gate open; attempt persisted)──▶ sending ──(text + submit written)──▶ sent ──(attributed ack)──▶ acked
   │                                         │                                   │
   │                                         └─(write outcome unknown, crash)─────┴─(ack window expired, or ack not attributable)──▶ unconfirmed
   ├─(validation failed, zero bytes)──▶ rejected                                                               │   (late attributed ack ──▶ acked)
   └─(cancel, thread exited, or daemon restart before any byte)──▶ cancelled                                    └─(human resolution)──▶ acked (by human) | dismissed
```

- **`queued`**: `input_queued` persisted, no byte written. The gate opens when the thread is bound, `turn_state = idle`, no other input of the thread is `sending`, `sent`, or `unconfirmed`, no attach client has written input since the last `Stop` (below), and, for bracketed paste, `TermMode::BRACKETED_PASTE` is set (prior-art row 4a; spike §4.4). One input is in flight per thread. **Gate timeout:** an input still `queued` after 60 s raises attention (`input_gate_timeout`) with a `vt_snapshot_taken`; it stays `queued` with zero bytes written until the gate opens or a human cancels it.
- **`sending`**: one transaction marks the input `sending`, records `sent_after_seq` (the thread stream's head at that moment), and keeps the command receipt `in_progress`; only after it commits are the bytes handed to the PTY writer. A crash in this state cannot tell whether bytes reached the PTY.
- **`sent`**: the text and the one submit key were written. The ack window (default 30 s, spike §4.7) starts.
- **`acked`**: the input's ack is the first `UserPromptSubmit` of the same launch with `seq > sent_after_seq`, provided no attach client wrote input bytes between the start of the input's write and that event. Position in the stream, not wall time, decides. The `prompt` is compared with the adapter's expected form and recorded as `fidelity: ok | differs`; fidelity never changes the ack (spike §4.4 "oracle split").
- **`unconfirmed`**: the window expired; or the matching `UserPromptSubmit` is not attributable because an attach client wrote input in between (the event is kept as `user_message` without `input_id`); or the input was `sending` when the daemon restarted; or the submit write started without reporting completion (spike §4.4). **Never resent** (arch §4.3). The thread goes `needs_attention` and the queue stays blocked, because the composer may still hold the text. A late attributable ack moves it to `acked`.
- **Human resolution** (`chaldea input resolve <input_id> --delivered | --dismiss`, or the PWA): `--delivered` → `acked` with `by: human`; `--dismiss` → `dismissed`. Both keep the history and open the gate; a dismissed prompt can be sent again only as a new input.
- **`rejected`** (ESC or C0 other than TAB/CR/LF, prior-art row 4b; Codex-on-Windows lossy input or empty prompt, spike F-1d; §8 Q6) and **`cancelled`** (a human cancel, the thread exited, or a daemon restart found it `queued`) are terminal before any byte.

**Programmatic vs human input.** `chaldea attach` writes into the same PTY. The daemon records the stream position of each attach input write; while attach input exists since the last `Stop` of the launch, programmatic inputs stay `queued` (and the gate timeout applies). This keeps attribution exact and keeps a human's half-typed text from being submitted with a prompt.

Ack per adapter (capability `ack_event`):

| Adapter | Ack | Notes |
|---|---|---|
| Claude Code | `UserPromptSubmit` of the launch bound by `--session-id` | `prompt` is Claude's own rendering: TAB → 4 spaces, NFC, `<pasted_content>` wrapper (arch §5); fidelity accounts for these transforms. |
| Codex CLI | `UserPromptSubmit` of the launch, bound by §2.9 | Send only after the previous turn's `Stop` (arch §5). The positional first prompt is input 1 with `sent_after_seq` at `launch_started`. |

Alternatives: auto-resend after a timeout (rejected by arch §4.3; an extra Return can accept a popup or run a slash command, arch §5); ack by screen echo (rejected, invariant 1); ack by time window alone (rejected: a human's attach input inside the window is indistinguishable); an SDK or app-server protocol with explicit turn acks (T3 Code's approach; rejected by invariant 1, no ACP lock-in).

Public: state names, `input_queued` / `input_state_changed` payloads, `send` and `input resolve`. Internal: window and timeout defaults. The states beyond the architecture's four are §8 Q3.

### 2.4 Sync API

**Replay.** `GET /api/events?stream_id=<id>&after_seq=<N>&limit=<L>[&epoch=<E>]` (arch §4.1). `limit` defaults to 200, range 1–1000 (`limit=0` or above 1000 → 400). `epoch` is optional on the first fetch; every response carries the current `epoch`, and the client sends it on later fetches. Response (public type `EventsResponse`):

```json
{ "mode": "replay", "stream_id": "thread:…", "epoch": "…", "events": [ … ], "head_seq": 812, "has_more": false }
{ "mode": "snapshot", "stream_id": "thread:…", "epoch": "…", "snapshot": { … }, "as_of_seq": 812, "reason": "epoch_changed" }
```

Snapshot fallback when: `epoch` is given and differs from the database's (the database was recreated; `epoch` is a random id stored in `meta` at creation), `after_seq > head_seq`, `after_seq < retained_from - 1` (no compaction in the MVP, so `retained_from` is 1; the rule is fixed now for later compaction), or the replay would exceed 10 000 events or 8 MiB, computed in O(1) from `cum_bytes`. The decision is one pure function in core, `decide_replay`, as in T3 Code's `decideThreadResume` (`apps/server/src/orchestration-v2/ThreadStream.ts:99-118`, bounds at `:18-20`). The snapshot and its `as_of_seq` are read in **one read transaction**, so the projection matches the sequence number exactly. A thread snapshot holds the projection (axes, session, launch, worktree, pending input, pending approval) and the last 50 events; a host snapshot holds the thread list. An unknown `stream_id`, or one the token may not read, returns 404 (no existence leak); a known stream with no events returns `replay` with `events: []`, `head_seq: 0`.

**Backward history.** `GET /api/events?stream_id=<id>&before_seq=<N>&limit=<L>` returns up to `L` events with `seq < N` in ascending order and `has_more_before`. A client that started from a snapshot reads older history this way.

**Live notifications.** `GET /api/ws?ticket=<t>` upgrades to a WebSocket. Browsers cannot set `Authorization` on a WebSocket, so the client first calls `POST /api/ws-ticket` with its Bearer token (§2.5). Messages (public):

```json
// server → client
{ "type": "hello", "epoch": "…", "server_time": "…" }
{ "type": "stream_advanced", "stream_id": "thread:…", "head_seq": 813 }
// client → server
{ "type": "subscribe", "stream_ids": ["host", "thread:…"] }
```

Notify-then-pull: the WS carries only heads, and the client fetches with `GET /api/events`. On every `subscribe` the server immediately sends `stream_advanced` with the current head of each subscribed stream, and the client refetches on every reconnect, so a lost last notification or an append between two pages costs latency, not correctness. Inline event push was considered and deferred: it can be added later as an optional field.

**Commands and idempotency.** Every mutating request carries a client-generated `command_id` (UUID): spawn, send, key, stop, approval decision, attention clear, input resolve, task set, mailbox send, worktree remove, `surface_action`. Receipts follow T3 Code's `orchestration_v2_command_receipts` keyed by `command_id` with a result sequence (`Migrations/055_OrchestrationV2.ts:42-50`), with these additions:

- The receipt is inserted as `in_progress` in the same transaction that records the command's intent (`input_queued`/`sending`, launch intent, worktree intent). The primary key makes a concurrent duplicate fail its insert, so two requests with one `command_id` cannot both reach the PTY or git.
- `ReceiptLookup` (core): `New`, `InProgress` (another request holds it: 409 with `retry_after`), `Replay(result)` (same hash: the stored result), `Conflict` (different hash or another token: 409 without the result), `UnknownAfterCrash` (attempted before a daemon restart: returned as a result with `status: "unknown_after_crash"`, never re-executed).
- At daemon start every `in_progress` receipt becomes `unknown_after_crash`, and its intent is reconciled: `send` inputs `sending` → `unconfirmed`, `queued` → `cancelled`; `key` → unknown (the key is not resent); `spawn` → §2.9 restart rules; `worktree remove` → §2.6 reconciliation.

**Offline.** When the daemon is unreachable, the PWA keeps the last fetched state, locks input, and shows the time of the last successful sync (arch §4.1 read-only lock with freshness indicator).

The Phase 2 relay can cache per stream and forward commands unchanged; that is why `stream_id`, `epoch`, and `command_id` are in the public contract now. A `stateright` model (PR 17) checks startup crash, offline buffering, and retry after a lost response (arch §6).

PR 7 test obligations: `limit=0` and above max, empty stream, unknown stream, unreadable stream, `after_seq = retained_from - 1`, `after_seq = head_seq`, `after_seq > head_seq`, append between two pages, epoch mismatch, byte bound, `before_seq` at 1, lost last notification (subscribe after the append).

Public: all request/response/message shapes above. Internal: replay bounds, ticket lifetime.

### 2.5 Auth

The daemon listens on `127.0.0.1:<port>` only. The HTTP stack is `axum` + `hyper` + `tower-http` (arch §3), so request parsing is hyper's, not ours (prior-art row 5a's reason). Constant-time token comparison uses `subtle` (prior-art row 5d); stored tokens are SHA-256 hashes (`sha2`, prior-art row 5c).

**Common checks.** `Host` must be exactly one header equal to `127.0.0.1:<port>` or `localhost:<port>` (else 421, DNS rebinding; spike B-8). Where `Origin` is checked, it must be absent or equal to `http://127.0.0.1:<port>` or `http://localhost:<port>` (else 403). Bearer: exactly one `Authorization: Bearer` header (else 401).

**Route matrix** (public):

| Route | Host | Origin | Credential | Scope / object check |
|---|---|---|---|---|
| `GET /`, `/assets/*` (static PWA) | yes | — | none | serves no data; strict CSP (`default-src 'self'`, `connect-src` own origins) |
| `POST /api/pair` | yes | must be present and own | single-use pair code in body | issues an `owner` token |
| `POST /api/ws-ticket` | yes | absent or own | Bearer | ticket bound to the issuing token and the requested purpose (`events` or `attach:<thread>`) |
| `GET /api/ws` | yes | absent or own | ticket | subscriptions limited to streams the issuing token can read |
| `GET /api/attach/<thread>` (WS) | yes | absent or own | ticket with `attach:<thread>` | owner only |
| `GET /api/events`, `/api/threads`, `/api/worktrees`, `/api/adapters` | yes | absent or own | Bearer | stream read rule below |
| `GET /api/blobs/<sha256>?stream_id=<id>` | yes | absent or own | Bearer | stream read rule, and `(stream_id, sha256)` in `blob_refs` |
| `POST /api/commands/<kind>` | yes | absent or own | Bearer | action matrix below; receipt owned by the issuing token |
| `POST /hook` | yes | must be absent | Bearer `hook` token | the token's own launch only |
| `POST /events` | yes | must be absent | Bearer `extension` or `rescue` token | the token's scope; kinds `surface_update`, `raw_unknown` |

Tickets are single use, valid 30 s, and carry the issuing token's id and purpose; revoking the token invalidates its tickets and closes its open sockets. Blob responses carry the stored `Content-Type` only from an allowlist (`text/plain; charset=utf-8`, `application/json`) and otherwise `application/octet-stream`, plus `X-Content-Type-Options: nosniff`, `Content-Disposition: attachment`, and `Cache-Control: no-store`.

**Token kinds** (public: kinds, scopes, environment variable names; internal: storage):

| Kind | Holder | Lifetime |
|---|---|---|
| `owner` | the user's CLI (token file, 0600 / user-only ACL) and the paired PWA | until revoked |
| `agent` | a thread's CLI tree, as `CHALDEA_AGENT_TOKEN` | one launch (a relaunch gets a new one) |
| `rescue` | a thread spawned with `--role rescue`, as `CHALDEA_RESCUE_TOKEN`; that thread gets **no** agent token | until revoked |
| `hook` | the launch's hook emitter, as `CHALDEA_HOOK_TOKEN` (with `CHALDEA_HOOK_URL`, `CHALDEA_THREAD_ID`) | one launch |
| `extension` | one one-shot extension hook run, as `CHALDEA_EXTENSION_TOKEN` | one hook run |

All processes also get `CHALDEA_URL`. Every inherited `CHALDEA_*` variable is scrubbed before setting the launch's own (spike §4.3).

**Action matrix** (allow = ✓; object rule in the cell):

| Action | owner | agent | rescue | hook | extension |
|---|---|---|---|---|---|
| read stream / blob | ✓ all | itself and its descendants | ✓ all | — | — |
| spawn | ✓ | children of itself | — | — | — |
| send / key / stop / input resolve | ✓ | itself and its descendants | — | — | — |
| attach | ✓ | — | — | — | — |
| approval decision, attention clear | ✓ | — | — | — | — |
| task set | ✓ | itself and its descendants | — | — | — |
| mailbox send | ✓ | to its parent and its children | — | — | — |
| worktree remove | ✓ (`--force` allowed) | worktrees it owns, never `--force` | — | — | — |
| emit surface (`POST /events`) | ✓ | — | ✓ | — | its scope |
| `surface_action` | ✓, only for a surface whose scope includes the target | — | — | — | — |
| `POST /hook` | — | — | — | its launch | — |
| doctor results | ✓ | — | ✓ | — | — |

A command receipt is visible only to the token that created it; the same `command_id` from another token is a `Conflict` that reveals nothing. A hook token cannot bind as another launch: binding also requires the emitter ancestry rule (§2.9; prior-art row 6c).

**Secrets in logs and URLs.** The tracing layer redacts `Authorization`, the `ticket` query parameter, and pair codes; request logs never include bodies of `/hook` or `/api/pair`. The pair code travels in the URL fragment (`#pair=<code>`), which is never sent to the server; the page exchanges it via `POST /api/pair` and then removes it with `history.replaceState`.

**PWA pairing.** `chaldea open` starts the daemon if needed and opens `http://127.0.0.1:<port>/#pair=<code>` with a single-use code (60 s); the page stores the returned `owner` token in IndexedDB. Rejected alternative: an `HttpOnly` session cookie. It hides the token from page scripts but needs CSRF defences on every route, and the invariant names Bearer auth (AGENTS.md invariant 5). Single-origin serving keeps the page and API on one origin (arch §2.4).

Comparison: T3 Code stores role-bearing sessions with explicit `scopes` and pairing links with expiry and consumption (`Migrations/031_AuthAuthorizationScopes.ts:4`, `:12-48`) and compares secrets with `timingSafeEqual` (`apps/server/src/auth/utils.ts:83-89`); Chaldea adopts scoped tokens and single-use pairing for the same reasons. herdr's API uses local sockets with no token (`src/ipc.rs:10-11`, prior-art row 8); rejected, because Chaldea's API is HTTP and reachable by browsers.

Browser CF Access vs host Service Tokens (arch §2) belong to Phase 2 and are not built; the token-kind enum leaves room for them.

**Rejections on axum/hyper (B-8 carried over).** The spike's 22 B-8 cases are rerun against the daemon, split by layer:

| Layer | Cases | Mechanism | Expected client observation |
|---|---|---|---|
| Connection (hyper) | header section over 8 KiB; slow head past the header deadline; malformed framing (`Content-Length: +5`, duplicate differing `Content-Length`) | hyper `http1` header read timeout and buffer limit; hyper's framing parser | oversize and framing: 400 (RFC 9112 §6.3 requires 400 for an invalid `Content-Length`, so the spike's expected 411 for `+5` is replaced); slow head: connection closed within 5 s, a status is not required because the request never completed |
| Request, before body | bad/duplicate `Host`, missing/wrong/duplicate token, wrong method/path, any `Transfer-Encoding`, missing `Content-Length` on POST | middleware ordered Host → Origin → Bearer → scope → framing | 421 / 403 / 401 / 405 / 404 / 400 / 411 |
| Request, body | body over 4 MiB declared or sent; whole-request deadline (5 s) | `DefaultBodyLimit`, `tower` timeout | 413; 408 |

Every case also checks: no event, no fixture, no readiness change, and the case's own deadline. **Rejection delivery:** the decision is made before any body byte is used. For a rejected request that declared a body of at most 4 MiB, the handler then reads and discards that body (1 s bound) before writing the response, so no unread data remains when hyper closes the connection (the spike's Windows status-0 results came from closing with unread data, spike §3 B-8, inference). For 413 with a body sent up front, the expected observation is "413 or connection reset" and is recorded as such. The spike's two open Windows cases are thereby decided: `deadline_slow_head` expects a closed connection without side effect, `non_numeric_content_length` expects 400; both run on `windows-latest` in PR 9 and on the real host in PR 23.

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
| `owner_thread_id` | the owning thread; passes to the parent when a thread closes, to the human owner when there is no parent |
| `created_by` | `human` or `thread:<id>` |
| `state` | `creating`, `active`, `released` (no live owner), `removing`, `removed`, `failed` |

**Short base path.** `base_path` is configurable per workspace (`worktree_base` in the workspace config). Default: `<state_dir>/wt` (macOS `~/Library/Application Support/chaldea/wt`, Windows `%LOCALAPPDATA%\chaldea\wt`). `chaldea doctor` warns when `base_path` is longer than 40 characters on Windows, because MSVC `link.exe` fails beyond 260 characters (arch §5); a user then sets e.g. `C:\wt`.

**Crash-safe creation and removal.** The intent (`worktree_changed` with `state: creating` or `removing`, plus the receipt `in_progress`) commits before git runs. At daemon start, each `creating` or `removing` record is reconciled against `git worktree list --porcelain -z`: present → `active` (owner from the intent; `released` if that thread is gone); absent after `creating` → `failed`; absent after `removing` → `removed`; present after `removing` → `active`. Reconciliation never deletes anything.

**Never auto-delete.** The daemon never removes a worktree on its own: not on thread exit, close, daemon restart, or any timer. Removal happens only through `chaldea worktree remove <id>` and is refused unless all hold:

- no tracked or untracked changes (`git status --porcelain=v1 -z --untracked-files=all` empty);
- no ignored files, unless `--discard-ignored` is given (§8 Q7);
- no commit reachable from the worktree's `HEAD` (including a detached `HEAD`) or from its registered branch that is not on a remote-tracking ref: `git rev-list --max-count=1 HEAD <branch> --not --remotes` run in the worktree, the check T3 Code runs before forced removal (`apps/server/src/storageCleanup.ts:1004-1022`);
- no live launch has it as cwd.

Any git error, non-zero exit, or truncated output refuses the removal and leaves the record and the files unchanged. `--force` (owner token only, human-only in the PWA) overrides the content checks and is recorded with the evidence that would have refused it. The branch is never deleted, as in herdr (`src/worktree.rs:175-192`, test at `:822`); an explicit opt-in to delete it is a later, reversible addition. herdr relies on git's own dirty-check refusal (`src/worktree.rs:194-215`), which misses unpushed commits; Chaldea adds the unpublished check.

### 2.7 Adapter Capability Table

A const table in `crates/core`, keyed by `(agent, os)`, read by the daemon; no `cfg` decides it (spike F-1f). The table is internal; `GET /api/adapters` exposes a read-only summary (public type) so the UI can show what an adapter supports.

| Capability | Claude Code (macOS, Windows) | Codex CLI macOS | Codex CLI Windows | Evidence |
|---|---|---|---|---|
| Hook injection | `--settings <run>/claude-settings.json` + `--session-id <thread uuid>`; global settings untouched | `-c hooks.<Event>=[…]` with fixed command strings, fixed order; per-session URL/token/thread id in env | same | arch §5; prior-art rows 6a, 6b |
| Hook events | `SessionStart`, `UserPromptSubmit`, `Stop`, `SessionEnd`, `PermissionRequest`, `PreToolUse`, `PostToolUse`, `Notification` | `SessionStart`, `UserPromptSubmit`, `Stop`, `PermissionRequest`, `PreToolUse`, `PostToolUse` | same | event names: spike §3 (observed in binaries); the events beyond the spike's four are **unverified** until the fixture corpus (PR 12) |
| Session binding | predetermined `--session-id` | §2.9 | same | spike §4.3 |
| `session_start` | `eager` | `first_turn` | `first_turn` | arch §5 |
| First prompt | PTY input after `SessionStart` | positional argument, required at spawn (§8 Q5) | same | arch §5 |
| Input encoding | bracketed paste, only while DECSET 2004 is set; newline `cr` | bracketed paste; newline `cr` | `win_keys` (below) | arch §5; prior-art rows 4a, 4d; spike F-1f; macOS A-6 passed for both CLIs (spike status line) |
| Submit | one CR, 300 ms after the text write | one CR after 300 ms | one Return after 300 ms, only to a ready composer | prior-art row 4e; arch §5 |
| Ack | `UserPromptSubmit` | `UserPromptSubmit` | `UserPromptSubmit` | arch §4.3 |
| Extra launch flags | — | `--no-daemon` | `--no-daemon`, `-c tui.disable_paste_burst=true` | arch §5 |
| Resume | `--resume <session_id>` (**unverified** in the spike) | `codex resume <session_id>` (**unverified**) | same | PR 13 / PR 14 fixtures |
| Transcript | JSONL from the bound hook's `transcript_path` | hooks only when `transcript_path` is null; a non-null path is tailed for assistant text when `Stop` lacks it (format **unverified**, PR 12 fixtures) | same | arch §4.2, §5 |
| Hook trust | n/a | trust per source/event/position; record CLI version with trust state; a `Hooks need review` stall after an upgrade is expected | same | arch §5 open item; prior-art row 6b |
| Startup landmines | trust prompt defaults to "No, exit": never send Enter blindly | update, folder trust, hook review; digit keys pick options | same | arch §5 |
| Env scrub at spawn | `CLAUDECODE`, `CLAUDE_CODE_*`, `CODEX_THREAD_ID`, `CODEX_SESSION_ID`, other `CODEX_*` session variables, inherited `CHALDEA_*` | same | same | prior-art row 6c; spike §4.3 |

**`win_keys` encoding** (Codex on Windows; first MVP test, arch §5): reject ESC/C0 other than TAB/CR/LF; reject trimmable leading/trailing whitespace, TAB, and empty prompts (§8 Q6); NFC-compose each grapheme cluster holding a combining or extending character, keeping CJK compatibility ideographs; map CRLF/CR/LF to one newline; pad with one leading and one trailing space; send ASCII as UTF-8, **every other BMP scalar** as a win32-input-mode key-down + key-up record (`ESC[0;0;<utf16>;1;0;1_` + `ESC[0;0;<utf16>;0;0;1_`), astral scalars as UTF-8; each newline as a Shift+Enter record pair; every escape sequence inside one write; then one Return after 300 ms. The spike's `--unicode-records marks` mode sent records only for Mn/Me/Cf; the planned encoding widens that to every non-ASCII BMP scalar because U+2764 written as UTF-8 was lost (arch §5).

New prior-art evidence, not adopted: herdr keeps Codex's paste-burst detection on and appends one non-character key (Right) after the text on Windows, so Codex flushes the burst before the Enter (`src/app/api/agents.rs:14-33`, 300 ms submit delay at `:13`). It addresses the submit race but not the lost BMP characters, which Chaldea's encoding targets; it stays the fallback to try if PR 15's end-to-end test shows `tui.disable_paste_burst=true` is not enough.

### 2.8 Extension & Rescue Surface (Minimal)

- One-shot JSON hooks `before_spawn`, `after_close`, `attention`: a configured command gets one JSON document on stdin and `CHALDEA_URL`, `CHALDEA_EXTENSION_TOKEN`, `CHALDEA_THREAD_ID`; 10 s timeout; stdout is ignored except for `before_spawn`, whose optional `{ "decision": "deny", "reason" }` blocks the spawn. Failure policy of `before_spawn`: §8 Q4.
- `POST /events` (extension or rescue token): appends `surface_update` or `raw_unknown` with a caller idempotency key.
- A2UI surfaces: `{ surface_id, scope: { "kind": "host" } | { "kind": "workspace", "workspace_id" } | { "kind": "thread", "thread_id" }, components: [...] }` with the minimal catalog `text`, `markdown`, `vt_snapshot`, `button`, `text_input`, `list`. A `button` carries a `surface_action { action_id, kind: "send_input" | "send_key" | "stop" | "clear_attention" | "open_attach", args }` that runs only when a human with an owner token clicks it, through the command API with a `command_id`.

### 2.9 Launch Contract

A launch is one process lifetime of a thread. Before spawning, one transaction persists the launch intent: `launch_id`, thread, agent, argv digest, the hook token hash and agent token hash (scoped to this `launch_id`), and the command receipt `in_progress`. Only then is `PtyRuntime::spawn` called; `launch_started` records pid, start time, and the pgid (Unix) or job (Windows).

**Early hooks.** A hook can arrive before `launch_started` commits (Claude's `SessionStart` fires at startup). A request with a valid hook token whose launch has no recorded pid waits up to 2 s, inside the request deadline, for the pid; then the ancestry rule runs. Past that wait the event is kept as `raw_unknown` (`unbound`).

**Binding.** Claude binds by the predetermined `--session-id`. For Codex, hook processes run concurrently, so `UserPromptSubmit` can reach the daemon before that turn's `SessionStart`. Authenticated hooks of a launch that is not yet bound are **buffered** in the launch's pending set (at most 64 events or 4 MiB, up to 30 s); the first `SessionStart` whose emitter's nearest CLI ancestor is the launched pid binds the launch (`session_bound`), then the buffered events whose `session_id` matches are appended in their source order and the rest become `raw_unknown` (`foreign_session`). On overflow or timeout the buffered events become `raw_unknown` (`unbound`). The alternative, binding from the first hook of any type whose ancestor is the launched pid, was rejected: it keeps one binding rule per agent only if every Codex hook payload carries the same `session_id`, which is not verified for every event, while the `SessionStart` rule is the one the spike verified (spike §4.3, B-11).

**Daemon restart (crash-only).** On start, before accepting commands, the daemon handles every launch without `launch_exited`:

1. Confirm the old tree is dead. Windows: the daemon held the kill-on-close job, so the tree died with it; each recorded identity (pid + start time) is checked dead. Unix: the master PTY closed with the daemon, which hangs up the session (SIGHUP to its leader); the daemon then scans processes with `getsid` equal to the recorded session id and a start time between the launch's start and `meta.last_alive_at` (written every 5 s), and runs the stop sequence (HUP → TERM → KILL) on those identities (spike §4.6). A `setsid` grandchild whose parent already exited remains the known containment limit (spike §4.6) and is reported.
2. Record `launch_exited` (`daemon_restart`; `unknown_after_crash` when no pid had been recorded) and revoke the launch's hook and agent tokens.
3. Reconcile inputs and receipts (§2.4).
4. Leave the thread `exited`. A resume is an explicit relaunch (`chaldea resume <thread>` or the PWA): a new `launch_id` with new tokens, the same `session_id` passed to the CLI's resume flag, and the transcript cursor carried over unchanged (a changed file identity starts a new generation by the cursor rules).

Public: `launch_id` in events, `chaldea resume`. Internal: buffer bounds, wait bounds.

### 2.10 Approvals

An approval is a CLI-side permission dialog the user must answer. Every approval has a Chaldea `approval_id` and a correlation key: the CLI's own request id when its `PermissionRequest` payload carries one (candidate fields `tool_use_id` for Claude and a call id for Codex, both **unverified** until PR 12 fixtures), else `(launch_id, hook event seq)`.

- **Requested:** `PermissionRequest` → `approval_requested` with tool name and arguments; `turn_state` → `awaiting_approval`.
- **Resolved:** by the mechanism of §8 Q1. Under the hook-decision option, the decision returned to the CLI is the resolution. Under the keypad option, a card resolves on continuation evidence of the same launch after the request: `PostToolUse` with the same correlation key (`outcome: approved`), or a later `PreToolUse` / `UserPromptSubmit` / `Stop` (`outcome: unknown`).
- **Expired:** launch exit, or the hook's own timeout under the hook-decision option (the CLI then falls back to its own dialog).
- **Stale cards:** once any later turn event of the launch arrives, the card is marked stale and its buttons are disabled; a keypad action from a stale card is refused unless the user confirms against a fresh VT snapshot.

## 3. Workspace Layout

```
Cargo.toml                 # [workspace] members = crates/*; shared lints; exact-pinned deps (spike policy)
rust-toolchain.toml        # 1.96.0, as the spike
crates/core/               # Sans-IO: types, envelope, state reducers, input state machine, encoders, capability table,
                           # replay decision, receipt lookup, auth policy, verdict model. No tokio, no std::fs/net/process, no clock.
crates/pty-runtime/        # PtyRuntime trait, FakePty, unix (portable-pty =0.8.1), windows (own windows-sys ConPTY),
                           # alacritty_terminal VT responder and snapshot, process-tree kill, output drain
crates/daemon/             # tokio + axum + rusqlite: store, HTTP API, hook ingest, transcript tailer, supervisor, git
crates/cli/                # binary `chaldea`: client subcommands, `daemon run`, `attach`, `doctor`, `hook-emit`
crates/testkit/            # publish = false: `chaldea-fake-cli` binary and fixture helpers (FakeCli modes from the spike)
packages/contracts/        # generated TS (ts-rs) + JSON Schema (schemars) + runtime validators
packages/web/              # React + TypeScript strict + Vite + Tailwind PWA, built into the daemon's static assets
fixtures/                  # real-CLI contract corpus: fixtures/<agent>/<cli-version>/<case>/
fuzz/                      # cargo-fuzz targets (nightly, outside the stable workspace)
spikes/pty-hooks/          # unchanged until issue #1 removes it
```

**Dependency direction.** `core` ← `pty-runtime` (uses `core` types: `Key`, `Snapshot`, `InputChunk`) ← `daemon` ← `cli`. `testkit` depends on `core` only. `core` depends on `serde`, `serde_json` (with `raw_value`), `ts-rs`, `schemars`, `uuid` (parsing only), `unicode-normalization`, `unicode-segmentation`, `subtle`, `sha2`. Sans-IO is enforced: `crates/core/clippy.toml` disallows `std::fs`, `std::net`, `std::process`, `std::time::{Instant, SystemTime}::now`, `std::thread::spawn`, and CI fails on a `tokio` dependency in `core`.

**Single binary.** `chaldea hook-emit` is a subcommand of the one `chaldea` binary (arch §2). Its dispatch runs before any runtime or config initialization and uses std networking only, so a hook costs one process start. On Windows the binary is console-subsystem; the GUI-subsystem emitter is not needed (prior-art row 9, spike C-5 `go`).

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
| `src/paste.rs`, `src/marks.rs` | `core::input` | Lift the pure encoders (validate, newline mapping, escape-safe chunking, `compose_marks`, win_keys, lossy checks) and apply `unit_records` to every non-ASCII BMP scalar; the writer-driving `paste()` / `type_keys()` are rewritten in the daemon. Corpora move to tests. |
| `src/receiver.rs` | `daemon` | Rewrite on axum; keep the binding rules and the B-8 cases as tests (§2.5). |
| `src/emitter.rs`, `src/http_client.rs` | `cli` (`hook-emit`) | Lift (std networking, limits, exit-0 rules), add `X-Chaldea-Emit-Id` and the one retry (§2.1). |
| `src/session.rs` | `daemon` supervisor | Rewrite; lift `scrub_env_name` and the kill ordering. |
| `src/verdict.rs` | `core::verdict` | Lift the outcome model and change two rules (§7). |
| `src/fake_cli.rs` | `testkit` | Lift the modes used by tests (`echo`, `queries`, `stall`, `banner`, `attachable`, `spawn-tree`, `sleeper`). |
| `src/scenario/*`, `report.rs`, `cli.rs` | — | Not lifted (spike harness). |

Lifted code is copied, not moved: `spikes/` stays buildable and its CI job stays until issue #1.

**Tooling.** CI adds a `workspace` job (macOS + Windows matrix) next to the spike job: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`, the generated-contract drift check, and the `core` dependency check. `lefthook.yml` gets the same commands for the workspace. Node is added to `mise.toml` when `packages/` lands (PR 18), with pnpm; the JS job runs typecheck, Vitest (with fast-check), and Playwright.

## 4. PR Plan

Each PR keeps `core` + `FakePty` tests green on macOS and Windows CI. macOS is implemented first within each PR; the Windows code of the same PR lands in the same PR, not in a later architecture change.

| PR | Scope | Depends on | Milestone(s) closed or advanced |
|---|---|---|---|
| 1 | Workspace, CI job, `core` contracts: ids, envelope + forward-compatible decode, `Content`, kinds, 3-axis reducer (incl. relaunch), input state machine, `decide_replay`, `ReceiptLookup`, `ts-rs` + `schemars` export with drift check (§5) | — | Ingest (types), Sync (types), Verification (PBT for both machines) |
| 2 | `pty-runtime`: `PtyRuntime` trait, `FakePty`, single-writer channel, drain, VT responder + snapshot | 1 | PTY Seam (trait, FakePty, VT) |
| 3 | `pty-runtime::unix`: portable-pty `=0.8.1`, `setsid` + `killpg` HUP→TERM→KILL, `getsid` scan, recorded-pid fallback; `testkit` FakeCli | 2 | PTY Seam (macOS) |
| 4 | `pty-runtime::windows`: own ConPTY (flags 0, `kernel32.dll` only), JOB_LIST + `CREATE_SUSPENDED` fallback, job kill + recorded-descendant fallback, failure-path teardown | 2, 3 (`testkit`) | PTY Seam (Windows) — **closes PTY Seam** |
| 5 | `core::input`: bracketed paste encoder, escape-safe chunking, `win_keys` encoder (planned encoding), lossy checks; adapter capability table | 1 | Agent Control (capability table, encoders) |
| 6 | Daemon store: migrations, append-only triggers (incl. REPLACE test), atomic append with projections and transcript cursor precondition, idempotency, receipts with `in_progress`, blobs and `blob_refs`, drop-and-rebuild equality | 1 | Ingest (event log, cursor) |
| 7 | Daemon HTTP: axum on 127.0.0.1, route matrix and action matrix, tokens, pairing, `GET /api/events` (replay, snapshot, `before_seq`), blob route, WS ticket + notifications, command receipts over HTTP | 6 | Sync (API), Extension (token separation) |
| 8 | Daemon lifecycle: `chaldea daemon run/start/stop`, `DETACHED_PROCESS`, `last_alive_at`, **parent kill-on-close job check** | 7 | Agent Control (daemon) |
| 9 | Hook ingest: `POST /hook`, per-launch tokens, early-hook pid wait, Codex bind buffer, `raw_unknown`; `chaldea hook-emit` with `emit_id`; B-8 cases on the axum receiver on macOS and Windows CI (**Windows hook receiver** open item) | 7, 3 (`testkit`) | **Closes Ingest** |
| 10 | Supervisor: launch contract, `chaldea spawn/stop/wait/resume` with the FakeCli adapter, send path through `sending`, gate timeout, SessionStart timeout + VT snapshot, restart reconciliation (tree kill, receipts, inputs) | 3, 4, 5, 8, 9 | Agent Control (spawn/stop/wait) |
| 11 | `chaldea attach`: WS PTY stream, raw console on macOS and Windows, resize, detach keys, attach input contract (§4 note), attach-input attribution | 10 | Agent Control (attach) |
| 12 | Fixture corpus: layout, capture command (`chaldea doctor capture`) with typed verdict, contract test runner over `fixtures/`; Claude and Codex fixtures incl. `PermissionRequest`, `Stop`, Codex transcript | 9, 10 | Verification (corpus) |
| 13 | Claude adapter: settings file, session id, bracketed paste send path wired to the input state machine, transcript ingest, `tool_activity` dedupe, resume | 10, 12 | Agent Control (Claude) |
| 14 | Codex adapter: `--no-daemon`, fixed `-c hooks`, positional first prompt, `win_keys` path for `send` and attach paste, `tui.disable_paste_burst=true`, hook-trust stall with CLI version (**Codex hook trust renewal** open item), assistant text fallback, resume | 13, 11 | Agent Control (Codex) |
| 15 | **First tests 1 and 2** on a real Windows host: Codex keyed input through `chaldea send`, and human paste through `chaldea attach` (**Codex on Windows keyed input**, **human paste through attach** open items) | 14 | Cross-OS E2E (real-host Codex) |
| 16 | Mailbox, `agent` tokens per launch, `task_set`, worktree ownership (intent + reconciliation, `remove` checks, short base path) | 10 | Agent Control (mailbox, worktrees) — **closes Agent Control** with 11, 14 |
| 17 | `stateright` model of the Sync Seam (startup crash, offline buffering, retry after lost response) | 7 | Verification (model) |
| 18 | PWA read path: `packages/contracts` validators, `packages/web` served by the daemon, pairing, thread list/view via sync API + WS, history paging, `GenericEvent` rendering, client reducer (fast-check) | 7 | Sync & PWA (read) |
| 19 | PWA input: `<textarea>` with IME guards, send, input resolve, approvals (§2.10), VT snapshot view, emergency keypad; Playwright desktop + mobile, IME composition tests | 18, 13 | **Closes Sync & PWA** |
| 20 | Extension & Rescue: one-shot hooks, `POST /events`, rescue role and token, A2UI minimal catalog, `surface_action` | 16, 19 | **Closes Extension & Rescue** |
| 21 | `chaldea doctor` round-trip checks (§7) | 14, 16 | Cross-OS E2E (doctor) |
| 22 | `cargo-fuzz` targets (JSONL line parser, hook payload decoder, VT responder) + scheduled CI run | 5, 9, 13 | **Closes Verification** (with 12, 17) |
| 23 | Real-host verification run: §6.1 checklist on macOS and Windows, Claude + Codex, recorded with versions; additional Windows builds where available (**Windows build coverage** open item) | all | **Closes Cross-OS CI & E2E** |

**Critical path.** 1 → 6 → 7 → 9 → 10 → 12 → 13 → 14 → 15 (9 PRs). Off the path: 2 → 3 → 4 must finish before 10, 5 after 1, 8 after 7, 11 after 10 and before 14, 16–18 after their dependencies. Windows (PR 4) is a hard dependency of the supervisor (PR 10): the supervisor is not written until both backends exist behind the trait.

**Why the first tests are not earlier.** Both first tests are end to end by definition (arch §5, issue #2): they need the Windows backend, the encoder, hook ingest, attach, and the Codex launch path. A failure would change only the encoder and capability table in `core` (PR 5) and the attach paste path (PR 11), not a contract. PR 5 carries the encoder's unit and property tests on both OSes from the start, and PR 14 runs FakeCli-level attach paste tests on `windows-latest`, so the real-host run in PR 15 is the only late part.

**Attach input contract (decided in PR 11).** The attach client enables bracketed paste on its own outer terminal (`ESC[?2004h`; on Windows with `ENABLE_VIRTUAL_TERMINAL_INPUT`), so outer-terminal pastes reach it framed. It parses its input with a small state machine that reassembles a paste split across reads (bounded at 4 MiB and 2 s; on overflow or timeout the bytes are forwarded as plain input and the event is logged), and sends `Paste { text }` and `Bytes` frames. The daemon applies the adapter's encoding to `Paste` (bracketed paste when the child set DECSET 2004, `win_keys` for Codex on Windows, with lossy input forwarded as-is because a human is watching) and passes `Bytes` through. On detach, Ctrl-C, EOF, and panic the client restores the outer terminal's modes (bracketed paste off, raw mode off, original console mode).

**Open items of arch §5.**

| Open item | PR | Note |
|---|---|---|
| Codex on Windows keyed input | 15 (unit tests in 5) | First test 1. |
| Human pastes through `chaldea attach` to Codex on Windows | 15 (contract in 11, FakeCli tests in 14) | First test 2. |
| Codex hook trust renewal after upgrades | 14, 21 | Adapter records CLI version + trust state; doctor reports a version change since the last trusted launch. |
| Windows hook receiver (B-8 `deadline_slow_head`, `non_numeric_content_length`) | 9, 23 | Decided expectations in §2.5; rerun on `windows-latest` and on the real host. |
| Daemon parent job check | 8 | At daemon start on Windows: `IsProcessInJob` + `QueryInformationJobObject`; a kill-on-close parent job is reported and the daemon relaunches itself with `CREATE_BREAKAWAY_FROM_JOB` when allowed, else refuses to start in background mode with a message (prior-art row 8). |
| Windows build coverage | 23 | CI runners give one more Windows build for FakeCli-level ConPTY behavior; real-CLI runs on more builds need more hosts and stay open until then. |
| prior-art "Not Verified" items | 15, 23 or none | The ones that affect behavior (inbox conhost on other builds, BMP characters delivered only on Alt release) are covered by PR 15 and 23; the others (ureq proxy, VS Code, Zellij) do not affect the MVP and stay unverified. |

## 5. Detailed Design: PR 1

**Goal.** A Cargo workspace whose `crates/core` holds the irreversible contracts of §2.1–§2.4 as types and pure functions, generated TypeScript and JSON Schema committed with a drift check, and a CI job running on macOS and Windows.

**Files.**

```
Cargo.toml                                   # [workspace] members = ["crates/core"], resolver = "3",
                                             # [workspace.package] edition = "2024", license = "MIT OR Apache-2.0"
                                             # [workspace.lints] rust.unsafe_code = "forbid", clippy pedantic subset
rust-toolchain.toml                          # channel = "1.96.0", components = ["rustfmt", "clippy"]
crates/core/Cargo.toml                       # name = "chaldea-core"; deps exact-pinned: serde, serde_json (raw_value), ts-rs, schemars, uuid (serde)
                                             # dev-deps: proptest
crates/core/clippy.toml                      # disallowed-methods / disallowed-types (Sans-IO list, §3)
crates/core/src/lib.rs
crates/core/src/ids.rs                       # ThreadId, LaunchId, EventId, CommandId, InputId, ApprovalId (UUID newtypes), StreamId, Seq, Epoch
crates/core/src/event.rs                     # EventEnvelope, EventKindTag, NewEvent, Source, SCHEMA_VERSION, IdempotencyKey, Content, BlobRef
crates/core/src/payload.rs                   # payload structs per kind; TypedEvent, GenericEvent, TypedEvent::decode
crates/core/src/state.rs                     # ProcessState, TurnState, TaskState, ThreadAxes, StateInput, reduce()
crates/core/src/input.rs                     # InputState, InputInput, step()
crates/core/src/sync.rs                      # EventsQuery, EventsResponse, ReplayPlan, decide_replay(), WsServerMsg, WsClientMsg
crates/core/src/command.rs                   # CommandEnvelope<T>, CommandReceipt, ReceiptStatus, ReceiptLookup, check_receipt()
crates/core/tests/contracts_export.rs        # generates TS + JSON Schema into a temp dir and diffs with committed files
crates/core/tests/golden/                    # golden JSON for one event of each kind
packages/contracts/src/generated/            # committed ts-rs output
packages/contracts/schema/                   # committed schemars output
.github/workflows/ci.yml                     # new `workspace` job (matrix macos-latest, windows-latest)
lefthook.yml                                 # workspace fmt / clippy (pre-commit), test (pre-push)
```

`packages/contracts` gets no `package.json` until PR 18; the generated files exist from PR 1 so their drift is checked from the first contract.

**Types and functions** (signatures; names may be adjusted in review, shapes may not):

```rust
pub const SCHEMA_VERSION: u32 = 1;

pub enum StreamId { Host, Thread(ThreadId) }                  // serialized as "host" / "thread:<uuid>"

pub struct EventEnvelope {                                     // wire form; no #[serde(flatten)]
    pub stream_id: StreamId, pub seq: u64, pub event_id: EventId, pub schema_version: u32,
    pub idempotency_key: IdempotencyKey, pub kind: EventKindTag,
    pub thread_id: Option<ThreadId>, pub launch_id: Option<LaunchId>, pub command_id: Option<CommandId>,
    pub source: Source, pub occurred_at: Timestamp, pub recorded_at: Timestamp,
    pub payload: Box<serde_json::value::RawValue>,
}
pub enum TypedEvent { ThreadRegistered(..), ThreadClosed(..), LaunchStarted(..), LaunchExited(..), SessionBound(..),
    UserMessage(..), AssistantMessage(..), ToolActivity(..), StatusChanged(..), ApprovalRequested(..),
    ApprovalResolved(..), InputQueued(..), InputStateChanged(..), MailboxMessage(..), TaskSet(..), ArtifactDiff(..),
    SurfaceUpdate(..), VtSnapshotTaken(..), WorktreeChanged(..), RawUnknown(..) }
pub struct GenericEvent { pub envelope: EventEnvelope, pub why: GenericReason }   // UnknownKind | NewerSchema | PayloadInvalid
impl TypedEvent { pub fn decode(e: &EventEnvelope) -> Result<TypedEvent, GenericEvent>; pub fn encode(..) -> NewEvent; }
pub enum Content { Inline { inline: String }, Blob { blob: BlobRef, preview: String } }

impl IdempotencyKey {
    pub fn hook(emit_id: Uuid) -> Self; pub fn transcript(generation: u64, offset: u64) -> Self;
    pub fn tool(launch: LaunchId, tool_use_id: &str, phase: ToolPhase) -> Self;
    pub fn launch(launch: LaunchId, what: LaunchMark) -> Self; pub fn command(id: CommandId, n: u32) -> Self;
    pub fn derived(cause: &IdempotencyKey, name: &str) -> Self; pub fn extension(caller_key: &str) -> Result<Self, KeyError>;
}

pub enum StateInput {                                          // no variant carries PTY output
    Registered { task: bool }, Relaunched, Spawned, SpawnFailed, SessionBound { eager: bool }, StopRequested,
    Exited { reason: ExitReason }, PromptSubmitted, TurnStopped, ApprovalRequested, ApprovalResolved,
    AttentionRaised { reason: AttentionReason }, AttentionCleared, TaskSet { to: TaskState, by: Actor },
}
pub struct ThreadAxes { pub process: ProcessState, pub turn: TurnState, pub task: TaskState }
pub fn reduce(axes: ThreadAxes, input: &StateInput) -> Result<(ThreadAxes, Vec<StatusChanged>), InvalidTransition>;

pub enum InputState { Queued, Sending, Sent, Acked, Unconfirmed, Rejected, Cancelled, Dismissed }
pub enum InputInput { GateOpened, WriteCompleted, WriteOutcomeUnknown, Rejected { reason: RejectReason },
    Cancelled, AckAttributed, AckNotAttributable, AckWindowExpired, DaemonRestarted, HumanResolved { delivered: bool } }
pub fn step(state: InputState, input: &InputInput) -> Result<InputState, InvalidTransition>;

pub fn decide_replay(q: &EventsQuery, head: StreamHead, epoch: &Epoch, retained_from: u64) -> ReplayPlan;
    // StreamHead { seq, cum_bytes, cum_bytes_at_after_seq }; ReplayPlan = Replay { after, through } | Snapshot { reason }

pub enum ReceiptStatus { InProgress, Completed, Failed, UnknownAfterCrash }
pub enum ReceiptLookup { New, InProgress, Replay(CommandResult), Conflict, UnknownAfterCrash }
pub fn check_receipt(existing: Option<&CommandReceipt>, token: TokenId, request_sha256: &[u8; 32]) -> ReceiptLookup;
```

**Tests** (all run on macOS and Windows):

- `state` property tests (`proptest`): for any sequence of `StateInput`, `reduce` never panics; `task` changes only on `TaskSet` and is unchanged by `Relaunched` and `Exited`; after `Exited`, `turn` is `unknown`; every change is reported in exactly one `StatusChanged`.
- `input` property tests: an input reaches `Sending` at most once; nothing leads from `Sending`, `Sent`, or `Unconfirmed` back to `Queued` or `Sending`; `Rejected` and `Cancelled` are reachable only from `Queued`; `DaemonRestarted` maps `Sending`/`Sent` to `Unconfirmed` and `Queued` to `Cancelled`; `Dismissed` and human `Acked` are reachable only from `Unconfirmed`.
- `decide_replay` table tests for every fallback reason, `after_seq = retained_from - 1`, `after_seq = head`, empty stream, and absent epoch.
- `check_receipt`: none → new; in progress → in progress; same token and hash → replay; other hash or other token → conflict; unknown after crash → unknown.
- Decoding: an unknown `kind`, a higher `schema_version`, and an invalid payload each give `GenericEvent` with the right reason; golden JSON round trip for one event per kind, `Content` both forms, `EventsResponse` both modes, `WsServerMsg`, `WsClientMsg`; the generated TS for `EventEnvelope` and `Content` is checked by eye in review once and then by drift.
- `contracts_export`: generated TS and JSON Schema equal the committed files (fails with the path of the first differing file; regenerating is one documented command, `cargo test -p chaldea-core --test contracts_export -- --ignored regenerate`).

**Completion commands.**

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
! cargo tree -p chaldea-core -e normal --prefix none | grep -q '^tokio '
```

plus the `workspace` job green on `macos-latest` and `windows-latest`, and the existing `pty-hooks` job unchanged and green.

## 6. Purpose Layer and Plan Layer

Purpose rows are `P-n`; plan claims are `CL-n` (so they do not collide with the spike's set C probe ids `C-1`…`C-5`).

### 6.1 Purpose Layer: Acceptance Criteria

Checked on the real thing (real hosts, real CLIs, the built `chaldea` binary and PWA). "Owner evaluation" marks subjective items; "unverified" marks items with no means of checking in the MVP.

| ID | Criterion (EARS) | Source | Scene | Observable result / failure | How to check on the real thing |
|---|---|---|---|---|---|
| P-1 | When a user runs `chaldea spawn --agent claude` in a trusted repository, Claude Code shall run in a daemon-owned PTY with the user's own skills, hooks, and permission settings working, and global hook settings unchanged. | arch §1 native fidelity; issue #2 | macOS and Windows host | Pass: `chaldea attach` shows the native TUI; a user skill and `/` command work; `~/.claude/settings.json` hooks unchanged. Fail: a feature missing, a settings diff. | Real Claude on both hosts; diff settings before/after. |
| P-2 | When a user runs `chaldea spawn --agent codex --prompt …`, Codex shall run with `--no-daemon`, its hooks shall deliver without a review dialog after one initial trust, and its MCP servers shall stay inside the launched tree. | arch §5 | both hosts | Pass: bound `SessionStart`, no review dialog on relaunch, MCP server pid under the CLI; the report lists registered hook events separately from events observed firing (spike A-5). Fail: review dialog, MCP server parented by a shared daemon. | Real Codex, 3 launches; process tree listing. |
| P-3 | When `chaldea stop` is run for a thread, no process of its tree (including MCP servers) shall be alive 5 s later, and an unrelated process shall survive. | arch §4.5; spike A-8, C-3 | both hosts | Pass: empty survivor list, with the MCP witness attested before stop (spike A-8; no attestation = inconclusive). Fail: survivor. | Real CLIs with an MCP sleeper; process listing after stop. |
| P-4 | When an orchestrator thread spawns a child with `--worktree` and waits for it, the orchestrator shall receive the child's 3-axis state and mailbox messages, and the child's worktree with uncommitted changes shall still exist after the child exits and after a daemon restart. | arch §1 orchestration | macOS host, real Claude as orchestrator | Pass: `chaldea wait` returns, `chaldea mailbox` lists the message, worktree files present. Fail: lost files, missing outcome. | Scripted run with real CLIs; check the worktree after restart. |
| P-5 | If removal is requested for a worktree with uncommitted changes, ignored files, commits not on any remote-tracking ref (on its branch or a detached `HEAD`), or a git error, then Chaldea shall refuse and leave the worktree and branch unchanged. | arch §1, AGENTS.md invariant 3 | both hosts | Pass: refusal with the reason; files and branch intact. Fail: anything removed. | Real repo per case. |
| P-6 | When a prompt containing Japanese, emoji, decomposed marks, and newlines is sent to Claude Code through `chaldea send` or the PWA, the prompt Claude records shall equal the expected form (Claude's documented transforms applied). | arch §1 IME; invariant 4 | both hosts | Pass: `user_message` equals the expected form. Fail: any other difference. | Real Claude with the spike corpus; compare events. |
| P-7 | When the same corpus is sent to Codex on Windows through `chaldea send`, each prompt shall arrive as exactly one `UserPromptSubmit` equal to the expected form, followed by that turn's `Stop`. | arch §5 first test; issue #2 | Windows host | Pass: one submit per prompt, equal, with no second submit in the 5 s settle interval after `Stop` (spike F-1a). Fail: split, joined, marks lost, early or second submit. | PR 15 run on a real Windows host, recorded with Codex version. |
| P-8 | When the corpus is sent to Codex on macOS through `chaldea send`, each prompt shall arrive as one `UserPromptSubmit` equal to the expected form. | arch §1 IME; spike A-6 | macOS host | Pass: equal. Fail: any difference. | Real Codex on macOS. |
| P-9 | When a human pastes multi-line Japanese text through `chaldea attach` into Codex on Windows, the paste shall arrive as one prompt with its newlines and shall not submit or queue before the human presses Enter. | arch §5 open item; issue #2 | Windows host, Windows Terminal as outer terminal | Pass: one prompt, submitted only on the human's Enter. Fail: early submit, Tab queueing, lost newlines. | PR 15 manual run. |
| P-10 | When a user attaches to a thread that ran without a viewer, the attach view shall show the live screen, follow a terminal resize, and on detach leave the thread running and the outer terminal restored. | arch §4.5; spike C-4 | both hosts | Pass: current screen, resized layout, thread still `running`, outer terminal usable. Fail: stale screen, dead thread, terminal left in raw or paste mode. | Real CLI, attach from a second terminal. |
| P-11 | While composing Japanese in the PWA prompt input, pressing Enter to confirm a conversion shall not send the prompt. | arch §1, §2.4; invariant 4 | macOS (Kotoeri / Google IME), Windows (MS-IME) desktop browsers | Pass: no send during composition. Fail: send on conversion Enter. | Manual with real IMEs; Playwright covers the event sequence. Android IME: unverified in the MVP (no remote access until Phase 2). |
| P-12 | When a CLI stalls on a startup prompt (trust, update, hook review) and no `SessionStart` arrives within the timeout, the thread shall show `needs_attention` with a legible VT snapshot (including CJK wide characters), and the keypad shall clear the prompt. | arch §1 self-healing, §2 landmine | both hosts, fresh untrusted folder | Pass: snapshot shows the prompt, keypad keys reach the CLI, thread proceeds. Fail: no attention, keys lost. | Real CLIs in a fresh folder; legibility is owner evaluation. |
| P-13 | When a CLI queries the terminal at startup (DSR, DA, OSC 10/11, kitty keyboard), it shall get an answer and not stall. | arch §4.5; spike B-5, C-2 | both hosts | Pass: CLI reaches `SessionStart`. Fail: a stall with an unanswered query in the log. | Real CLIs; query log on both hosts. |
| P-14 | If a hook payload or transcript line cannot be parsed or bound, then Chaldea shall keep it verbatim as `raw_unknown` with the CLI version, visible in the thread or host stream. | arch §1 self-healing | both hosts | Pass: event present with raw bytes. Fail: dropped. | Malformed payload with a valid hook token; nested CLI case. |
| P-15 | If an input is not acknowledged within the ack window, then Chaldea shall mark it `unconfirmed`, raise attention, and never send it again on its own. | arch §4.3 | both hosts | Pass: exactly one write in the PTY log; state `unconfirmed`. Fail: a second submit. | Real CLI with a dialog open over the composer. |
| P-16 | When an approval is requested by the CLI, the PWA shall show it in the thread and let the user resolve it; a card that the CLI no longer waits on shall show as stale. | issue #2 PWA; arch §1 | both hosts | Pass: card appears; resolution reaches the CLI; stale card disabled. Fail: no card, decision lost, action on a stale card. | Real Claude tool permission (mechanism per §8 Q1). |
| P-17 | When a Codex or Claude turn ends, the thread shall show the assistant's final message. | arch §1 (reading threads) | both hosts | Pass: `assistant_message` present. Fail: missing. If neither the `Stop` payload nor a transcript carries it for a CLI version, the row fails for that version and is reported, not filled from the screen. | Real CLIs; fixtures per version. |
| P-18 | When the daemon is killed and restarted, every thread that was running shall show `exited` (`daemon_restart`) with its history intact and no surviving old process, and a resume shall continue the CLI session. | arch §2 crash-only | both hosts | Pass: history equal, no old-tree survivor, resumed session remembers context. Fail: lost or duplicate events, survivor, resume fails. | Kill the daemon by PID mid-turn; restart; resume. |
| P-19 | When the PWA reconnects after the daemon was unreachable, it shall show the time of last sync while offline and catch up without duplicate or missing events, and older history shall stay readable. | arch §4.1 | desktop browser | Pass: banner while offline, event list equals the server's after; scrolling back loads older events. Fail: duplicates, gaps, unreachable history. | Stop and start the daemon while the PWA is open. |
| P-20 | If a request has a wrong `Host`, a missing or wrong token, or a scope it lacks (e.g. Rescue sending input, an agent reading an unrelated thread or its blobs), then the daemon shall reject it and change nothing. | AGENTS.md invariant 5 | both hosts | Pass: 421/401/403/404, no event. Fail: accepted. | `curl` probes against the real daemon. |
| P-21 | When a tool output larger than 64 KiB is recorded, the thread shall show its preview and offer the full content as a download. | arch §4.2 | both hosts | Pass: preview shown; download equals the output. Fail: truncated silently, page stalls. | Real CLI producing a large output. |
| P-22 | When a configured `before_spawn`, `after_close`, or `attention` hook runs, it shall receive the documented JSON, and events it posts to `POST /events` shall appear in the scoped stream. | arch §4.4 | macOS host | Pass: hook log shows the input; surface visible. Fail: hook not run, event missing or in the wrong scope. | A sample hook script. |
| P-23 | When a Rescue Agent emits a Thread-scoped A2UI panel with a button, the button's action shall run only after a human clicks it, and only on that thread. | arch §1, §4.4 | macOS host | Pass: action event with the human's command id. Fail: action without a click, wrong thread. | Real Claude as Rescue Agent. |
| P-24 | Where a workspace sets a short worktree base path on Windows, a Rust project with MSVC shall build inside a Chaldea worktree. | arch §5 MSVC | Windows host | Pass: `cargo build` succeeds; doctor shows no path warning. Fail: `LNK1104`. | Real repository under `C:\wt`. |
| P-25 | While Chaldea spawns CLIs and runs hooks on Windows, no console window shall appear. | prior-art row 9; spike C-5 | Windows host | Pass: no visible window. Fail: a flash. | Owner evaluation by watching the desktop (the spike's observer is not lifted). |
| P-26 | When `chaldea doctor` runs, it shall report each check as pass, fail, or inconclusive with evidence, and report inconclusive (never pass) for any check it could not run. | arch §2 landmine, §6 | both hosts | Pass: report matches the host's state, including a deliberately broken check. Fail: pass without evidence. | Run on both hosts, once with the hook port blocked. |
| P-27 | Chaldea shall be usable on a phone-sized viewport for reading threads, short prompts, approvals, and the keypad. | arch §1 | mobile viewport in a desktop browser | Layout usability is owner evaluation; real Android access is unverified until Phase 2. | Playwright mobile viewport screenshots + manual review. |

### 6.2 Plan Layer: Claims and Falsifiers

| ID | Claim (EARS) | Source | Test that would falsify it |
|---|---|---|---|
| CL-1 | The thread state reducer shall accept only typed explicit inputs, none built from PTY output. | invariant 1; §2.2 | A `StateInput` variant holding PTY bytes or text (review of PR 1 types); `reduce` proptest. |
| CL-2 | After registration, `task_state` shall change only on `task_set`; relaunch and exit shall keep it. | §2.2 | PR 1 proptest finding a change on another input. |
| CL-3 | An input shall reach `sending` at most once and never leave `sending`, `sent`, or `unconfirmed` for `queued`. | arch §4.3; §2.3 | PR 1 proptest; PR 10 PTY write count; PR 13/14 PTY log showing a second submit. |
| CL-4 | A daemon restart shall turn `sending`/`sent` inputs into `unconfirmed`, `queued` inputs into `cancelled`, and `in_progress` receipts into `unknown_after_crash`, with no PTY write. | §2.3, §2.4 | PR 10 kill-and-restart test with FakeCli. |
| CL-5 | Concurrent requests with one `command_id` shall cause at most one effect, and a repeat shall return the stored result. | arch §4.1 | PR 7 receipt tests over HTTP; PR 10 concurrent `send` / `key` test counting PTY writes. |
| CL-6 | An ack shall be attributed to an input only by stream position within the same launch and only when no attach input intervened. | §2.3 | PR 10/11 test: attach typing between write and `UserPromptSubmit` leaves the input `unconfirmed`. |
| CL-7 | A `queued` input that cannot pass its gate shall raise attention with a snapshot after the gate timeout, with zero bytes written. | §2.3 | PR 10 FakeCli that never sets DECSET 2004. |
| CL-8 | Each stream's `seq` shall be gapless and monotonic under concurrent appends and crash recovery. | §2.1 | PR 6 concurrent append test; PR 17 model. |
| CL-9 | A replayed source (hook retry with one `emit_id`, transcript reread) shall not create a duplicate event; distinct `emit_id`s shall each create one. | §2.1 | PR 6 key test; PR 9 HTTP test of both cases. |
| CL-10 | Projections rebuilt from events shall equal the incrementally maintained projections. | §2.1 | PR 6 drop-and-rebuild test. |
| CL-11 | No statement shall update or delete an event, including `INSERT OR REPLACE`. | §2.1 | PR 6 trigger tests. |
| CL-12 | Transcript events and the cursor shall commit atomically, only from a path given by the bound session, and only when the stored cursor still matches. | arch §4.2; §2.1 | PR 6/13 fault injection between batch and cursor write; stale-cursor and swapped-file tests. |
| CL-13 | A client following `decide_replay`, `before_seq` paging, and subscribe-time heads shall end with the server's events for the stream, without duplicates, after any sequence of disconnects, restarts, and lost responses. | arch §4.1, §6 | PR 7 edge-case list (§2.4); PR 17 model; PR 18 fast-check client reducer. |
| CL-14 | Requests with a bad Host, Origin, token, scope, framing, or size shall be rejected before any body byte is used, the client shall observe the expected status (or the recorded alternative), and no event shall be created. | invariant 5; spike B-8; §2.5 | PR 9 B-8 cases on macOS and Windows CI. |
| CL-15 | Each route shall enforce the route and action matrices, including object-level checks for streams, blobs, receipts, attach, and `surface_action`. | §2.5 | PR 7 matrix tests per token kind; PR 20 rescue scope tests. |
| CL-16 | Secrets (Bearer tokens, tickets, pair codes) shall not appear in logs or in the PWA URL after pairing. | §2.5 | PR 7 log capture test; PR 18 Playwright URL check. |
| CL-17 | A hook from a nested CLI inheriting the environment shall not bind as the parent thread, and Codex hooks arriving before `SessionStart` shall be appended after binding in source order. | prior-art row 6c; spike B-11; §2.9 | PR 9 FakeCli `echo --nested`; out-of-order delivery test. |
| CL-18 | After a daemon restart, no process of a pre-restart launch shall be alive when a resume starts, and the old launch's tokens shall be rejected. | §2.9 | PR 10 restart test with FakeCli `spawn-tree` on both OSes. |
| CL-19 | The bracketed paste encoder shall write zero bytes for ESC or C0 controls other than TAB/CR/LF, and never split an escape sequence across writes. | prior-art row 4b; spike F-3d | PR 5 unit + proptest; PR 22 fuzz. |
| CL-20 | The `win_keys` encoder shall produce records for every non-ASCII BMP scalar, UTF-8 for ASCII and astral scalars, one Shift+Enter record pair per newline, and the spike's composition and padding rules. | arch §5 | PR 5 unit tests over the spike's corpus F; PR 15 real-host run. |
| CL-21 | The input encoding shall be chosen from the capability table by `(agent, os)`, never by DECSET 2004. | spike F-1f | PR 5 table test; PR 13 Claude-on-Windows uses bracketed paste. |
| CL-22 | Bracketed paste shall be written only while the CLI has DECSET 2004 set and the thread is bound and idle. | prior-art row 4a | PR 10 FakeCli without 2004: input stays `queued`. |
| CL-23 | The attach client shall deliver an outer-terminal paste split across reads as one paste, encoded by the adapter, and restore the outer terminal's modes on every exit path. | §4 attach input contract | PR 11 split-frame and exit-path tests; PR 14 Windows FakeCli paste test. |
| CL-24 | The VT responder shall answer the queries conhost forwards and all startup queries on macOS, in one write each, and snapshots shall place wide characters on two cells. | arch §4.5; spike B-5, F-3e | PR 2 FakePty tests; PR 3/4 FakeCli `queries` and `stall` grid. |
| CL-25 | Stop shall leave no owned process alive on macOS (`killpg` + `getsid` + fallback) and Windows (job + recorded descendants). | arch §4.5; spike B-7, C-3 | PR 3/4 FakeCli `spawn-tree`; PR 23 real CLIs with MCP witness. |
| CL-26 | The Windows backend shall launch on the JOB_LIST branch on supported builds and leave no process or handle behind on every injected setup failure. | prior-art rows 1b, 1d; spike F-2c | PR 4 Windows CI with injections; handle count. |
| CL-27 | The daemon shall detect a kill-on-close parent job on Windows at startup. | arch §5 open item; prior-art row 8 | PR 8 test starting the daemon inside a kill-on-close job on Windows CI. |
| CL-28 | `core` shall build and pass its tests without tokio, filesystem, network, process, or clock access. | §3 Sans-IO | CI dependency check; clippy disallowed list. |
| CL-29 | Generated TS and JSON Schema shall equal what the Rust types generate, and readers shall render unknown kinds and newer schema versions generically. | §2.1 | PR 1 drift and decode tests; PR 18 PWA test with an unknown kind. |
| CL-30 | No code path shall remove a worktree except the explicit remove command passing the checks or `--force` with an owner token, and a crash during create or remove shall reconcile without deleting. | arch §4.3; §2.6 | PR 16 tests: exit, close, restart leave the worktree; each refusal case; agent `--force` refused; crash between `git worktree add` and the record. |
| CL-31 | Content above 64 KiB shall be stored as a blob with a preview, and blobs shall be served only through a stream that references them, as attachments with `nosniff`. | §2.1, §2.5 | PR 6 offload test; PR 7 blob route tests. |
| CL-32 | One-shot extension hooks shall receive their documented input within the timeout, and `POST /events` shall accept only the token's scope and kinds. | arch §4.4 | PR 20 hook and route tests. |
| CL-33 | The PWA prompt input shall not submit while `isComposing`, on `keyCode === 229`, or on the Enter right after `compositionend`. | invariant 4 | PR 19 Playwright composition event tests (desktop + mobile viewports). |
| CL-34 | A hook emitter run shall exit 0, write nothing to stdout, and finish within 3 s whatever the daemon does. | spike B-9 | PR 9 emitter tests (daemon down, slow, rejecting). |
| CL-35 | A Rescue token shall not cause input, key, stop, attach, or worktree removal. | arch §4.4 | PR 20 scope tests. |
| CL-36 | An approval card shall resolve or go stale only on explicit events of the same launch. | §2.10 | PR 13 FakeCli `PermissionRequest` followed by `PostToolUse` / `Stop`; PR 19 UI test. |

## 7. Verification Plan

| arch §6 item | What | PR |
|---|---|---|
| Real-CLI contract corpus (`fixtures/`) | `fixtures/<agent>/<cli-version>/<case>/` holding hook payloads (`<Event>-<n>.json` + `.meta.json` with allowlisted fields, as spike §4.3) and transcript excerpts, captured by `chaldea doctor capture`, scrubbed of tokens, home paths, and prompts other than the corpus. Contract tests decode every fixture through the adapter decoders; an unknown field is fine, a known event that decodes to `raw_unknown` fails. The runner fails when a declared fixture set is empty. New CLI versions add a directory; old ones stay. | 12 (layout, capture, runner, first fixtures); 13, 14 (adapter decoders) |
| PBT (`proptest` / `fast-check`) | 3-axis reducer, input state machine (PR 1); encoders (PR 5); projection rebuild (PR 6); client reducer against generated event sequences (PR 18, fast-check). | 1, 5, 6, 18 |
| `stateright` | Scoped to the Sync Seam: startup crash during append, offline buffering in the client, retry after a lost command response. Actors: store, daemon API, client. Properties: no gap, no duplicate, one effect per `command_id`. | 17 |
| `cargo-fuzz` | JSONL line parser (transcript), hook payload decoder (per adapter), VT escape responder (arbitrary bytes, replies checked for one-write sequences). Nightly, scheduled CI, corpus seeded from `fixtures/`. | 22 |
| Cross-OS CI | macOS + Windows matrix for the workspace job from PR 1; FakePty and FakeCli tests in every PR; Windows backend injections in PR 4. | 1 onward |
| `chaldea doctor` | Round trip on the real host: daemon reachable with valid Host/token; hook round trip through `chaldea hook-emit`; FakeCli spawn in a PTY, VT query reply, paste echo, tree kill; Windows parent job; ConPTY module is `kernel32` (checked after that real PTY spawn, since the module loads then); worktree base path length; CLI versions and Codex hook trust state vs last trusted launch. | 21 |
| Playwright | Desktop and mobile viewports; IME guards via synthetic `compositionstart/update/end` and `keyCode 229` sequences; send, approval, keypad flows against a daemon running FakeCli. | 18, 19 |
| Real-host E2E | §6.1 checklist on macOS and Windows hosts with versions recorded. | 15, 23 |

**Test obligations per PR** (the PR's own design writes the cases): PR 6 drop-and-rebuild, REPLACE, stale cursor; PR 7 the §2.4 edge-case list and the §2.5 matrices; PR 9 B-8 table, emit-id retry, Codex out-of-order binding; PR 10 restart reconciliation, concurrent duplicate commands, gate timeout; PR 11 split pastes and terminal restore; PR 13 approval resolution, `tool_activity` dedupe; PR 16 worktree refusal and crash cases.

**Verdicts of evidence-collecting harnesses.** `chaldea doctor`, `chaldea doctor capture`, the fixture runner, and the real-host runs of PR 15 and 23 decide their verdict in one function, `core::verdict::evaluate(plan: &CheckPlan, checks: &[Check]) -> Verdict`, lifted from the spike (`spikes/pty-hooks/src/verdict.rs`). Each check's outcome is typed: `Pass`, `PlatformFail { cause }` (only with positive evidence of what the CLI or OS did), `HarnessFail { cause }` (setup or observation failure), `Missing` (no result), `NotExercised { reason }`. Rules:

- The plan (required check ids and the expected context: run id, launch id, OS build, CLI version) is fixed before observation starts. An empty required set is `Inconclusive`.
- A check whose context does not match the plan, or whose evidence is null, is `HarnessFail`.
- Two observations of one check with different outcomes make the verdict `Inconclusive`. This replaces the spike's "last check per key wins" (`verdict.rs:116-135`); checks outside the plan are recorded but do not affect the verdict, which also differs from the spike.
- Completeness first: any required check `Missing`, `HarnessFail`, or `NotExercised` makes the verdict `Inconclusive`; otherwise `Pass` when all passed, else `Fail`. Missing evidence, an observation failure, or a check that did not run is never `Pass` (spike §1).

Doctor's JSON report carries every check with its evidence, context, and the verdict.

## 8. Open Questions

Only items that change public behavior or are irreversible. Everything else is decided above with its reason.

**Q1. How approvals are answered.**

| Option | Gains | Loses | Reversibility | Cost |
|---|---|---|---|---|
| A. Hook decision: for `PermissionRequest`, `chaldea hook-emit` waits (long poll, bounded by the hook timeout) for the user's decision and prints the CLI's decision JSON on stdout. Request id: the CLI's id if the payload has one, else `(launch, seq)` (§2.10). Resolution: the returned decision. Expiry: hook timeout or launch exit; the CLI then shows its own dialog and the card goes stale. | A real Approve/Deny button; no keys sent blind; exact resolution evidence. | Changes the emitter contract (it now writes stdout and blocks); depends on each CLI's hook decision schema (Claude: documented in its hooks docs, not verified in the spike; Codex: **unverified**). | Medium: the emitter command string is part of Codex's trust key, so changing it later re-triggers `Hooks need review` (arch §5). | Fixture work per CLI version. |
| B. Keypad only: the card shows the hook's tool and arguments plus the VT snapshot; the user answers with keypad keys. Resolution: continuation evidence (`PostToolUse` with the same key = approved; a later turn event = unknown outcome). Expiry: launch exit. Stale card when any later turn event arrives. | No emitter change; works for every CLI. | Reads the dialog from the snapshot; outcome "unknown" when the tool was denied; slower on a phone. | High. | Low. |

Recommendation: B in the MVP for both CLIs, and A for Claude once the fixture corpus shows its decision schema; for Codex, A needs a hook command string that stays fixed, so its design must be settled before Codex hooks ship to users. Changes if: the owner requires one-tap approval in the MVP, in which case A for Claude goes into PR 13 behind the fixture test.

**Q2. CLI surface for `task_state`.**

| Option | Gains | Loses | Reversibility | Cost |
|---|---|---|---|---|
| A. `chaldea task set <state> [--summary …] [--thread …]` | Explicit and greppable in agent instructions. | One more command than issue #2 lists. | CLI flags are public; adding is easy, removing is not. | Low. |
| B. `chaldea mailbox send --task-state <state>` | No new command. | Couples a state change to a message; a state change without a message is awkward. | Same. | Low. |

Recommendation: A. Changes if: the command list of issue #2 is meant to be closed.

**Q3. Input states beyond the architecture's four.** The architecture names `queued -> sent -> acked / unconfirmed`. §2.3 adds `sending` (attempt persisted, needed for crash safety), `rejected` and `cancelled` (terminal before any byte), and `dismissed` (a human gave up an `unconfirmed` input). Options: (A) these eight states; (B) keep four and express the rest as reasons on `queued` / `unconfirmed`. A keeps `unconfirmed` meaning "may have reached the CLI", which the never-resend rule depends on, and makes crash recovery exact; B makes validation failures look like possible deliveries and blocks the queue. Both are data formats in `input_state_changed`, hard to change later. Recommendation: A, with the arch §4.3 edit below. Changes if: the four-state list is meant to be exhaustive.

**Q4. Failure policy of `before_spawn`.** Options: fail-open (timeout or crash allows the spawn, failure recorded) or fail-closed (deny). Fail-open keeps a broken extension from blocking all work; fail-closed suits policy-enforcement hooks. Public behavior of the Extension Seam; a default is hard to flip later. Cost is the same. Recommendation: fail-open by default with a per-hook `fail_closed = true` option. Changes if: `before_spawn` is meant for security policy first.

**Q5. Codex launch without a first prompt.**

| Option | Gains | Loses | Reversibility | Cost |
|---|---|---|---|---|
| A. First prompt mandatory for Codex (`spawn --agent codex` requires `--prompt`), as a product constraint | Every Codex thread binds and is controllable from turn 1; matches arch §5 (nothing written before binding). | The prompt is in argv, so it shows in process listings; Windows caps a command line at 32,767 UTF-16 units, so long prompts are rejected (limit about 24 KiB after quoting); no "open an empty Codex thread" flow. | Adding B later is additive. | Low. |
| B. Also allow an unbound manual mode (`--interactive`): no prompt, no programmatic input until a human's first turn binds the session through attach | Interactive use like a plain terminal. | The thread has no hook-based state until the first turn; `send` is refused until bound; more UI states. | Additive; removing it later breaks users. | Medium. |

Recommendation: A now, B later if wanted. Changes if: the PWA must offer "new Codex thread" without a prompt in the MVP.

**Q6. Codex-on-Windows prompts with leading/trailing whitespace or TAB.** Codex trims leading and trailing whitespace and, with paste-burst detection off, a TAB submitted early (arch §5).

| Option | Gains | Loses | Reversibility | Cost |
|---|---|---|---|---|
| A. Reject (`rejected`, reason `lossy_input`), as spike F-1d; the user edits the prompt | Never sends something other than what was written. | Pasted code with tabs or a trailing newline is refused until edited. | Adding B later is additive. | Low. |
| B. Offer an explicit transform per send (`--transform trim,tab-to-spaces`; a PWA confirmation showing the result) and compare fidelity against the transformed text | Convenient for code snippets. | A changed prompt reaches the CLI with the user's consent only; one more public flag. | Flag names are public. | Low. |

Recommendation: A in the MVP, B as a follow-up. Changes if: real usage shows frequent rejections.

**Q7. Ignored files on a normal worktree remove.** `git worktree remove` without `--force` refuses untracked changes but deletes ignored files, which can include `.env` files or local data a user cannot regenerate.

| Option | Gains | Loses | Reversibility | Cost |
|---|---|---|---|---|
| A. Refuse when ignored files exist, unless `--discard-ignored` (refusal lists the top-level ignored paths and sizes) | Nothing a user wrote is lost without saying so. | Nearly every built worktree (e.g. `target/`, `node_modules/`) needs the flag. | Relaxing later is easy. | Low. |
| B. Delete ignored files (git's default) | Removal "just works". | Can lose `.env` and other ignored user data. | Tightening later surprises users. | Low. |

Recommendation: A, because the never-lose rule (arch §1, AGENTS.md invariant 3) covers it. Changes if: the owner treats ignored files as regenerable by policy.

## Proposed Canonical-Doc Changes

Not applied here; each is a proposal for `docs/architecture.md`, `docs/prior-art.md`, or `AGENTS.md`.

- **arch §4.1**: add `epoch`, the `mode: replay | snapshot` response with `as_of_seq`, `before_seq` paging, notify-then-pull WS messages with heads sent on subscribe, WS auth by single-use ticket plus `Origin` check, and receipt states including `unknown_after_crash`.
- **arch §4.2**: extend the event kind list with `launch_started`, `launch_exited`, `session_bound`, `approval_resolved`, `input_queued`, `input_state_changed`, `mailbox_message`, `task_set`, `vt_snapshot_taken`, `thread_registered`, `thread_closed`, `worktree_changed`; state the two stream kinds and forward-compatible decoding.
- **arch §4.3**: if Q3 A is accepted, the input states of §2.3 and ack attribution by stream position.
- **arch §2 / AGENTS.md invariant 5**: name the token kinds (`owner`, `agent`, `hook`, `rescue`, `extension`) next to "regular vs Rescue".
- **arch §5 open items**: the Windows hook receiver item now concerns the axum receiver; keep `deadline_slow_head` (closed connection, no side effect) and `non_numeric_content_length` (400 per RFC 9112 §6.3) as checks to run after the change, on CI and the real host.
- **prior-art**: add rows for event store and replay (T3 Code v1/v2 tables, `decideThreadResume`), command receipts, scoped auth and pairing (T3 Code), thread/turn status and schema fixture tests (Codex app-server), worktree removal checks (T3 Code unpushed check vs herdr git refusal), and herdr's Codex-on-Windows paste-boundary key (`src/app/api/agents.rs:14-33`) as an alternative to `tui.disable_paste_burst=true`.
- **README status**: "MVP implementation has not started" changes when PR 1 merges.
