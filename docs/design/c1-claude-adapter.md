# C1: Claude Code Adapter (lite) and the Rescue Read Surface

Status: design for mvp §4 row C1 (carries PR 12's Claude fixture checks and PR 13). It makes a real Claude Code launch bind, take prompts, open and close approval cards, record assistant text, and resume, and it adds the external agent's read surface (`GET /api/threads/{thread_id}/screen`, `chaldea inspect`). Contracts it builds on: mvp §2.3, §2.7, §2.9, §2.10, §4 "Design obligations" (C1 rows); `docs/design/hook-ingest.md` §4 row 13 and D2, D3; `docs/design/supervisor-send.md` §2.5; `docs/design/input-encoders.md` §2.4, D4; `docs/design/event-store.md` §2.3 `append_transcript`. Identifiers marked *new* do not exist at main.

## 1. What already exists and what C1 adds

At main a Claude launch already gets `--session-id <launch_id>` or `--resume <session>` (`core::launch::plan`), the hook token and env, the session-id binding rule (`MvpHooks::rule`), the bracketed paste send and gate (10b's send driver with `Guard::bracketed_paste`), card state effects (`SupervisorHooks`), `append_transcript` with generations and cursor checks, and `LiveLaunch::snapshot_before`. The daemon's decoder is `NoDecoder`, so a real Claude sends no hook and never binds (supervisor L-4).

C1 adds, and only adds:

- **Hook injection.** The daemon writes one settings file, `<state_dir>/claude-settings.json`, atomically at startup before serving; every Claude launch gets `--settings <that path>` (`LaunchInput::claude_settings`, *new*). It registers `SessionStart`, `UserPromptSubmit`, `Stop`, `PermissionRequest`, `PreToolUse`, `PostToolUse`, each running `<current_exe> hook-emit claude` with the path quoted for the host's hook shell.
- **Decoder.** `ClaudeDecoder` (*new*), a `HookDecoder` for `Agent::Claude`, behind a per-agent dispatch that keeps `NoDecoder` for Codex until X1. `PreToolUse` and `PostToolUse` also yield `tool_activity` events.
- **Panic isolation.** `SupervisorHooks::interpret` runs `decode` inside `catch_unwind`; the tailer does the same per line. A panic yields `raw_unknown` with reason `schema_mismatch` and the exact bytes.
- **Coarse card resolution.** `HookFact::CardEvidence` carries `tool_name` and `tool_input` with an optional `tool_use_id` (internal change). A `PostToolUse` resolves (`ran`) the oldest open card of the launch with equal `tool_name` and equal canonical `tool_input`; a correlated card still matches by `tool_use_id` first.
- **Transcript tailer** (`TranscriptTailer`, *new*, daemon): one task per live bound launch with a transcript path; polls every 1 s and once more after `launch_exited`; opens the path through `chaldea_pty_runtime::file::open_regular` (*new*: absolute path only, non-blocking open, Windows device and pipe namespaces refused before the open, regular-file check and `file_id` on the opened handle, reads through that handle). Assistant lines with text become `assistant_message`; other known line types produce no event; unparseable lines become `raw_unknown`.
- **Read surface.** The route and `chaldea inspect` (§3).
- **Fixtures** under `fixtures/claude/<cli-version>/<case>/`, captured by hand (§4).

## 2. Acceptance criteria

Checks: **FC** = FakeCli/FakePty test in CI on macOS and Windows; **P-n** = the M1 dogfooding row with real Claude on macOS (Windows at M2).

| ID | Criterion (EARS) | Check |
|---|---|---|
| C1-1 | When a Claude launch is reserved, its argv shall carry `--settings <state_dir>/claude-settings.json` and either `--session-id <launch_id>` or `--resume <bound session>`, and the user's own settings files shall stay byte-equal. | FC (`plan` unit test; settings file content test); P-1 |
| C1-2 | When real Claude starts in a trusted folder, its `SessionStart` shall bind the launch and record `readiness_observed { session_start }`, and every registered event the CLI fires shall reach the decoder. | FC (fixture payloads through FakeCli `hooks`); P-2 evidence (registered vs observed events) |
| C1-3 | For every committed fixture of a registered event, the decoder shall return a fact other than `Other` or at least one event; a fixture that decodes to `raw_unknown` shall fail the test. | FC (fixture test) |
| C1-4 | If a decoder panics on a hook payload or transcript line, then the daemon shall store that payload or line as `raw_unknown` (`schema_mismatch`, exact bytes) and keep serving. | FC (a panicking test decoder; the daemon keeps answering) |
| C1-5 | When the spike corpus is sent through `chaldea send` (or `spawn --prompt`) to a ready Claude, the recorded prompt shall be accepted by `prompt_matches` for its text and the input shall end `acked`. | FC (fixture prompts against `prompt_matches`); P-6 |
| C1-6 | If an input is not acknowledged within the ack window, then it shall end `unconfirmed` with attention and be written once. | FC (existing 10b tests with `ClaudeDecoder`); P-15 |
| C1-7 | When `PermissionRequest` arrives, the thread shall show an open card and `awaiting_approval`; when a matching `PostToolUse` arrives, that card shall resolve `ran`; when `Stop` or a bound `UserPromptSubmit` arrives, or the launch exits, every open card shall close `superseded` or `expired`. | FC (sequences, CL-20); P-16 with `chaldea key --approval` (key bar: T2) |
| C1-8 | While a launch is bound with a transcript path, the tailer shall append each assistant text line as `assistant_message` with its cursor advance in one transaction, and shall start a new generation when the path or `file_id` changes, the file is shorter than the cursor, or the continuity window differs, including after a cursor rebuild. | FC (real files in the OS temp dir: append, truncate-and-regrow of the same `file_id` across a rebuild, replace) |
| C1-9 | If the bound transcript path is relative, a FIFO, a device, or not a regular file on the opened handle, then the tailer shall read nothing and block nothing. | FC (FIFO on macOS, `\\.\` path on Windows) |
| C1-10 | When the owner resumes a thread whose last launch's death is confirmed, Claude shall start with `--resume <session>`, bind the same session, and the tailer shall continue the same generation from the carried cursor. | FC (FakeCli resume); P-18 |
| C1-11 | When a token that can read a thread requests `GET /api/threads/{thread_id}/screen` while the thread has a live launch, the daemon shall return its current screen and append no event; otherwise 404 (unreadable), 409 `no_live_launch`, or 503 `busy` (snapshot not taken in 250 ms). | FC (route × token matrix, CL-15; FakePty screen); P-12 legibility |
| C1-12 | When `chaldea inspect <thread>` runs, it shall print the thread's state, attention causes, launch, session, non-terminal inputs, open cards, recent events, and the screen; with `--json` it shall print one `InspectOutput` object; it shall exit 0, or 1 when the thread is unreadable or the daemon unreachable. | FC (CLI against a test daemon); the external-agent dogfooding entry |
| C1-13 | While Claude stalls on a startup prompt without `SessionStart`, the thread shall carry `session_start_timeout`, the daemon shall write no byte, and `inspect` shall show the prompt. | FC (FakeCli `stall`); P-12 (clearing through the key bar: T2) |
| C1-14 | If a bound launch receives a `SessionStart` with another session id (`/clear`, in-session `/resume`), then it shall be stored as `raw_unknown` `foreign_session` and the binding shall stay. | FC (fixture); P-2 evidence |

P-11 (IME) is W2's; P-7 here is only its evidence item "settle interval after `Stop`", recorded for Claude during P-6.

## 3. Decisions

Public contract items are marked **public**.

| # | Decision | Alternative rejected |
|---|---|---|
| D1 | **public** `GET /api/threads/{thread_id}/screen`: a new `ROUTES` row (`owner_pr` 13), credential `READERS`, object check "readable thread" as `GET /api/threads/{thread_id}` (agents: self+, from O1). Body `ScreenResponse` (*new*, `core::sync`, ts-rs): `{ thread_id, launch_id, cols, rows, cursor, modes, lines }`, `lines` = one string per row, cells concatenated, trailing blanks trimmed. Errors reuse existing codes: 404, 409 `no_live_launch`, 503 `busy` with `retry_after: 1`. | Owner only: an orchestrator could not diagnose its own children; the route is a read, and terminal writes stay owner-only. The `vt_snapshot_taken` cell grid as the body: every caller would rebuild text. |
| D2 | **public** `chaldea inspect <thread> [--json]`: reads `GET /api/threads/{thread_id}`, the thread snapshot through `GET /api/events` with `after_seq` above the head (the documented `cursor_ahead` fallback), and the screen. `--json` prints `InspectOutput` (*new*, `core::sync`): `{ summary, snapshot, screen \| null, screen_error \| null }`. A missing screen is not a failure. | A new snapshot query parameter or route: adds wire surface for what the documented fallback already returns. |
| D3 | **public** Claude argv gains `--settings <state_dir>/claude-settings.json` (so `argv_digest` changes for Claude). One file per daemon start, written by rename. | One file per launch: needs cleanup at exit and after crashes, for content identical across launches (per-launch values travel in env). |
| D4 | Registered Claude events are the six of §1; `SessionEnd` and `Notification` are left out. | Registering all eight of mvp §2.7: no event kind carries them, so every one would be `raw_unknown`; Claude hashes no hook list, so adding them later costs nothing. |
| D5 | Session switches (hook-ingest D2): a launch keeps its first binding; a later same-session `SessionStart` (`compact`) stays `raw_unknown` `unknown_event`. | Following the new session: a repeatable `session_bound`, a gate turn start, a tailer generation, and a trigger migration for a case the owner can recover by a new thread. |
| D6 | Claude keeps rule `SessionId` (hook-ingest D3); no ancestry check is added. | Switching to `Ancestry`: needs real-host chain verification first and only separates a nested Claude resuming the same session id. |
| D7 | A decoder panic is `raw_unknown` reason `schema_mismatch`. | A new reason `decoder_panic`: a public enum value for a case the daemon log already distinguishes. |
| D8 | Claude ack matching keeps input-encoders D4's lenient set; the captures become a test that each recorded prompt is in the accepted set. A capture outside it fails the test and changes `prompt_matches`. | Narrowing to the captured form: changes which sends end `acked` with no observed failure to fix. |
| D9 | No `vt_snapshot_taken` when a card opens. | Recording one: `interpret` is data only and cannot take a snapshot; the screen route serves the live dialog. |
| D10 | Continuity evidence is `sha256:<hex>:<len>` of the last up to 4 KiB before the offset, re-read through the open handle on every poll. | A whole-prefix hash: re-reads the whole file per poll. |
| D11 | Claude's `cli_version` stays `null` in `launch_started` and `raw_unknown`; fixtures record the version in their path. | A `claude --version` probe per spawn: an extra child under the spawn lock and a startup hang risk; X1 adds a probe if trust renewal needs one. |
| D12 | `GET /api/adapters` gains no readiness column. | Adding it: every row is `stop_settle`, so the field would carry no information. |
| D13 | `open_regular` and `FileId` live in `chaldea_pty_runtime::file` (*new*), as `process::ancestry` does, so OS-specific code stays in pty-runtime. | `cfg` code in the daemon. |

## 4. Fixtures

Captured by hand from a real launch: with injection in place and the decoder still `NoDecoder`, every hook arrives as `raw_unknown` with its exact bytes; the developer exports them from `GET /api/events`, scrubs them, and commits `<Event>-<n>.json` plus an allowlisted `.meta.json` (spike §4.3), and transcript excerpts as `transcript.jsonl`. Cases: a plain turn; the corpus prompts of input-encoders §2.4 (compatibility ideograph, spaces and LF at the ends, TAB, wrapped body with TAB and marks, DEL, C1, leading `/`, `!`, `#`); an approved and a denied permission; `/clear` and `/compact`; a resume. A test fails when a fixture holds a home-directory shape (`/Users/`, `/home/`, `C:\Users\`) or a token-shaped string.

## 5. Risks

| Risk | Detection | If accepted: the limit, and why the purpose holds |
|---|---|---|
| `SessionStart` fires before the folder-trust prompt is answered, so readiness opens the gate and a queued prompt is pasted into the dialog whose default is "No, exit". | P-12 in a fresh untrusted folder; fixture event order. | Not accepted: if observed, the Claude rows switch to `ReadinessMode::OwnerOnly` (a table change); the gate's DECSET 2004 condition is a second guard. |
| `--resume <session>` starts a new session id, so the resumed launch never binds. | P-18; `session_start_timeout`; `foreign_session` in `inspect`. | Resume then needs a fresh thread; no input is written to an unbound launch. |
| Claude's hook shell on Windows does not run the quoted command, so no hook arrives. | M2 P-1 and P-6 on Windows: `session_start_timeout`. | M1 (macOS) is unaffected; the fix is the settings file's command string only. |
| A denied permission fires no `PostToolUse`, so its card stays open until `Stop`, the next prompt, or exit; two parallel calls with equal tool and input can resolve each other's card. | P-16; FC sequences. | The card always closes by the turn's end; a second card key after the first is refused (`card_closed` or `key_not_written`), so no stray key reaches the composer; `ran` for an identical call is the same fact. |
| After `/clear` or in-session `/resume` the thread stops following the CLI (D5). | `raw_unknown` `foreign_session` in `inspect` and the W2 count. | Fail-safe: no readiness from the new session, so sends wait and raise `input_gate_timeout`; the owner starts a new thread. |
| A transcript line longer than 64 MiB (`MAX_BLOB_BYTES`, e.g. inline images) cannot be stored verbatim. | Tailer log; FC with a synthetic long line. | Kept as `raw_unknown` `unparseable` truncated to its first 64 KiB with a marker, the same shape as hook stdin over 4 MiB. **Needs owner sign-off**: it extends the invariant's single exception. |
| A truncate-and-regrow reproduces the same last 4 KiB before the cursor, so D10 misses it. | None at run time. | Claude writes a UUID into every line, so equal windows across different contents are not expected. |
| Lenient matching (D8) acks an input whose text differs from the record only by Claude's transforms. | C1-5 captures. | The text is the same prompt up to known transforms; two candidates ack none (mvp §2.3). |
| Assistant text lags up to 1 s behind the transcript write. | None needed. | No state depends on assistant text; the gate uses hooks only. |
| `raw_unknown` from Claude carries no CLI version (D11). | Triage reads the transcript line's own `version` field. | Fixtures name their version; the corpus loop of arch §2 still works by hand. |

## 6. PR split

1. **C1a: read surface.** D1, D2, `ScreenResponse`, `InspectOutput`, the `ROUTES` row and the matrix inventory entry. C1-11, C1-12. Independent of the others; may run beside C1b.
2. **C1b: hooks.** D3–D9, D11; settings injection first, then the fixture capture, then `ClaudeDecoder`, the per-agent dispatch, `catch_unwind` in `interpret`, coarse cards. C1-1 to C1-7, C1-10's binding half, C1-13, C1-14.
3. **C1c: transcript tailer.** D10, D13; `open_regular`, the tailer, transcript fixtures, the per-line `catch_unwind`, the long-line rule (after sign-off). C1-4's line half, C1-8, C1-9, C1-10's cursor half. After C1b, whose binding supplies the transcript path on real hosts.

The M1 dogfooding rows run after all three merge.
