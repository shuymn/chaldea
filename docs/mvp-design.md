# MVP Design: Local-First Chaldea

Status: PRs 1–4 and 6 implemented (PR 2–4 designs: `docs/design/pty-runtime.md`, `pty-unix.md`, `pty-windows.md`; PR 6: `docs/design/event-store.md`); the rest is design. It plans the local-first MVP of issue #2. Canonical constraints: `AGENTS.md` and `docs/architecture.md` (cited as "arch §N"). Mechanism decisions: `docs/prior-art.md` (cited as "prior-art row N"). Spike evidence: `docs/spikes/pty-hooks-spike.md` (cited as "spike §N" or by its case ids such as "spike A-8", with report ids) and `spikes/pty-hooks/src/`.

**What this document fixes.** Only what PR 1 freezes or what every later PR depends on: the public wire types and payloads, the state machines and their transition rules, the HTTP/CLI surface and its authorization matrix, the receipt and idempotency contract, the sync API contract, and the PR plan. Behaviour owned by one later PR is listed as that PR's **design obligations** (§4) and designed in its own document when the PR starts.

Additional prior art read for this document (shallow clones, default branch on 2026-11-21 unless a tag is named; paths are relative to each repository root):

| Source | Revision | Read for |
|---|---|---|
| T3 Code (`pingdotgg/t3code`) | `10586f9e` | event store, command receipts, replay/snapshot resume, auth scopes, worktree cleanup |
| Codex app-server protocol (`openai/codex`, sparse `codex-rs/app-server-protocol`) | `17cb2e3` | thread/turn status model, approvals, `ts-rs` export and schema fixture tests |
| Codex CLI (`openai/codex`, tag `rust-v0.171.0`, sparse `codex-rs/hooks`, `codex-rs/cli`) | `ccf9be8` | hook trust hash, `PermissionRequest` and `Stop` payloads, `resume` argument parsing |
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

**Streams.** Every event belongs to one stream: `host` (thread registration and closing, worktrees, host- and workspace-scoped surfaces, unbound `raw_unknown`) or `thread:<thread_id>` (everything about that thread, across all its launches). `seq` is per-stream, gapless, monotonic from 1, assigned inside the append transaction. An internal `global_seq` orders events across streams and is not exposed.

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
| `source` | object | `{ kind, agent, cli_version, transcript: { generation, path, file_id, offset_end } \| null }`; the known `kind` values are `hook`, `transcript`, `process`, `command`, `daemon`, `extension`, and readers keep an unknown `kind` or `agent` string as received (`Open<T>`) |
| `occurred_at`, `recorded_at` | RFC 3339 UTC, ms | source time (or `recorded_at` when the source states none); append time |
| `payload` | object | kind-specific |

**Open envelope and forward-compatible decoding.** Readers validate the envelope first, with `kind` as a string and `payload` as raw JSON, then decode the payload by kind. An unknown `kind`, a `schema_version` above the reader's, or a payload that fails its schema becomes a `GenericEvent` (envelope plus raw payload), rendered as "unknown event" in the PWA and printed as JSON by the CLI; it never fails a page or a stream. Rust: `EventEnvelope { kind: EventKindTag, payload: Box<RawValue>, … }` with `TypedEvent::decode(&EventEnvelope) -> Result<TypedEvent, GenericEvent>`. TypeScript: `packages/contracts` validates the open envelope with the generated JSON Schema, then maps it to the generated union of known kinds or to `GenericEvent`. No `#[serde(flatten)]` is used on the envelope.

**`Content`** (public) is the type of every payload field that can be large (`text`, `prompt`, `input`, `output`, `body`, `raw`, VT grids): `{ "inline": string, "encoding": "utf8" | "base64" } | { "blob": { "sha256", "size", "media_type" }, "preview": string, "preview_lossy": bool }`. `utf8` carries text, `base64` bytes that are not valid UTF-8. Above 64 KiB the content moves to a blob; `preview` is its first 1 KiB decoded as UTF-8 with each invalid sequence replaced by U+FFFD (`preview_lossy: true` when any was); a valid character that crosses the 1 KiB boundary is left out, not replaced, so a cut alone never sets `preview_lossy`. The rest of the payload stays inline (arch §4.2 size-bounded outputs).

**`schema_version`** is one integer for the whole schema set, starting at 1. Additive changes (an optional field, a new kind) do not bump it; a change of meaning or type of an existing field does, except before any producer has persisted events: such a change (D9's `vt_snapshot_taken.grid`, `docs/design/pty-runtime.md`) keeps version 1. Per-kind versions were rejected: no client can use the extra precision.

**Idempotency keys** are deterministic so that replaying a source yields the same key; the store rejects duplicates (`UNIQUE (stream_id, idempotency_key)`):

| Source | Key |
|---|---|
| Hook payload | `hook:<emit_id>`. `emit_id` is a UUID the emitter generates once per invocation (`X-Chaldea-Emit-Id`) and reuses for its own retries. Deduplication happens on `(launch_id, emit_id)` **before** routing to a stream: each delivery stores, durably before the response, its immutable first reply, a receipt ordinal, and its current routing (`appended` with stream and seq, `buffered` while its launch is unbound, or `raw_unknown`); a retry replays the first reply, and only the routing changes when a `buffered` delivery is promoted (§2.9). A CLI that runs a hook again starts a new emitter with a new `emit_id`, which is a second event; content is never deduplicated (two identical prompts are both real). |
| Transcript line | `transcript:<generation>:<offset>` of the line start |
| Process lifecycle | `launch:<launch_id>:<reserved\|started\|exited\|death_confirmed>` |
| Command | `command:<command_id>:<n>` for the n-th event of the command |
| Derived event | `derived:<cause key>:<name>` |
| Extension `POST /events` | `ext:<caller key>`, required from the caller |

**Event kinds** (public):

| Kind | Stream | Produced by / payload |
|---|---|---|
| `thread_registered` / `thread_closed` | host | spawn / `chaldea close` (§2.11); `{ thread_id, agent, cwd, workspace_id, parent_thread_id, role, task }` / `{ thread_id, by }` |
| `launch_reserved` | thread | `{ launch_id, expected_previous \| null, agent, argv_digest, manual, worktree_id \| null }` (§2.9) |
| `launch_started` | thread | `{ launch_id, pid, start_time, pgid_or_job, cli_version }`; `pgid_or_job` is opaque display text that no reader parses: the process group id in decimal on macOS, `job:<leader pid>` on Windows, empty for a process outside the job |
| `launch_exited` | thread | `{ launch_id, code, signal, reason: "exit" \| "stopped" \| "spawn_failed" \| "daemon_restart" \| "unknown_after_crash", death_confirmed }` |
| `launch_death_confirmed` | thread | `{ launch_id, method }` for a launch that exited with `death_confirmed: false`; clears `resume_blocked` |
| `session_bound` | thread | `{ launch_id, session_id, transcript_path }` (§2.9) |
| `transcript_generation` | thread | `{ generation, path, file_id }`, appended before the tailer reads past a generation change; sets the cursor to offset 0 of that generation |
| `readiness_observed` | thread | `{ launch_id, evidence: "session_start" \| "stop_settled" \| "post_decision" \| "owner_asserted", by }` (§2.3) |
| `human_write` | thread | `{ launch_id, origin: "attach" \| "key" \| "card_key", approval_id \| null }`, appended on the first human write after each readiness evidence or input write (§2.3); `card_key` marks a key sent from an open approval card and does not close the gate |
| `user_message` | thread | `UserPromptSubmit`; `{ prompt: Content, input_id \| null }` |
| `assistant_message` | thread | `{ text: Content, turn_ref }`; Claude from the transcript; Codex from `Stop.last_assistant_message`, which is optional (`hooks/src/events/stop.rs:26-37`, v0.171.0), else from the Codex transcript when `transcript_path` is non-null (format checked by PR 12 fixtures) |
| `tool_activity` | thread | hooks only (`PreToolUse` → `started`, `PostToolUse` → `completed`); `{ phase, tool_name, tool_use_id \| null, turn_ref \| null, input: Content, output: Content \| null }` |
| `status_changed` | thread | an axis or the attention set changed (§2.2); `{ field, from, to, cause }` |
| `approval_requested` | thread | `{ approval_id, launch_id, turn_ref \| null, tool_name, tool_input: Content, correlation: { tool_use_id } \| null }` |
| `approval_resolved` | thread | `{ approval_id, outcome: "ran" \| "expired" \| "superseded" }` |
| `input_created` | thread | `{ input_id, launch_id \| null, initial_state: "queued" \| "sending", text: Content, encoding, origin, created_by }` |
| `input_state_changed` | thread | `{ input_id, from, to, reason, sent_after_seq, by }` (the last two when set) |
| `mailbox_message` | thread (recipient) | `{ message_id, from_thread_id \| null, to_thread_id, body: Content, in_reply_to \| null }` (`null` sender = a human) |
| `task_set` | thread | `{ to, by, summary }` |
| `surface_update` | host (host and workspace scope) or thread | `{ surface_id, scope, components }` (§2.8); a surface with no components is removed |
| `vt_snapshot_taken` | thread | `{ grid: Content, cols, rows, cursor, modes }`; `grid` is a JSON array of `rows` rows, each an array of cell strings, `""` only for the second cell of a wide character, trailing blank cells omitted (`docs/design/pty-runtime.md` §2.6) |
| `worktree_changed` | host | §2.6; full record on every change |
| `raw_unknown` | thread if bound, else host | `{ source, reason: "unparseable" \| "unknown_event" \| "foreign_session" \| "unbound" \| "schema_mismatch" \| "partial_line", agent, cli_version, content_type, raw: Content }`; nothing is dropped (arch §1; spike B-11) |

`turn_ref` is the CLI's own turn identifier when its payload has one (Codex `turn_id`, `hooks/src/events/permission_request.rs:39`; Claude `prompt_id` among the common input fields of the hooks reference, presence per event **unverified**). `workspace_id` is derived from the canonical path of the repository's common git directory (`git rev-parse --git-common-dir`), so a repository and all its worktrees share one; outside git it is derived from the canonical cwd. A diff kind (`artifact_diff`, arch §4.2) is not defined in the MVP: the diff view is a reversible layer, and adding a kind later does not bump `schema_version`.

**Store** (internal). SQLite in WAL mode with `synchronous=FULL` and one writer task. `events` holds the envelope columns plus `payload_bytes` and a per-stream running `cum_bytes` (an O(1) replay size). Update and delete triggers abort with `RAISE(ROLLBACK)`, `recursive_triggers=ON` makes a conflicting `INSERT OR REPLACE` hit the delete trigger, and the store never issues `REPLACE` or `UPSERT` on `events`; triggers and checks also enforce the gapless per-stream `seq`, the global order, `cum_bytes`, stream membership, and the transcript generation order and path (`docs/design/event-store.md` §2.4). Blobs live in `blobs`, with each stream's references in `blob_refs` (layout: `docs/design/event-store.md` §2.4). `command_receipts`, `hook_deliveries (launch_id, emit_id, first_reply, receipt and commit ordinals, routing)`, and `tokens` are their own tables. Internal values that projections need but no client sees (the readiness write basis, transcript continuity) live in an immutable per-event annex column, so rebuild reproduces them.

**Projections** (`threads`, `launches`, `inputs`, `approvals`, `surfaces`, `worktrees`, `transcript_cursors`) change only with the event that changes them, in its transaction, and dropping them and replaying `events` reproduces them:

| Projected column | Event payload |
|---|---|
| thread agent, cwd, `workspace_id`, parent, role, closed | `thread_registered`, `thread_closed` |
| axes, attention set | `status_changed` for process, turn, and attention; `task_set` for `task_state` (the paired `status_changed(TaskState)` is checked at write time and changes nothing) |
| current and reserved launch, pid, start time, manual, exit reason, death confirmed | `launch_reserved`, `launch_started`, `launch_exited`, `launch_death_confirmed` |
| `session_id`, `transcript_path` | `session_bound` |
| gate evidence (last readiness, its kind, and its write basis; last write of any origin; last human write that closes the gate; last turn start) | `readiness_observed`, which also stores the write basis: the PTY writer's `last_started(counted)` captured at the evidence's intake, checked by the writer at dispatch (VT replies count by default; `docs/design/pty-runtime.md` §2.3); `input_state_changed` to `sending` and every `human_write` for the last write of any origin (a `card_key` advances only this one); `human_write` with origin `attach` or `key` for the last gate-closing human write; `user_message` (from `UserPromptSubmit`) and `session_bound` for the last turn start |
| input text, encoding, launch, `created_by`, state, `sent_after_seq`, attribution | `input_created`, `input_state_changed`, `user_message.input_id` |
| approval card, correlation, outcome | `approval_requested`, `approval_resolved` |
| current surfaces | `surface_update` |
| worktree record and owner | `worktree_changed` |
| transcript generation, path, `file_id`; offset | `transcript_generation`; `source.transcript.offset_end` of the last transcript event |

An offset applies only to the generation and `file_id` of its event. A cursor offset that advanced past lines producing no event is not in any event; after a rebuild the cursor rewinds to the last event's `offset_end` in the current generation (or 0 after its `transcript_generation`), which is the only permitted rewind: the tailer re-reads only the bytes after the last stored event, which produced no event, so nothing is stored twice. A resubmitted stored line never reaches the key check: the cursor checks run first and refuse it (`StaleCursor` against an old cursor, a refused batch against the rebuilt one), and the transcript idempotency keys catch only a line whose batch passed them. Tokens, receipts, and hook deliveries are not projections.

**Transcript cursor rules.** A path is accepted only from a hook bound to the thread's current session; never from directory guessing (arch §4.2). `file_id` (device + inode on Unix, volume serial + file index on Windows) comes from the same open handle the tailer reads. `offset` is just after the last complete line. A new generation starts whenever continuity with the cursor is disproved: the path or `file_id` changed, the file is shorter than `offset`, or the bytes before `offset` changed (truncate-and-regrow); how each case is detected is a PR 13 / 14 obligation. The change is appended as `transcript_generation` before any line of the new generation is read, so every generation the events refer to is durable and rebuildable; the alternative, keeping the checkpoint table out of rebuild, was rejected because a lost checkpoint could not tell which file the offsets meant. A batch of events and the cursor advance commit in one transaction that first checks the stored `(generation, file_id, offset)` still matches. A trailing partial line after `launch_exited` becomes `raw_unknown` (`partial_line`). `transcript_path: null` (Codex, arch §5) creates no cursor. Rejected alternatives: a cursor without generation (a replaced file mixes histories), a line-count cursor, a last-line hash.

**Public vs internal.** Public: envelope, kinds, payloads, `Content`, `GenericEvent`, `schema_version` rules, `X-Chaldea-Emit-Id`, the `ext:` key format. Internal: SQL schema, projections, `global_seq`, blob layout, other key formats.

### 2.2 Thread State: Three Axes and Attention

Three independent axes (arch §2, §4.2) plus an attention set. Every change has an explicit, typed cause; the reducer's input type has no variant built from PTY output, so screen text cannot drive state (invariant 1). VT snapshots and DECSET 2004 are read only to gate input and to show a human what the CLI shows.

| Axis | States | Driven by |
|---|---|---|
| `process_state` | `starting`, `running`, `stopping`, `exited` | `launch_reserved` (§2.9); `launch_started`; `stop` accepted; `launch_exited` (also for a spawn failure) |
| `turn_state` | `unknown`, `idle`, `running`, `awaiting_approval` | `unknown` before a launch's first signal, on `Stop` until its readiness evidence (§2.7 `readiness_evidence`), on an interrupt, and after `exited`; `idle` on `readiness_observed`; `running` on a bound `UserPromptSubmit`, and when the last open card resolves while the turn is active (§2.10); `awaiting_approval` on `approval_requested`. The interrupt input comes only from an explicit event: Codex's `Interrupt` hook (spike §3; v0.171.0 `hooks/src/engine/discovery.rs:957`); Claude's adapter hook list (§2.7) has no interrupt event, so an Esc interrupt leaves `running` until the next evidence. |
| `task_state` | `none`, `active`, `blocked`, `completed`, `failed`, `cancelled` | set at registration, then only by `task_set` (thread itself, its parent, or a human; §8 D2) |

**Attention** is a set of causes, separate from `turn_state`, so clearing a cause never has to guess the turn. Each cause has its own clearing event:

| Cause | Raised on | Cleared on |
|---|---|---|
| `session_start_timeout` | no `SessionStart` 20 s after `launch_started` of a non-manual launch (spike §4.7) | `session_bound`, or `launch_exited` |
| `startup_stall` | an expected Codex `Hooks need review` after a CLI upgrade (arch §5) | `session_bound`, or `launch_exited` |
| `input_unconfirmed(input_id)` | that input entered `unconfirmed` | that input left `unconfirmed` |
| `input_gate_timeout(input_id)` | that input `queued` for 60 s | that input left `queued` |
| `resume_blocked` | a resume refused because the old launch's death is not confirmed (§2.9) | `launch_death_confirmed` |

Every raise and clear is a `status_changed` event and runs the outbound `attention` extension hook (§2.8). Timers are explicit inputs (`Tick(now)` fed by the daemon). A **relaunch** sets `process` to `starting` and `turn` to `unknown`, keeps `task`, and emits no `thread_registered`. Process exit and relaunch never change `task_state`; an orchestrator sees "exited, task still active" and decides (invariant 2).

| Option | Precedent | Verdict |
|---|---|---|
| One status enum | herdr `AgentState { Idle, Working, Blocked, Unknown }` from screen detection (`src/detect/mod.rs:11-20`) | Rejected: mixes the axes and is fed by screen scraping. |
| Thread status + turn status | Codex app-server `ThreadStatus { NotLoaded, Idle, SystemError, Active { active_flags: [WaitingOnApproval, WaitingOnUserInput] } }` (`src/protocol/v2/thread.rs:1810-1827`), `TurnStatus { Completed, Interrupted, Failed, InProgress }` (`v2/turn.rs:34-39`) | Partly adopted: its flags map to `awaiting_approval` and attention. No task axis, because it has no orchestration outcome. |
| Three axes + attention set | arch §2 | **Adopted.** |

Public: field names, state and cause values, `status_changed` / `task_set` payloads. Internal: reducer, timer defaults.

### 2.3 Input State Machine and Gate

An input is one prompt sent through `chaldea send`, the PWA, or `surface_action`. `send` writes the adapter-encoded text plus one submit. A paste the attach client recognises is encoded text only; a physical Enter from the human submits it (§4, PR 11).

```
queued ──(gate open; attempt persisted)──▶ sending ──(text + submit written)──▶ sent ──(attributed ack)──▶ acked
  │                                          └───────(write outcome unknown, daemon restart, launch_exited)────┐   │
  │                                                                     (window expired, ack not attributable)─┴─▶ unconfirmed
  ├─(validation failed, zero bytes)──▶ rejected                                  late attributed ack ──▶ acked ◀─┤
  └─(cancel, launch exited, daemon restart; zero bytes)──▶ cancelled             input resolve ──▶ acked (by human) | dismissed
     (also sending ──(writer recheck failed before the first byte)──▶ cancelled)
```

**Gate (one conservative rule).** A queued input is written only when the current launch has **positive readiness evidence** newer than the last write of any origin: a `readiness_observed` event from the adapter's `readiness_evidence` capability (§2.7) or an owner's explicit `chaldea ready <thread>` (`evidence: owner_asserted`, accepted only for the current bound launch; for an unbound launch, e.g. a manual Codex launch, it is refused and writes nothing). In addition, `turn_state = idle`, no other input of the current launch is `sending`, `sent`, or `unconfirmed` (inputs of earlier launches never block), and for bracketed paste `TermMode::BRACKETED_PASTE` is set (prior-art row 4a).

- **`readiness_evidence` modes** (per adapter, §2.7): `post_decision` — a signal that the CLI emits only after every `Stop` hook decided (none is verified yet; PR 12 looks for one); `stop_settle` — a `Stop` followed by a settle interval with no further event of the launch, with a known race: a user `Stop` hook that blocks after the interval (Claude per the hooks reference; Codex `hooks/src/events/stop.rs:83`) makes the next prompt land in a running turn, where it is attributed or left `unconfirmed`, never resent; `none` — only `chaldea ready`. The MVP default is §8 D8.
- Any human write (attach input, raw `key`) after the evidence closes the gate (`human_write`). A key sent from an open approval card (D1; it carries that card's `launch_id` and `approval_id`) answers a dialog while the turn awaits approval, not the composer, so it is recorded with origin `card_key` and does not close the gate; the turn's next readiness evidence opens it. It reopens only on `chaldea ready` or on evidence from a later turn, i.e. with a `UserPromptSubmit` or `SessionStart` of the launch between the human write and the evidence; evidence of the same turn as the human write, however late it arrives, does not reopen it. Detaching does not reopen it.
- Unknown readiness means closed: no evidence yet, or an interrupt.
- After 60 s in `queued` the input raises `input_gate_timeout` with a `vt_snapshot_taken` and stays `queued`, zero bytes written.
- **Writer revalidation.** The writer re-evaluates the whole gate, including `process_state = running` and the input's launch, under its arbitration lock immediately before the first byte, and writes the text and its submit as one unit that no human write can interleave. A failed recheck cancels the input with zero bytes, and so does a counted write (VT replies included by default) started after the write basis stored with the readiness evidence. Human writes queued during that unit go after it and make a later ack non-attributable if their order against the hook is ambiguous, so the input stays `unconfirmed`.

**States.**

- **`sending`**: the transaction that records `sending` and `sent_after_seq` (the thread stream's head) commits before the bytes go to the PTY writer.
- **Ack**: a `UserPromptSubmit` of the same launch with `seq > sent_after_seq` whose prompt equals the input's expected form (the text after the adapter's documented transforms: Claude's TAB expansion, NFC, and paste wrapper, arch §5; Codex's `win_keys` composition and trimming), and only if, at the hook's intake, this input's write had dispatched and no other counted write (attach, `key`, another input, and by default a VT reply) had started after it, by the PTY writer's position (the Codex positional prompt is anchored by `launch_started` instead). Position, not wall time, orders candidates. A hook whose prompt matches no in-flight input is a `user_message` without `input_id`; one that matches more than one input acks none. An ack that arrives while the input is still `sending` is kept as its candidate and evaluated when `sent` is recorded.
- **`unconfirmed`**: the 30 s window (spike §4.7) expired, the candidate is not attributable (it stays a `user_message` without `input_id`), the write outcome is unknown, the daemon restarted, or the launch exited while `sending`/`sent`. **Never resent** (arch §4.3). A late attributable ack moves it to `acked`. `chaldea input resolve <id> --delivered | --dismiss` settles delivery only: `acked` (`by: human`) or `dismissed`, keeping the history and clearing `input_unconfirmed`; it does not assert readiness, which is `chaldea ready`. Both `ready` and `input resolve` are recovery commands that write no PTY bytes.
- **Prompt limit.** `send` and recognised attach pastes refuse prompt text above `core::input::MAX_PROMPT_BYTES` (256 KiB UTF-8) before admission, with zero bytes and no input created.
- **`rejected`** (ESC or C0 other than TAB/CR/LF, prior-art row 4b; Codex-on-Windows lossy text or empty prompt, spike F-1d, §8 D6) and **`cancelled`** are terminal with zero bytes written. `cancelled` comes from `queued` (`chaldea input cancel <id>`, the launch exited, a daemon restart) or from `sending` only when the writer's recheck fails before the first byte.
- **Codex positional first prompt**: its `input_created` (`initial_state: sending`) commits with `launch_reserved`, since its bytes travel in argv; `launch_started` moves it to `sent` with `sent_after_seq` = that event's seq, in the same transaction and before any waiting hook is released. If the spawn is confirmed not to have started (e.g. the executable was not found, `spawn_failed` before the child existed), it goes `sending` → `rejected` with that reason; only an indeterminate spawn (a crash between reservation and `launch_started`) is recovered as `unconfirmed`. Its ack window is suspended while `session_start_timeout` or `startup_stall` is active.

**Residual constraint.** Hooks are asynchronous processes and carry no CLI-side ordering against the daemon's PTY writes, so ack attribution and readiness remain inferences. The contract guarantees only that (a) no input is ever resent automatically, (b) no input is acked without a text match, and (c) any ambiguity leaves the gate closed or the input `unconfirmed`. Later PRs do not need to close this gap.

Alternatives: auto-resend (rejected by arch §4.3; an extra Return can accept a popup, arch §5); ack by screen echo (invariant 1); readiness inferred from DECSET 2004 or the screen (invariant 1; Codex sets 2004 while a review dialog is up, spike §3); special-case rules per input source (rejected: the gate requires the same positive evidence from every source).

Public: state names, payloads, `send`, `ready`, `input resolve`, `input cancel`, the `readiness_evidence` modes. Internal: window, timeout, and settle defaults. The states beyond the architecture's four are §8 D3.

### 2.4 Sync API and Commands

**Replay.** `GET /api/events?stream_id=<id>&after_seq=<N>&limit=<L>[&epoch=<E>]` (arch §4.1). `limit` 1–1000, default 200 (else 400). `epoch` is a random id stored at database creation; the client omits it on its first fetch and sends the last one it saw afterwards. Response (public `EventsResponse`):

```json
{ "mode": "replay", "stream_id": "thread:…", "epoch": "…", "events": [ … ], "head_seq": 812, "has_more": false, "has_more_before": true }
{ "mode": "snapshot", "stream_id": "thread:…", "epoch": "…", "snapshot": { … }, "as_of_seq": 812, "reason": "epoch_changed" }
```

Snapshot fallback when the given `epoch` differs from the server's, `after_seq > head_seq`, or the replay would exceed 10 000 events or 8 MiB (from `cum_bytes`). The decision is one pure function, `decide_replay`, as T3 Code's `decideThreadResume` (`apps/server/src/orchestration-v2/ThreadStream.ts:99-118`, bounds at `:18-20`). The client also checks every response's `epoch` against the one it holds and, on a difference, drops its local copy and starts from a snapshot; only a forward response (on the active reconnect or fetch) may establish a new epoch, switching epoch and local state together, and a response from an older request generation is discarded. Snapshot and `as_of_seq` come from one read transaction. A **thread snapshot** is the thread's projection (axes, attention, launch, session, gate evidence, inputs not yet terminal, open approval cards, worktree, current thread surfaces, and, while any attention cause is active or an approval card is open, the latest `vt_snapshot_taken`) plus the last 50 events; a **host snapshot** is the thread list, worktrees, and current host- and workspace-scoped surfaces. Everything a client renders is in a snapshot or in events after `as_of_seq`. A stream that does not exist or that the token may not read is 404; an empty stream returns `replay` with `events: []`, `head_seq: 0`.

**Backward history.** `GET /api/events?stream_id=<id>&before_seq=<N>&limit=<L>[&epoch=<E>]` returns the nearest `L` events preceding `N` (`seq < N`), in ascending order, with `has_more_before`. An epoch mismatch on a backward page does not switch the client: it triggers a forward resync, and the client switches epoch and local state together on that forward response. `after_seq` with `before_seq` is 400.

**Live notifications.** `POST /api/ws-ticket` (Bearer) returns a ticket; `GET /api/ws?ticket=<t>` upgrades. Server → client: `{ "type": "hello", "epoch", "server_time" }`, `{ "type": "stream_advanced", "stream_id", "head_seq" }`, `{ "type": "heads", "heads": [{ "stream_id", "head_seq" }] }`; client → server: `{ "type": "subscribe", "stream_ids": [...] }`. Notify-then-pull: the client fetches with `GET /api/events`. The server sends current heads on every `subscribe` and a `heads` message every 30 s, so a lost notification is recovered within 30 s, and the client refetches on every reconnect. Inline event push is deferred; it can be added as an optional field.

**Commands** (public). Every mutating request carries a client-generated `command_id`: `spawn`, `resume`, `send`, `ready`, `key`, `stop`, `close`, `input resolve`, `input cancel`, `task set`, `mailbox send`, `worktree remove`, `doctor run`, `surface_action`. Receipts follow T3 Code's `orchestration_v2_command_receipts` keyed by `command_id` (`Migrations/055_OrchestrationV2.ts:42-50`), with:

- **Insert.** The receipt is inserted `in_progress` in the transaction that records the command's intent; the primary key makes a concurrent duplicate fail its insert, so only one request can act.
- **Completion.** `send` appends `input_created` and its completed receipt (with `input_id`) in one transaction; delivery is then the input's state. Synchronous side-effect commands (`key`, `spawn`, `resume`, `stop`, `close`, `worktree remove`) stay `in_progress` until the effect finished or failed.
- **`ReceiptLookup`** (core) and HTTP: `New` → the command runs; `InProgress` → 409 `{ code: "in_progress", retry_after }`; `Replay(result)` (same token and request hash) → the stored status and body; `Conflict` (other hash or other token) → 409 `{ code: "conflict" }`, revealing nothing; `UnknownAfterCrash` → 409 `{ code: "unknown_after_crash" }`.
- **Restart.** Every `in_progress` receipt becomes `unknown_after_crash` and is never re-executed. Inputs are recovered from the `inputs` table, independent of receipts: `queued` → `cancelled` (zero bytes), `sending`/`sent` → `unconfirmed` (unknown). Launches and worktrees follow §2.9 and §2.6.

**Offline.** When the daemon is unreachable, the PWA keeps the last state, locks input, and shows the time of the last successful sync (arch §4.1).

`stream_id`, `epoch`, and `command_id` are public now so the Phase 2 relay can cache per stream and forward commands unchanged. A `stateright` model (PR 17) checks startup crash, offline buffering, and retry after a lost response (arch §6).

### 2.5 HTTP Surface and Authorization

The daemon listens on `127.0.0.1:<port>` only, on `axum` + `hyper` + `tower-http` (arch §3), so request parsing is hyper's (prior-art row 5a's reason). Token comparison uses `subtle` (prior-art row 5d); tokens are stored as SHA-256 hashes (`sha2`, row 5c).

**Common checks.** `Host`: one header, `127.0.0.1:<port>` or `localhost:<port>`, else 421 (DNS rebinding; spike B-8). `Origin`, where checked: absent, or exactly `http://` + the received `Host`, else 403. Bearer: one `Authorization: Bearer` header, else 401. **Deadlines:** `/hook` and short non-streaming routes have a 5 s whole-request deadline; WebSockets, attach, and the long synchronous commands (`spawn`, `resume`, `stop`, `close`, `worktree remove`, `doctor run`) each have their own bounded deadline (values in their PRs). The deadline and the intent commit are decided in one step: a command answers 408 only if its deadline prevented the commit; once the intent committed, the reply follows the receipt (409 `in_progress` or the stored result), and a retry with the same `command_id` is the recovery path (§2.4).

**Routes** (public):

| Route | Origin | Credential | Object check |
|---|---|---|---|
| `GET /`, `/assets/*` (PWA) | — | none | serves no data; CSP `default-src 'self'`, `connect-src` own origins |
| `POST /api/pair` | present and matching | single-use pair code | issues an `owner` token |
| `POST /api/ws-ticket` | absent or matching | Bearer | ticket bound to the token and a purpose (`events` or `attach:<thread>`) |
| `GET /api/ws` | absent or matching | ticket | subscriptions limited to readable streams |
| `GET /api/attach/<thread>` (WS) | absent or matching | ticket `attach:<thread>` | owner only |
| `GET /api/events`, `/api/blobs/<sha256>?stream_id=` | absent or matching | Bearer | readable stream; for blobs also `(stream_id, sha256)` in `blob_refs` |
| `GET /api/threads` | absent or matching | Bearer | threads the token can read |
| `GET /api/worktrees` | absent or matching | Bearer | owner and rescue: all, including human-owned; agent: those owned by self+ |
| `GET /api/adapters` | absent or matching | Bearer | owner, agent, rescue; a static capability catalog with no host data (trust state is in doctor) |
| `GET /api/doctor` | absent or matching | Bearer | owner and rescue; the latest doctor report |
| `POST /api/commands/<name>` | absent or matching | Bearer | action matrix; receipt owned by the token |
| `POST /hook` | must be absent | `hook` token | its own launch |
| `POST /events` | must be absent | `owner`, `rescue`, or `extension` token | owner and rescue: any scope; extension: its configured scope; kinds `surface_update`, `raw_unknown` |

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
| send, stop, resume, close | ✓ | self+ | — | — |
| task set | ✓ | itself and its direct children (§2.2) | — | — |
| input cancel | ✓ | inputs it created | — | — |
| key, attach, and the recovery commands ready and input resolve | ✓ | — | — | — |
| mailbox send | ✓ | to its parent and children | — | — |
| worktree remove | ✓ (`--discard-ignored` and `--force` allowed) | worktrees it owns, neither flag | — | — |
| `POST /events` | any scope | — | any scope | its scope |
| `surface_action` | ✓, when the surface's scope contains the target (Thread: that thread; Workspace: a thread with that `workspace_id`; Host: any) | — | — | — |
| doctor run / read | ✓ / ✓ | — | — / ✓ | — |

**Order.** The route and the token kind are checked before any body byte is consumed; object authorization runs on the validated body before any effect, and for an update or delete it also checks the stored object's scope (e.g. a surface replaced by `POST /events` must already be in the caller's scope). A receipt is visible only to the token that created it. Hook binding also requires the emitter ancestry rule (§2.9; prior-art row 6c).

**Secrets.** Logs redact `Authorization`, the `ticket` parameter, and pair codes, and never contain `/hook` or `/api/pair` bodies. `chaldea open` opens `http://127.0.0.1:<port>/#pair=<code>` (single use, 60 s); the fragment never reaches the server, the page exchanges it via `POST /api/pair`, stores the `owner` token in IndexedDB, and removes the fragment with `history.replaceState`. Rejected: an `HttpOnly` cookie (needs CSRF defences on every route; invariant 5 names Bearer). T3 Code uses the same shape: scoped sessions, single-use pairing links (`Migrations/031_AuthAuthorizationScopes.ts:4`, `:12-48`), `timingSafeEqual` (`apps/server/src/auth/utils.ts:83-89`). herdr's socket API has no token (`src/ipc.rs:10-11`); rejected because Chaldea's API is HTTP. CF Access and host Service Tokens (arch §2) are Phase 2; the token enum leaves room.

**Rejections (spike B-8 on hyper).** Two layers, neither with side effects (no event, fixture, or readiness change):

- **Parser layer (hyper)**, before any middleware runs: malformed framing such as `Content-Length: +5` → 400 (RFC 9112 §6.3 requires 400; the spike's 411 is replaced); an oversized head → hyper's status; a slow head → connection closed within its budget, observed by the client as EOF, never inferred from a client timeout or `status: 0`. Duplicate equal `Content-Length` and `Transfer-Encoding` follow hyper's framing rules; PR 9 records the observed outcome.
- **Middleware layer**, for parsed requests only, in order Host → Origin → Bearer → scope → framing → size, before any body byte is used: a missing `Content-Length` on POST → 411, any remaining `Transfer-Encoding` → 400, a body over 4 MiB → 413, the request deadline → 408.

### 2.6 Worktree Ownership

`chaldea spawn --worktree [--base <ref>]` creates a git worktree with the git CLI (as herdr and T3 Code do), through `process-wrap` with `CREATE_NO_WINDOW` on Windows (prior-art rows 1c, 9). Metadata (public, in `worktree_changed` and `GET /api/worktrees`): `worktree_id` (8 hex, also the directory name), `repo`, `workspace_id`, `path`, `branch` (`chaldea/<id>` unless given), `base_ref`, `base_commit` (resolved SHA), `owner_thread_id`, `created_by` (`human` or `thread:<id>`), `state` (`creating`, `active`, `released`, `removing`, `uncertain`, `removed`, `failed`).

- **Short base path.** `worktree_base` is configurable per workspace, default `<state_dir>/wt`, so a Windows user can pick a short root such as `C:\wt` for MSVC's 260-character limit (arch §5).
- **Ownership.** On `thread_closed`, ownership passes to the parent thread, or to the human owner when there is none.
- **Crash safety.** The `creating` / `removing` intent commits before git runs; restart reconciles against git and never deletes anything.
- **Exclusion.** The worktree row is the single point of exclusion: a launch reservation that uses the worktree and a removal both take it in one transaction. A reserved or live launch, or one whose death is unconfirmed, blocks removal; a reservation requires the worktree to be `active` or `released` (§2.9).
- **Never auto-delete.** No exit, close, restart, or timer removes a worktree. `chaldea worktree remove <id>` is **refused**, with nothing changed, when a check before any mutation fails: tracked or untracked changes; ignored files without `--discard-ignored` (owner only, §8 D7); a commit reachable from the worktree's `HEAD` (detached included) or its registered branch that is not on a remote-tracking ref (`git rev-list --max-count=1 HEAD <branch> --not --remotes`, the check T3 Code runs before forced removal, `apps/server/src/storageCleanup.ts:1004-1022`); a launch that uses it and is reserved, live, or not yet confirmed dead; or a git error during these checks. If git fails after it started changing files, the result is `uncertain`, not a refusal; the record stays `uncertain` and is reconciled from the intent before any retry. `--force` (owner only) skips the content checks but never the in-use check, and is recorded. herdr relies on git's dirty refusal (`src/worktree.rs:194-215`), which misses unpushed commits.
- **Branches are never deleted** (herdr does the same, `src/worktree.rs:175-192`); an explicit opt-in is a later, reversible addition.

### 2.7 Adapter Capability Table

A const table in `crates/core`, keyed by `(agent, os)`; no `cfg` decides it (spike F-1f). `GET /api/adapters` exposes it as a static catalog.

| Capability | Claude Code (macOS, Windows) | Codex CLI macOS | Codex CLI Windows | Evidence |
|---|---|---|---|---|
| Hook injection | `--settings <run>/claude-settings.json` + `--session-id <uuid>`; global settings untouched | `-c hooks.<Event>=[…]`, fixed command strings, order, and timeouts from the first release (the timeout is part of the trust hash, `hooks/src/engine/discovery.rs:715-718`, `:847`); per-session values in env | same | arch §5; prior-art rows 6a, 6b |
| Hook events | `SessionStart`, `UserPromptSubmit`, `Stop`, `SessionEnd`, `PermissionRequest`, `PreToolUse`, `PostToolUse`, `Notification` | `SessionStart`, `UserPromptSubmit`, `Stop`, `PermissionRequest`, `PreToolUse`, `PostToolUse`, `Interrupt` (registered from the first release, since adding an entry later costs a hook review) | same | spike §3; payloads beyond the spike's four are checked by PR 12 fixtures |
| Session binding | predetermined `--session-id` | §2.9 | same | spike §4.3 |
| `readiness_evidence` | `SessionStart`; after a turn, the §8 D8 mode (`stop_settle`) | the §8 D8 mode (`stop_settle`); no hook fires before the first turn (arch §5) | same | §2.3; PR 12 fixtures |
| First prompt | PTY input after readiness | positional argument (§8 D5) | same | arch §5 |
| Text encoding | bracketed paste only while DECSET 2004 is set; newline `cr` | bracketed paste; `cr` | `win_keys` | arch §5; prior-art rows 4a, 4d; spike A-6, F-1f |
| Submit | one CR after a delay | same | one Return after a delay, ready composer only | prior-art row 4e; arch §5 |
| Extra flags | — | `--no-daemon` | `--no-daemon`, `-c tui.disable_paste_burst=true` | arch §5 |
| Resume | `--resume <session_id>` (**unverified**) | `codex resume <id> [<prompt>]` (parser tests `cli/src/main.rs:4061-4072`, v0.171.0; hook behaviour on resume **unverified**) | same | PR 12 fixtures |
| Assistant text | transcript JSONL from the bound `transcript_path` | `Stop.last_assistant_message` (optional), else the transcript when its path is non-null | same | §2.1 |
| Hook trust | n/a | record CLI version with trust state; `Hooks need review` after an upgrade is `startup_stall` | same | arch §5; prior-art row 6b |
| Startup landmines | trust prompt defaults to "No, exit": never send Enter blindly | update, folder trust, hook review; digits pick options | same | arch §5 |
| Env scrub | `CLAUDECODE`, `CLAUDE_CODE_*`, `CODEX_THREAD_ID`, `CODEX_SESSION_ID`, other `CODEX_*` session variables, inherited `CHALDEA_*` | same | same | prior-art row 6c |

**`win_keys`** (Codex on Windows, first MVP test, arch §5): reject ESC/C0 other than TAB/CR/LF, trimmable leading/trailing whitespace, TAB, and empty text (§8 D6); NFC-compose each grapheme cluster holding a combining or extending character, keeping CJK compatibility ideographs; pad with one leading and one trailing space; ASCII as UTF-8, **every other BMP scalar** as a win32-input-mode key record pair, astral scalars as UTF-8; each newline as a Shift+Enter record pair; every escape sequence in one write. The same text encoding applies to `send` and to recognised attach pastes; only `send` adds the submit. The spike sent records only for Mn/Me/Cf; the encoding widens that because U+2764 sent as UTF-8 was lost (arch §5). Byte sequences, delays, and the fallback are PR 5, 14, and 15 obligations.

### 2.8 Extension & Rescue Surface (Minimal)

- **One-shot hooks** (outbound, arch §4.4): `before_spawn`, `after_close`, and `attention` (run on every attention raise and clear). Each gets on stdin `{ "hook", "host_id", "thread": { "thread_id", "agent", "cwd", "workspace_id", "parent_thread_id", "role" }, "attention": { "cause", "raised" } | null }` and the env `CHALDEA_URL`, `CHALDEA_EXTENSION_TOKEN`, `CHALDEA_THREAD_ID`; each run is bounded by a timeout (value in PR 20); only `before_spawn`'s stdout `{ "decision": "allow" | "deny", "reason" }` is read. Failure policy: §8 D4.
- **A2UI surfaces**: `{ surface_id, scope: { kind: "host" } | { kind: "workspace", workspace_id } | { kind: "thread", thread_id }, components }` with `text`, `markdown`, `vt_snapshot` (its grid encoded and fetched as in `vt_snapshot_taken`), `button`, `text_input`, `list`. A `button` carries `surface_action { action_id, kind: "send_input" | "send_key" | "stop" | "ready" | "open_attach", args }`, run only when a human with an owner token clicks it, through the command API. This is how a Rescue Agent acts on a PTY.

### 2.9 Launch Contract

- **Reservation.** One transaction checks the thread's current `launch_id` against the one the command expected, checks the thread is not closed, the previous launch is `exited` with death confirmed, and the worktree (if any) is `active` or `released` (so `creating`, `removing`, `uncertain`, `removed`, and `failed` all refuse, §2.6), and appends `launch_reserved` with the hashes of the launch's hook and agent tokens and the receipt; only then is `PtyRuntime::spawn` called. `launch_started` records pid, start time, and pgid or job. `close` takes the same thread row, so a close and a resume cannot both succeed.
- **Early hooks.** A hook with a valid token whose launch has no recorded pid waits, bounded and inside the request deadline, for the pid; then the ancestry rule runs; past the bound it is `raw_unknown` (`unbound`).
- **Binding.** Claude binds by its `--session-id`. Codex hook processes run concurrently, so `UserPromptSubmit` can arrive before `SessionStart`: authenticated hooks of an unbound launch are stored with the `buffered` disposition (bounded count, size, and time), ordered by receipt order at the daemon (an ordinal PR 9 assigns at intake and the store keeps with the delivery), since hook processes carry no shared sequence; the first `SessionStart` whose emitter's nearest CLI ancestor is the launched pid binds it; each buffered event's ancestry is evaluated at receipt, while its emitter is alive, and that result is checked again before promotion, and those with that `session_id` that pass are appended in that order in the binding transaction, while the rest become `raw_unknown` (`foreign_session`). An overflow or timeout turns the buffer into `raw_unknown` (`unbound`), and so does a restart for every launch that can no longer bind. Rejected: binding from the first hook of any type, because only the `SessionStart` rule was verified (spike §4.3, B-11).
- **Daemon restart.** Before accepting commands, the daemon records `launch_exited` (`daemon_restart`, or `unknown_after_crash` without a recorded pid) for every open launch, with `death_confirmed: false`, and revokes its tokens; `launch_death_confirmed { method }` follows once the old tree is proven dead. A **resume is refused**, with `resume_blocked` raised, until then; such a launch also counts as in use for worktree removal.
- **Resume** (`chaldea resume <thread> [--prompt …]`) is an explicit relaunch through the reservation: a new `launch_id` and tokens, the bound `session_id` passed to the adapter's resume flag, and the transcript cursor carried over. For Codex, `--prompt` is passed positionally (`codex resume <id> <prompt>` parses, `cli/src/main.rs:4070-4072`); without it the launch is a manual launch (§8 D5).

### 2.10 Approval Cards

An approval card has an `approval_id` and, when possible, a correlation. Claude's `PermissionRequest` input has `tool_name` and `tool_input` "like PreToolUse hooks, but without `tool_use_id`" (Claude hooks reference, `PermissionRequest input`); Codex's request has `turn_id` and no tool id (`hooks/src/events/permission_request.rs:37-51`). The correlation is therefore the `tool_use_id` of the latest `PreToolUse` of the **same turn** (same `turn_ref`, or no turn boundary in between) with equal `tool_name` and `tool_input`, when exactly one matches (that ordering is an inference, **unverified** until PR 12 fixtures); otherwise the card is uncorrelated.

- **Open:** `approval_requested`; `turn_state` → `awaiting_approval`.
- **Resolved** only on evidence correlated to that card: a `PostToolUse` with its `tool_use_id` (`outcome: ran`).
- **Superseded / expired:** `Stop` or an interrupt (§2.2) closes every open card with `outcome: superseded`; `launch_exited` closes them with `expired`. These take precedence over resolution, and the turn follows §2.2.
- When the last open card resolves while the turn is active, `turn_state` returns to `running`. A key sent from a card carries `launch_id` and `approval_id` and is refused (409) at the write boundary when that card is no longer open.

### 2.11 Thread Close

`chaldea close <thread> [--stop]` (owner, or an agent for self+) closes a thread whose current launch is `exited` with death confirmed (with `--stop`, it stops it first). It takes the thread row as in §2.9, emits `thread_closed`, runs `after_close`, passes worktree ownership on (§2.6), and revokes the thread's tokens. `closed` is terminal: no `send`, `resume`, or `spawn` into it. Closed threads stay readable.

## 3. Workspace Layout

```
Cargo.toml, rust-toolchain.toml (1.96.0)    # workspace, shared lints, exact-pinned deps (spike policy)
crates/core/        # Sans-IO: types, envelope, reducers, input machine, encoders, capability table, replay decision,
                    # receipt lookup, auth policy, verdict model. No tokio, no std::fs/net/process, no clock.
crates/pty-runtime/ # PtyRuntime trait, FakePty, unix (rustix PTY + `Command`/`pre_exec`), windows (own windows-sys ConPTY),
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

**Dependency direction.** `core` ← `pty-runtime` ← `daemon` ← `cli`; `testkit` may depend on `core` and on no other workspace crate; on Windows it reaches the console through `winapi-util`, keeping `forbid(unsafe_code)`. `core` depends on `serde`, `serde_json` (`raw_value`), `ts-rs`, `schemars`, `uuid`, `unicode-normalization`, `unicode-segmentation`, `subtle`; `sha2` (blob addresses, token hashes) is a `daemon` dependency. Sans-IO is enforced by `crates/core/clippy.toml` (disallowing `std::fs`, `std::net`, `std::process`, `Instant::now`, `SystemTime::now`, `std::thread::spawn`) and a CI check that `core` has no `tokio`. `unsafe_code = "forbid"` stays workspace-wide except in `pty-runtime`, whose own `[lints]` table (`docs/design/pty-unix.md` §2.7) uses `deny` and allows `unsafe` only in its two `sys` modules (`unix::sys` and `windows::sys`).

**Single binary.** `chaldea hook-emit` dispatches before any runtime or config initialization and uses std networking only (arch §2). Console subsystem on Windows; no GUI-subsystem variant (prior-art row 9, spike C-5 `go`).

| Option | Precedent | Verdict |
|---|---|---|
| One crate with modules | herdr, `src/platform/{unix_common,macos,windows}.rs` | Rejected: Sans-IO and the Windows CI gate are easier at crate boundaries. |
| Protocol crate + server crate | Codex `codex-rs/app-server-protocol` owning the generated schema | **Adopted** in spirit: `core` owns every public type. |
| Separate store crate | — | Rejected for the MVP: one user. |

**Spike modules.** Copied, not moved; `spikes/` and its CI job stay until issue #1.

| Spike module | Destination | Decision |
|---|---|---|
| `src/os/drain.rs` | `pty-runtime` | Lift without the hold: the session's sink is passed into spawn and fed from the first byte. |
| `src/vt.rs` | `pty-runtime` | Lift. |
| `src/os/unix.rs` | `pty-runtime::unix` | Lift kill and enumeration; drop console and probe helpers. |
| `src/os/windows.rs` | `pty-runtime::windows`; parent job check in `daemon` | Lift the backend, handle wrappers, teardown order; drop the window observer, calibration, keys probe, registry reads. |
| `src/pty.rs` | `pty-runtime` | Rewrite behind `PtyRuntime`, keeping single-writer ordering and the revocable gate. |
| `src/paste.rs`, `src/marks.rs` | `core::input` | Lift the pure encoders, widen `unit_records` to every non-ASCII BMP scalar; rewrite the writer-driving parts in the daemon. |
| `src/receiver.rs` | `daemon` | Rewrite on axum; keep the binding rules and B-8 cases. |
| `src/emitter.rs`, `src/http_client.rs` | `cli` | Lift; add `X-Chaldea-Emit-Id` (retry policy in PR 9). |
| `src/session.rs` | `daemon` | Rewrite; lift `scrub_env_name` and the kill ordering. |
| `src/verdict.rs` | `core::verdict` | Lift the outcome model; change the duplicate and plan rules (§7). |
| `src/fake_cli.rs` | `testkit` | PR 3 lifts `echo` (input reporting), `queries`, `attachable` (size and `SIGWINCH` reporting), `spawn-tree` (direct hellos), and `sleeper`; PR 4 adds the Windows console, `attachable` size polling, the `breakaway` role, `--emit-output`, and `--validate-input`, and the new `report-process` mode; `stall` and `banner` come with PR 10. A loopback report stream per process replaces `/witness`. |
| `src/scenario/*`, `report.rs`, `cli.rs` | — | Not lifted. |

**Tooling.** CI adds a `workspace` job (macOS + Windows): `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`, the contract drift check, the `core` dependency check; `lefthook.yml` mirrors it. Node and pnpm join `mise.toml` with PR 18; the JS job runs typecheck, Vitest with fast-check, and Playwright.

## 4. PR Plan

Each PR keeps `core` + `FakePty` tests green on macOS and Windows CI; macOS first within each PR, its Windows code in the same PR.

| PR | Scope | Depends on | Milestone |
|---|---|---|---|
| 1 | Workspace, CI, `core` contracts (§5) | — | Ingest, Sync (types); Verification (PBT) |
| 2 | `PtyRuntime` trait, `FakePty`, single writer with origin log, drain, VT responder + snapshot | 1 | PTY |
| 3 | macOS backend (rustix PTY, `Command` + `pre_exec`, `killpg` HUP→TERM→KILL, `getsid` scan, recorded-pid fallback); `testkit` | 2 | PTY (macOS) |
| 4 | Windows backend (flags 0, `kernel32.dll` only, JOB_LIST + `CREATE_SUSPENDED` fallback, job + recorded-descendant kill) | 2, 3 | **closes PTY** |
| 5 | `core::input` encoders, capability table | 1 | Agent Control |
| 6 | Store: append-only events, projections, receipts, hook deliveries, blobs | 1 | Ingest |
| 7 | HTTP: route and action matrices, tokens, pairing, sync API, blobs, WS, receipts | 6 | Sync, Extension |
| 8 | Daemon lifecycle (`daemon run/start/stop`, `DETACHED_PROCESS`), parent kill-on-close job check | 7 | Agent Control |
| 9 | `POST /hook`, binding, `raw_unknown`, `chaldea hook-emit`, B-8 cases (**Windows hook receiver** open item) | 7, 3 | Ingest |
| 10 | Supervisor: reservation, `spawn/stop/wait/resume/close`, send path and gate, attention, restart, `GET /api/adapters` | 3, 4, 5, 8, 9 | Agent Control |
| 11 | `chaldea attach` with the attach input contract | 10 | Agent Control |
| 12 | Fixture corpus: capture (`chaldea doctor capture`) with typed verdict, envelope and payload-presence validation | 9, 10 | Verification |
| 13 | Claude adapter: hooks, readiness evidence, bracketed paste send, transcript tailer, approvals, resume | 10, 12 | Agent Control; Ingest (tailer) |
| 14 | Codex adapter: flags, positional prompt, `win_keys` for send and attach paste, trust stall (**Codex hook trust renewal**), assistant text, resume | 13, 11 | Agent Control; **closes Ingest** |
| 15 | **First tests 1 and 2** on a real Windows host (**Codex keyed input**, **human paste through attach**) | 14 | Cross-OS E2E |
| 16 | Mailbox, agent tokens, `task set`, worktree ownership and remove | 10 | **closes Agent Control** once 11, 14, 16 are all merged |
| 17 | `stateright` Sync model | 7 | Verification |
| 18 | PWA read path: `packages/contracts/package.json`, validators, pairing, thread list/view, paging, `GenericEvent`, client reducer | 7 | Sync & PWA |
| 19 | PWA input: `<textarea>` IME guards, send, ready, input resolve, approvals, VT snapshot, keypad; Playwright | 18, 13 | **closes Sync & PWA** |
| 20 | Extension & Rescue: one-shot hooks, `POST /events`, rescue role, A2UI, `surface_action` | 16, 19 | **closes Extension & Rescue** |
| 21 | `chaldea doctor` and `GET /api/doctor` | 14, 16 | Cross-OS E2E |
| 22 | `cargo-fuzz` targets + scheduled CI | 5, 9, 13 | **closes Verification** (with 12, 17) |
| 23 | Real-host run: §6.1 on macOS and Windows; more Windows builds where available (**Windows build coverage**) | all | **closes Cross-OS CI & E2E**; end of the MVP |

**Longest paths.** The longest dependency chains are 10 PRs, all ending at PR 23; the PRs on at least one of them are 1, 2, 3, 4, 6, 7, 8, 9, 10, 12, 13, 14, 15, 19, 20, 21, and 23. The Windows backend (PR 4) is on a longest path, so the supervisor is written only after both backends exist.

**Why the first tests run in PR 15.** Both are end to end (arch §5, issue #2) and need the Windows backend, the encoder, hook ingest, attach, and the Codex launch path. A failure changes only the encoder, the capability table, and the attach paste path, not a contract. PR 5 tests the encoder on both OSes from the start and PR 14 runs FakeCli attach paste tests on `windows-latest`.

**Open items of arch §5.** Codex keyed input: PR 15 (unit tests in 5). Human paste through attach: PR 15 (contract in 11, FakeCli in 14). Codex hook trust renewal: PR 14, 21. Windows hook receiver: PR 9, 23, with the expectations of §2.5. Daemon parent job check: PR 8 (prior-art row 8). Windows build coverage: PR 23; real-CLI runs on more builds need more hosts and stay open until then. prior-art "Not Verified" items that affect behaviour (inbox conhost on other builds, BMP characters delivered only on Alt release) are covered by PR 15 and 23; the rest do not affect the MVP and stay unverified.

**Design obligations.** One line per scenario that the PR's own design and tests must cover; the PR's document decides how.

- **PR 1:** late attributed ack moves `unconfirmed` → `acked`; `SpawnFailed` is `Exited { reason }`, not its own input; superseded cards take precedence over resolution.
- **PR 2:** writer records origin and position of every write; VT responder replies in one write each; wide characters on two cells in snapshots.
- **PR 3 / 4:** FakeCli `spawn-tree` survivors and the unrelated negative control; Windows injected setup failures leave no process or handle (spike F-2c); JOB_LIST branch recorded.
- **PR 5:** paste encoder writes zero bytes on rejected input and never splits an escape sequence (spike F-3d); `win_keys` byte sequences (`ESC[0;0;<utf16>;1;0;1_` + `ESC[0;0;<utf16>;0;0;1_`, Shift+Enter records) over the spike's corpus F; submit delay (300 ms in the spike, prior-art row 4e); encoding chosen by `(agent, os)`, never by DECSET 2004.
- **PR 6:** drop-and-rebuild equality for every projection; `INSERT OR REPLACE`, update, and delete on `events` abort; `transcript_generation` before new-generation lines; stale-cursor batch aborts; blob offload and lossy preview.
- **PR 7:** `limit` 0 and above max, empty / unknown / unreadable stream, `after_seq = head_seq` and above, append between pages, epoch mismatch forward and backward, byte bound, `after_seq` + `before_seq`, `before_seq` at 1, lost notification recovered by `heads`; snapshot contents restore every rendered field; every route × token kind cell; per-resource filtering; `Origin` vs received `Host`; ticket reuse and expiry; secrets absent from logs; receipt HTTP codes.
- **PR 8:** daemon inside a kill-on-close job on Windows CI is detected and handled (breakaway or refusal with a message). The single-instance lock is released only after the store's `ShutdownReport::writer_exited` is true; otherwise the process exits holding it. Store shutdown (`docs/design/event-store.md` §5): `shutdown` blocks the calling thread, so the daemon runs it off the async runtime (e.g. `spawn_blocking`); the first call's deadline is the only drain deadline, so the daemon coordinates one shutdown budget; before calling it the daemon stops producers and lets in-flight read scopes return; process exit need not drain notification receivers; the DB files are touched in-process only when both `readers_returned` and `writer_exited` are true, otherwise only after process exit; committed transactions are recovered from the WAL even if the checkpoint was skipped; `Failed` or `WriterGone` stops the daemon. The state directory sits under the user's profile, which is user-only on Windows (`docs/design/event-store.md` §5). The daemon's process rules, which PR 10 keeps for every child it creates (`docs/design/pty-unix.md` §2.1, §2.4): (1) every child is created inside `chaldea_pty_runtime::with_spawn_lock` (the spawn lock), and no library creates children outside it; (2) the daemon has exclusive ownership of each PTY child's wait status: no wildcard waits (`waitpid(-1)`, `wait`, `waitid(P_ALL)`), no `SIGCHLD` ignore or `SA_NOCLDWAIT`, waits for its other children by exact pid; on loss (`ECHILD`, or a different process at the leader's pid) the backend reports `WaitFailed` or the ownership-lost kill, and the supervisor keeps `death_confirmed: false`; (3) its signal handlers are async-signal-safe and never block (they can run in a forked child before `pre_exec` resets them). On Windows (`docs/design/pty-windows.md` §2.1, §4): `CreatePseudoConsole` and each `CreateProcessW` under the lock are the backend's; the daemon never closes a handle it did not open; a parent job that refuses nesting makes spawns fail, with no uncontained running child.
- **PR 9 / 10:** `extra` identities are attributed to the launch before the daemon passes them to `kill_tree`: a matching creation time identifies a process, not its provenance.
- **PR 9:** `hook_deliveries` keeps the first reply, ordinal, and current routing apart (a retry replays the first reply); the emitter's retry policy (attempts, which failures retry, all inside its budget, one `emit_id`); B-8 cases per layer with per-case deadline, client observation, and no side effect; response ordering and budgets measured from connection start; oversized heads; duplicate equal `Content-Length` and `Transfer-Encoding` outcomes recorded; body held, already sent, and slow; positive controls; a retry with the same `emit_id` gets the first disposition, including after the launch became bound; nested FakeCli (`echo --nested`) does not bind; buffered early Codex hooks are durable before 200 is returned; later hooks re-validate ancestry; a conflicting second `SessionStart`.
- **PR 10:** restart scan of a launch's session by recorded sid with `launch.start_time ≤ start ≤ restart_now`; the crash window between spawn and saving the pid; recorded descendants that left the session; resume and close racing on one thread; a reservation racing a worktree removal; concurrent duplicate commands write once; a late attributable ack after `unconfirmed`; a human write during the writer's text-and-submit unit; the write basis is the writer position (`last_started` of the counted origin classes) stored with the readiness evidence at its intake, and a counted write started after it refuses the input with zero bytes; a hook is an ack candidate only for an input whose write had dispatched at the hook's intake, and the positional prompt is anchored by `launch_started` instead; a delayed hook after a human write does not reopen the gate; `chaldea ready` on an unbound launch writes nothing; restart turns `sending` into `unconfirmed` and `queued` into `cancelled`; gate closed by each missing condition one at a time (no evidence, human write after evidence, `Stop` inside the settle interval, interrupt, DECSET 2004 off, not bound, `readiness_evidence: none` without `ready`) with zero bytes written; gate timeout snapshot; ack arriving while `sending`; ack after a `key` write is not attributable; an ack requires a text match, and a prompt matching no input or two inputs acks none; a card key does not close the gate while a raw key does; evidence of the same turn as a human write does not reopen the gate; a crash between reservation and `launch_started` leaves the positional prompt `unconfirmed`; buffered hooks of a launch that can no longer bind become `raw_unknown` at restart; write-boundary recheck after exit; a fake banner never changes state; the PR 8 process rules hold for every child the supervisor creates. On both OSes the stop, close, and resume paths pass the launch's full list of recorded descendants on every `kill_tree` call, the retry after `windows_busy` / `unix_busy` or `tree_dead: false` included, never the report's survivor list; the supervisor retries through `Session::kill` while the session is available, closes explicitly with the full list, and keeps the launch unresolved when that consuming `close` cannot prove death; an implicit `Session` drop is best-effort cleanup, never a death proof. On Windows the old tree is terminated by the daemon's job handle closing, which is not a death proof, and a daemon crash between a suspended `CreateProcessW` and `AssignProcessToJobObject` leaves a suspended, unrecorded child outside any Chaldea job.
- **PR 11:** outer-terminal paste boundaries verified first on both OSes; a paste split across reads; a recognised paste is written as text only and a physical Enter submits; a recognised paste that cannot be framed or encoded is rejected with zero bytes written; terminal modes restored on detach, Ctrl-C, EOF, and panic; attach writes produce `human_write`.
- **PR 12:** capture refuses an empty fixture set; envelope and payload presence per event; scrubbing of tokens, home paths, and non-corpus prompts; per CLI version: `PermissionRequest` fields and its order against `PreToolUse`, `Stop` continuation behaviour, the settle interval, and any post-decision signal, Codex `Stop` with `last_assistant_message` null and its transcript, Codex `Interrupt` payload, Codex resume hooks, Claude `prompt_id` presence.
- **PR 13 / 14:** real decoders over the fixtures; continuity evidence persisted so that a truncate-and-regrow of the same `file_id` is still detected after a cursor rebuild, tested across a rebuild; bind and cursor advance under one update condition; reread tests on real files; Codex assistant text from the transcript when `last_assistant_message` is null; approval correlation and supersession (§2.10), with a `vt_snapshot_taken` recorded when a card opens; Codex `Interrupt` decoded into the interrupt input; Codex positional prompt ack with a startup stall; zero submit or queue during an attach paste and exactly one submit after a physical Enter (Codex on Windows, FakeCli); herdr's paste-boundary key (`src/app/api/agents.rs:14-33`) kept as the fallback if `tui.disable_paste_burst=true` falls short.
- **PR 15 / 23:** per-row evidence: P-1 settings diff, P-2 registered vs observed hook events (spike A-5), P-3 MCP witness attested before stop (spike A-8), P-4 worktree after restart, P-7 settle interval after `Stop` (spike F-1a), P-11 real IMEs, P-13 query log, P-14 malformed and nested payloads, P-16 superseded card, P-17 per CLI version, P-18 survivor list with the macOS containment limit reported, P-20 probe list, P-23 wrong-target action without effect, P-25 a calibrated window observer (uncalibrated and nothing seen is `inconclusive`), P-27 viewport screenshots.
- **PR 16:** each refusal case of §2.6; agent `--force` refused; git failing after it started changing files gives `uncertain` and reconciles before retry; reconcile git admin data and the real path; removals serialized per worktree; `--force` keeps the in-use refusal; a reserved launch and a launch with unconfirmed death count as in use; a worktree in any state but `active` or `released` (including `removed`) refuses a new reservation; `task set` from a grandparent is refused; crash between `git worktree add` and the record; a sharing violation right after a confirmed Windows stop (a job member's file handles can be released a little after `tree_dead`) is absorbed by the `uncertain`/retry path.
- **PR 18 / 19:** a response from an older request generation is discarded and never sets the epoch; a key sent from an approval card carries `launch_id` and `approval_id` and is refused (409) at the write boundary when that card is no longer open; unknown kind and newer `schema_version` render generically; pair fragment removed from the URL; IME guards (`isComposing`, `keyCode === 229`, Enter after `compositionend`) on desktop and mobile viewports.
- **PR 20:** the one-shot hook timeout (10 s proposed); one-shot hooks get the documented input within it, `attention` on raise and clear; `POST /events` scope rules; rescue has no direct PTY write; Workspace-scoped `surface_action` membership check; a Thread-scoped action naming another thread has no effect.
- **PR 21:** the ConPTY module check runs after a real PTY spawn; a deliberately broken check reports `fail` or `inconclusive`, never `pass`.

## 5. Detailed Design: PR 1

**Goal.** A Cargo workspace whose `crates/core` holds the contracts of §2.1–§2.4 as types and pure functions, generated TypeScript and JSON Schema committed with a drift check, and a CI job on macOS and Windows.

**Types and functions:**

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
pub enum TypedEvent { ThreadRegistered(..), ThreadClosed(..), LaunchReserved(..), LaunchStarted(..), LaunchExited(..),
    LaunchDeathConfirmed(..), SessionBound(..), TranscriptGeneration(..), ReadinessObserved(..), HumanWrite(..),
    UserMessage(..), AssistantMessage(..), ToolActivity(..), StatusChanged(..), ApprovalRequested(..),
    ApprovalResolved(..), InputCreated(..),
    InputStateChanged(..), MailboxMessage(..), TaskSet(..), SurfaceUpdate(..), VtSnapshotTaken(..),
    WorktreeChanged(..), RawUnknown(..) }
pub struct GenericEvent { pub envelope: EventEnvelope, pub why: GenericReason }  // UnknownKind | NewerSchema | PayloadInvalid
impl TypedEvent { pub fn decode(e: &EventEnvelope) -> Result<TypedEvent, GenericEvent>; }
pub enum Content { Inline { inline: String, encoding: Encoding }, Blob { blob: BlobRef, preview: String, preview_lossy: bool } }

pub enum StateInput {                                                     // no variant carries PTY output
    Reserved { relaunch: bool }, Spawned, SessionBound, StopRequested,
    Exited { reason: ExitReason }, DeathConfirmed, ReadinessObserved, PromptSubmitted,
    Interrupted,                    // only from an explicit interrupt event (Codex `Interrupt` hook)
    TurnStopped,                    // turn -> unknown; ReadinessObserved then sets idle per `readiness_evidence`
    ApprovalOpened, ApprovalsAllResolved,
    AttentionRaised(AttentionCause), AttentionCleared(AttentionCause), TaskSet { to: TaskState, by: Actor },
}
pub struct ThreadState { pub process: ProcessState, pub turn: TurnState, pub task: TaskState, pub attention: BTreeSet<AttentionCause> }
impl ThreadState { pub fn registered(task: bool) -> ThreadState; }      // the state `thread_registered` creates; task `active` when given, else `none`
pub fn reduce(s: &ThreadState, input: &StateInput) -> Result<(ThreadState, Vec<StatusChanged>), InvalidTransition>;

pub enum InputState { Queued, Sending, Sent, Acked, Unconfirmed, Rejected, Cancelled, Dismissed }
pub enum InputInput { GateOpened { sent_after_seq: u64 }, WriteCompleted, WriteOutcomeUnknown, Rejected(RejectReason),
    Cancelled, RecheckFailed, LaunchExited, DaemonRestarted, AckAttributed, AckNotAttributable, AckWindowExpired, Resolved { delivered: bool } }
pub fn step(state: InputState, input: &InputInput) -> Result<InputState, InvalidTransition>;
pub fn gate(g: &GateView) -> GateDecision;    // Open | Closed(reason); GateView holds bound, process_state, evidence and its kind (incl. owner_asserted), last write of any origin, last gate-closing human write, last_turn_start, turn, in-flight, 2004

pub fn decide_replay(q: &EventsQuery, head: StreamHead, epoch: &Epoch) -> Result<ReplayPlan, QueryError>;
pub enum ReceiptLookup { New, InProgress, Replay(CommandResult), Conflict, UnknownAfterCrash }
pub fn check_receipt(existing: Option<&CommandReceipt>, token: TokenId, request_sha256: &[u8; 32]) -> ReceiptLookup;
```

**Tests** (macOS and Windows):

- `reduce` proptests: never panics; after `ThreadState::registered(task)`, `task` changes only on `TaskSet`; after `Exited`, `turn` is `unknown`; each attention cause is cleared only by its own clearing input; `ApprovalsAllResolved` sets `running` only from `awaiting_approval`, and `TurnStopped` / `Interrupted` / `Exited` win over open cards; `TurnStopped` alone never sets `idle`; `DeathConfirmed` clears `resume_blocked`; every change appears in exactly one `StatusChanged`.
- `step` proptests: `Sending` is entered at most once; nothing leads from `Sending`, `Sent`, or `Unconfirmed` back to `Queued` or `Sending`; `Rejected` from `Queued`, `Cancelled` from `Queued` or from `Sending` on `RecheckFailed`; `Rejected` also from `Sending` on a confirmed spawn failure of a positional prompt; `DaemonRestarted` and `LaunchExited` map `Sending`/`Sent` to `Unconfirmed` and `Queued` to `Cancelled`; `AckAttributed` moves `Unconfirmed` to `Acked`; `Dismissed` and `Resolved { delivered: true }` only from `Unconfirmed`.
- `gate` table: open only with evidence newer than the last write and every other condition; each condition missing alone closes it.
- `decide_replay` table: each fallback reason, `after_seq = head`, empty stream, absent epoch, `after_seq` with `before_seq`, `limit` bounds.
- `check_receipt`: every `ReceiptLookup` case, including another token with the same hash.
- Decoding: unknown kind, newer `schema_version`, and invalid payload each give `GenericEvent`; golden round trips for every kind, both `Content` forms and encodings, `EventsResponse` both modes, WS messages.
- `contracts_export`: committed TS and JSON Schema equal the generated output; the regenerate command is documented in the test.

**Completion commands.**

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum) ' <<<"$deps"
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
| P-5 | If removal is requested, without an applicable owner override, for a worktree with uncommitted changes, ignored files, unpublished commits (branch or detached `HEAD`), or a git error during the checks, then Chaldea shall refuse and leave worktree and branch unchanged; if git fails after it started removing, the result shall be reported as uncertain, never as a refusal. | arch §1; invariant 3 | Pass: refusal with reason, or `uncertain`. Fail: anything removed after a refusal, or a partial removal reported as refused. | Real repo per case. |
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
| P-16 | When the CLI requests an approval, the PWA shall show it and let the user act on it, and a card the CLI no longer waits on shall be closed. | issue #2; arch §1 | Pass: card, action reaches the CLI, closed card disabled. Fail: otherwise. | Real Claude permission (§8 D1). |
| P-17 | When a Claude or Codex turn ends, the thread shall show the assistant's final message. | arch §1 | Pass: `assistant_message`. Fail: missing, shown visibly as missing when neither `Stop.last_assistant_message` nor a transcript has it; never filled from the screen. | Real CLIs per version. |
| P-18 | When the daemon is killed and restarted, running threads shall show `exited` (`daemon_restart`) with history intact and no surviving old process except the documented macOS containment limit (a `setsid` grandchild whose parent exited, spike §4.6), which is reported, and a resume shall continue the session. | arch §2 | Pass: equal history, survivors none or reported as that limit, resumed context. Fail: lost or duplicate events, other survivor, resume fails. | Kill by PID mid-turn. |
| P-19 | When the PWA reconnects after the daemon was unreachable, it shall show the time of last sync while offline, catch up without duplicate or missing events, and keep older history readable. | arch §4.1 | Pass: banner, equal list, paging works. Fail: otherwise. | Stop and start the daemon. |
| P-20 | If a request has a wrong `Host`, a missing or wrong token, or a scope it lacks, then the daemon shall reject it and change nothing. | invariant 5 | Pass: 421/401/403/404, no event. Fail: accepted. | `curl` probes. |
| P-21 | When a tool output larger than 64 KiB is recorded, the thread shall show its preview and offer the full content as a download. | arch §4.2 | Pass: preview, equal download. Fail: silent truncation. | Real large output. |
| P-22 | When a configured one-shot hook runs, it shall receive the documented JSON, and events it posts to `POST /events` shall appear in the scoped stream. | arch §4.4 | Pass: input logged, surface visible. Fail: otherwise. | Sample hook script. |
| P-23 | When a Rescue Agent emits a Thread-scoped panel with a button, the action shall run only after a human clicks it, and only on that thread. | arch §1, §4.4 | Pass: action with the human's command id. Fail: otherwise. | Real Claude as Rescue Agent. |
| P-24 | Where a workspace sets a short worktree base path on Windows, an MSVC Rust project shall build inside a Chaldea worktree. | arch §5 | Pass: build succeeds. Fail: `LNK1104`. | Real repo under `C:\wt`. |
| P-25 | While Chaldea spawns CLIs and runs hooks on Windows, no console window shall appear. | prior-art row 9; spike C-5 | Pass: a calibrated observer sees no window. Fail: a flash. Uncalibrated and nothing seen: inconclusive. | PR 23. |
| P-26 | When `chaldea doctor` runs, it shall report each check as pass, fail, or inconclusive with evidence, and inconclusive (never pass) for any check it could not run. | arch §2, §6 | Pass: matches the host, broken check not passing. Fail: pass without evidence. | Both hosts, hook port blocked once. |
| P-27 | Chaldea shall be usable on a phone-sized viewport for reading threads, short prompts, approvals, and the keypad. | arch §1 | Owner evaluation; Android unverified until Phase 2. | Playwright mobile viewport + manual review. |

### 6.2 Plan Layer: Claims the Contracts Make

| ID | Claim (EARS) | Source | Falsifier |
|---|---|---|---|
| CL-1 | The thread state reducer shall accept only typed explicit inputs, none built from PTY output. | invariant 1; §2.2 | A `StateInput` variant carrying PTY bytes; PR 10 fake banner changing state. |
| CL-2 | After registration, `task_state` shall change only on `task_set`. | §2.2 | PR 1 proptest. |
| CL-3 | Each attention cause shall be cleared only by its own clearing event, without changing `turn_state`. | §2.2 | PR 1 proptest. |
| CL-4 | An input shall enter `sending` at most once and never return to `queued` or `sending`. | arch §4.3; §2.3 | PR 1 proptest; PR 10 write count. |
| CL-5 | A queued input shall be written only with positive readiness evidence newer than the last write of any origin, revalidated by the writer before the first byte; otherwise zero bytes shall be written. | §2.3 | PR 1 `gate` table; PR 10 one-condition-missing and race tests. |
| CL-6 | A daemon restart or launch exit shall turn `sending`/`sent` inputs into `unconfirmed` and `queued` into `cancelled`, and only inputs of the current launch shall block the gate. | §2.3 | PR 1 proptest; PR 10 restart test. |
| CL-7 | An ack shall be attributed only to an input whose expected form the prompt equals, by stream position within one launch, and only when no write of another origin happened in between. | §2.3 | PR 10 interleaving tests. |
| CL-8 | One `command_id` shall cause at most one effect, and a repeat by its token shall return the stored result. | arch §4.1; §2.4 | PR 7 receipts; PR 10 concurrent duplicates. |
| CL-9 | Each stream's `seq` shall be gapless and monotonic, and no statement shall update or delete an event. | §2.1 | PR 6 tests; PR 17 model. |
| CL-10 | A replayed source shall not create a duplicate event; a hook retry shall get its first delivery's disposition; distinct `emit_id`s shall each create one. | §2.1 | PR 6 key test; PR 9 HTTP test. |
| CL-11 | Projections rebuilt from events shall equal the maintained projections. | §2.1 | PR 6 drop-and-rebuild. |
| CL-12 | Transcript events and the cursor shall commit together, only from the bound session's path, only when the stored cursor matches, and only after their generation is recorded. | arch §4.2; §2.1 | PR 6 / 13 fault injection. |
| CL-13 | A client following `decide_replay`, `before_seq`, subscribe-time heads, and periodic `heads` shall end with the server's events and current surfaces, without duplicates, after any disconnects, restarts, and lost responses. | arch §4.1, §6 | PR 7 edge cases; PR 17 model; PR 18 fast-check reducer. |
| CL-14 | Readers shall render unknown kinds and newer schema versions as `GenericEvent`, and generated TS and JSON Schema shall equal what the Rust types generate. | §2.1 | PR 1 decode and drift tests; PR 18. |
| CL-15 | Every route shall enforce the route and action matrices, including object-level checks. | §2.5 | PR 7 matrix tests; PR 20 rescue tests. |
| CL-16 | Rejected requests shall get the outcome of §2.5 for their layer and create nothing. | invariant 5; spike B-8 | PR 9 B-8 cases on both OSes. |
| CL-17 | A hook shall bind only through §2.9's rule; a nested CLI inheriting the environment shall not bind. | prior-art row 6c; spike B-11 | PR 9 nested FakeCli. |
| CL-18 | A new launch shall be reserved only against the expected current launch, after its death is confirmed, on a thread that is not closed and a worktree that is `active` or `released`. | §2.6, §2.9, §2.11 | PR 10 restart and race tests; PR 16 removal race. |
| CL-19 | The text encoding shall be chosen by `(agent, os)`, never by DECSET 2004; an attach paste shall never carry a submit. | spike F-1f; §2.3 | PR 5 table test; PR 11 paste tests. |
| CL-20 | An approval card shall resolve only on evidence correlated to it, and close on `Stop`, interrupt, or launch exit. | §2.10 | PR 13 FakeCli sequences. |
| CL-21 | No code path shall remove a worktree except `worktree remove` passing its checks or owner `--force`, which keeps the in-use check. | arch §4.3; §2.6 | PR 16 tests. |
| CL-22 | The PWA prompt input shall not submit while `isComposing`, on `keyCode === 229`, or on the Enter right after `compositionend`. | invariant 4; arch §2.4 | PR 19 Playwright. |
| CL-23 | A hook emitter run shall exit 0, write nothing to stdout, and finish within 3 s, whatever the daemon does. | arch §2; spike B-9 | PR 9 emitter tests. |
| CL-24 | `core` shall build and pass its tests without tokio, filesystem, network, process, or clock access. | §3 | CI dependency check; clippy list. |

## 7. Verification Plan

| arch §6 item | What | PR |
|---|---|---|
| Real-CLI contract corpus (`fixtures/`) | `fixtures/<agent>/<cli-version>/<case>/`: hook payloads (`<Event>-<n>.json` + allowlisted `.meta.json`, spike §4.3) and transcript excerpts, captured by `chaldea doctor capture` and scrubbed. PR 12 validates envelopes and payload presence; the adapter PRs decode every fixture, and a known event decoding to `raw_unknown` fails. New CLI versions add a directory. | 12; 13, 14 |
| PBT (`proptest` / `fast-check`) | reducer, input machine, gate (1); encoders (5); projection rebuild (6); client reducer (18). | 1, 5, 6, 18 |
| `stateright` | Sync Seam only: crash during append, offline client buffering, retry after a lost response. Properties: no gap, no duplicate, one effect per `command_id`. | 17 |
| `cargo-fuzz` | JSONL line parser, hook payload decoder, VT responder (replies in one write). Nightly, scheduled, seeded from `fixtures/`. | 22 |
| Cross-OS CI | macOS + Windows from PR 1; FakePty / FakeCli in every PR; Windows injections in PR 4. | 1 onward |
| `chaldea doctor` | Host/token round trip; hook round trip through `hook-emit`; FakeCli in a PTY with a VT query, paste echo, and tree kill; Windows parent job; ConPTY module after that spawn; CLI versions and Codex trust state. | 21 |
| Playwright | Desktop and mobile viewports; IME composition sequences; send, approval, keypad against FakeCli. | 18, 19 |
| Real-host E2E | §6.1 on macOS and Windows with versions recorded. | 15, 23 |

**Verdicts of evidence-collecting harnesses.** `chaldea doctor`, `doctor capture`, and the real-host runs decide their verdict in one function, `core::verdict::evaluate(plan: &CheckPlan, evidence: &[Check]) -> Verdict`, lifted from `spikes/pty-hooks/src/verdict.rs`. Outcomes are typed: `Pass`, `PlatformFail { cause }` (positive evidence of what the CLI or OS did), `HarnessFail { cause }`, `Missing`, `NotExercised { reason }`.

- The plan (required evidence keys and expected context: run, launch, OS build, CLI version) is fixed before observation. An empty plan is `Inconclusive`.
- Each required key must appear exactly once, with its content checked; a second observation of the same key, even with the same outcome, is `HarnessFail`. This replaces the spike's "last check per key wins" (`verdict.rs:116-135`). Evidence outside the plan is recorded and does not affect the verdict, also a change from the spike.
- A context mismatch or null evidence is `HarnessFail`.
- Completeness first: any required key `Missing`, `HarnessFail`, or `NotExercised` → `Inconclusive`; else `Pass` when all passed, else `Fail`. Missing evidence, an observation failure, or a check that did not run is never `Pass` (spike §1).

## 8. Decisions

Decisions that change public behaviour or are irreversible. D1, D3, D7, and D8 were decided by the owner on 2026-11-22; D2, D4, D5, and D6 are reversible or additive and were decided on the facts, recorded with the condition that would reopen them.

**D1. Approvals are answered with the keypad** (owner). The card shows tool and arguments plus the VT snapshot, and the user answers with keys (§2.10); it works the same for both CLIs and needs no emitter change. A key sent from an open card answers the dialog, not the composer, so it is recorded as a `card_key` and does not close the input gate (§2.3). Rejected alternative: a hook decision, where `hook-emit` waits for the user and prints the CLI's decision JSON (Claude `hookSpecificOutput.decision.behavior`, hooks reference; Codex allow/deny, `hooks/src/events/permission_request.rs:55-58`). It would give one-tap answers but needs the emitter's 3 s budget (CL-23) and `/hook`'s 5 s deadline lifted for human time, and its answer is still only one input to the CLI's final decision. Switch later: add an `approval_answered` kind (additive), the `PermissionRequest` exception to CL-23 and the `/hook` deadline, and a long-poll path. Codex hashes the hook timeout with the handler (`hooks/src/engine/discovery.rs:715-718`, `:847`), so PR 14 registers the `PermissionRequest` entry with its final command string and timeout now, and the switch costs no hook review.

**D2. `chaldea task set <state> [--summary] [--thread]` sets `task_state`.** Explicit and greppable in agent instructions; issue #2's command list is not closed. Rejected: a `--task-state` flag on `mailbox send`, which couples a state change to a message. The commands this design adds beyond issue #2 are `open`, `ready`, `resume`, `close`, `input resolve`, `input cancel`, `task set`, `worktree remove`, `daemon run/start/stop`, and `doctor capture`.

**D3. The eight input states of §2.3 are public** (owner). Crash recovery and human resolution stay exact, and `unconfirmed` means only "may have reached the CLI", which the never-resend rule depends on. Rejected alternatives: the architecture's four states with a reason on `unconfirmed` (every reader must check the reason before treating it as a possible delivery, and the gate must skip zero-byte cases), and a mapped public set with a terminal `closed` (a mapping layer for little gain).

**D4. `before_spawn` fails open by default, with a per-hook `fail_closed = true` key.** A broken extension never blocks all spawns, and policy hooks can still fail closed; doctor lists fail-closed hooks and their last failure. Reopen if `before_spawn` turns out to be used mainly for security policy.

**D5. A Codex `spawn` requires a first prompt.** Every spawned launch is controllable from turn 1 (arch §5). A Codex `resume` without `--prompt` stays a manual launch (§2.9). Known costs: the prompt is in argv and shows in process listings, and Windows limits a command line to 32,767 UTF-16 units (`CreateProcessW` docs; the usable length after quoting is unmeasured). A manual `spawn` with the resume path's behaviour can be added later; it is additive.

**D6. Codex-on-Windows text with trimmable leading/trailing whitespace or TAB is rejected** with zero bytes written (spike F-1d), for `send` and recognised attach pastes. Codex trims such whitespace and takes TAB as its queue key (arch §5), so sending it would change the prompt silently. An explicit transform (`--transform trim,tab-to-spaces`) can be added later if rejections prove frequent; it is additive.

**D7. A normal worktree remove refuses when ignored files exist** (owner), unless the owner passes `--discard-ignored` or `--force`; agents can pass neither. `git worktree remove` would delete ignored files such as `.env`, and deleted files are unrecoverable (arch §1, invariant 3). Cost: nearly every built worktree (`target/`, `node_modules/`) needs the flag.

**D8. Readiness after a turn defaults to `stop_settle`** (owner), with `post_decision` and `none` selectable per adapter (§2.3, §2.7). Orchestration runs unattended across turns. Known race: a user `Stop` hook that blocks after the settle interval makes the next prompt land in a running turn; the input is then attributed or left `unconfirmed`, never resent. Switching the default is a capability-table change.

**Also decided.**

- Raw PTY writes (`key`), attach, and the recovery commands `ready` and `input resolve` (which write no bytes) are owner-only; agents keep `send`, `stop`, and the other rows of the action matrix (§2.5). Rescue acts on a PTY only through scoped A2UI `surface_action`s that a human clicks (arch §4.4). Reason: a raw key can approve a dialog or submit into another thread's composer; granting more later is additive.
- A resume is refused until the old launch's death is confirmed (§2.9). Reason: two live CLIs on one session corrupt it; refusing is safe and can be relaxed.
- B-8 expectations follow RFC 9112: `Content-Length: +5` → 400; a slow head → connection closed within its budget, observed as EOF, never inferred from a client timeout or `status: 0` (§2.5). Reason: the spike's 411 contradicts RFC 9112 §6.3, and an incomplete request has no response to observe.

## Proposed Canonical-Doc Changes

Not applied here; proposals for `docs/architecture.md`, `docs/prior-art.md`, `AGENTS.md`.

- **arch §4.4**: Rescue acts on PTYs through human-clicked `surface_action`s only; `attention` is an outbound hook on raise and clear.
- **arch §2 / AGENTS.md invariant 5**: the token kinds (`owner`, `agent`, `hook`, `rescue`, `extension`) next to "regular vs Rescue".
- **arch §5 open items**: the Windows hook receiver item concerns the axum receiver; keep `deadline_slow_head` (EOF, no side effect) and `non_numeric_content_length` (400) as checks to run on CI and the real host.
- **prior-art**: rows for event store and replay (T3 Code v1/v2, `decideThreadResume`), command receipts, scoped auth and pairing (T3 Code), thread/turn status and schema fixtures (Codex app-server), worktree removal checks (T3 Code vs herdr), herdr's Codex-on-Windows paste-boundary key (`src/app/api/agents.rs:14-33`), `PermissionRequest` without a tool id in both CLIs, and Codex's trust hash covering the hook timeout.
