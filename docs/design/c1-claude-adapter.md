# C1: Claude Code Adapter (lite) and the Rescue Read Surface

Design for mvp §4 row C1.

## 1. Acceptance criteria

FC: FakeCli/FakePty CI (macOS, Windows); M1: real Claude on macOS (Windows at M2).

| ID | Criterion | Check |
|---|---|---|
| C1-1 | When a Claude launch is reserved, its argv shall carry `--settings <state_dir>/claude-settings.json` and `--session-id` or `--resume`, and the user's settings files shall stay byte-equal. | FC; M1 P-1 |
| C1-2 | When Claude starts in a trusted folder, its `SessionStart` shall bind the launch and, with intake facts, record `session_start` readiness. | FC; M1 |
| C1-3 | Every committed fixture shall decode to its recorded fact and events. | FC |
| C1-4 | If a decoder panics on a hook payload or transcript line, then the daemon shall keep the bytes as `raw_unknown` and keep serving. | FC |
| C1-5 | When a corpus prompt is sent, Claude shall record it once in its captured expected form and the input shall end `acked`. | FC; M1 P-6 |
| C1-6 | If an input is not acknowledged in the ack window, then it shall end `unconfirmed` with no second submit. | FC; M1 P-15 |
| C1-7 | When a card is open, it shall resolve `ran` only under D15, else close at the turn boundary; a card stale at open shall refuse its key (`card_closed`). | FC |
| C1-8 | While a launch is bound, the tailer shall store assistant text with its cursor atomically and start a new generation whenever continuity is disproved. | FC |
| C1-9 | If the transcript path is not an absolute regular file, then the tailer shall read nothing and block nothing. | FC |
| C1-10 | When a launch exits, the tailer shall read the rest (`partial_line` for a trailing fragment); when resumed, the same session shall bind and the cursor continue. | FC; M1 P-18 |
| C1-11 | When a thread reader requests its screen, the daemon shall answer `ScreenResponse` within its size bound and append no event. | FC |
| C1-12 | When `chaldea inspect` runs, it shall print the thread's state and screen, or the summary when the snapshot cannot be read. | FC; M1 |
| C1-13 | While Claude stalls before `SessionStart`, the thread shall carry `session_start_timeout` and the daemon shall write no byte. | FC |
| C1-14 | If a bound launch receives another session's `SessionStart`, then it shall be `foreign_session` and the binding shall stay. | FC |

## 2. Decisions

| # | Decision | Rejected |
|---|---|---|
| D1 | **Public** `GET /api/threads/{thread_id}/screen`: `READERS`, thread read check, `ReadStream`; errors 404, 409 `no_live_launch`, 503 `busy`, 500 `internal`. `ScreenResponse`: `thread_id`, `launch_id`, `cols`, `rows`, `cursor`, `modes`, one text line per row, `truncated`; body ≤ 512 KiB. | Owner only (orchestrators could not diagnose children); the cell grid. |
| D2 | **Public** `chaldea inspect <thread> [--json]` (owner token): one snapshot read (`cursor_ahead` fallback, 16 MiB cap), then the screen; `ThreadSummary` fallback; `InspectOutput`, `ErrorBody` errors; exit 0/1/2/4. | A snapshot route; raising the client's global cap. |
| D3 | **Public** Claude argv gains `--settings`; one settings file per daemon start. | One per launch. |
| D4 | Six Claude hook events: `SessionStart`, `UserPromptSubmit`, `Stop`, `PermissionRequest`, `PreToolUse`, `PostToolUse`. | All eight (no kind carries `SessionEnd`, `Notification`). |
| D5 | A launch keeps its first binding across `/clear` and in-session `/resume`. | Following the new session. |
| D6 | Claude keeps binding rule `SessionId`. | `Ancestry` (unverified on real hosts). |
| D7 | A caught decoder panic is `raw_unknown` `schema_mismatch`; only pure parsing runs in the recovered region. | A reason `decoder_panic`. |
| D8 | Prompt forms come from captures; `prompt_matches` accepts only them, and a folded paste of any size whose body equals the sent text. | Every transform subset; a size threshold (it refused real folded prompts). |
| D9 | No VT snapshot when a card opens. | Recording one (`interpret` is data only). |
| D10 | Continuity is a SHA-256 of the read prefix, checked in full at every open and the final read, and each poll by `file_id`, size, and the 64 KiB before the read position and the batch's end (§3). | A tail window alone (misses deeper rewrites); full re-hash per poll (O(n²)). |
| D11 | The CLI version is `claude --version` per launch (own process tree, 5 s deadline, 4 KiB output cap); failure, non-zero exit, or the cap records `null`. | `null` (arch §2 wants the version). |
| D12 | `GET /api/adapters` gains no readiness column. | A field no client acts on. |
| D13 | `open_regular` lives in `pty-runtime`, Windows FFI in `windows::sys`. | `cfg` code in the daemon. |
| D14 | A value over 64 MiB is one `BlobRef` blob of internal parts, served by `/api/blobs`; the producer spools it; parts, event, refs, and cursor commit in one transaction. | Truncation (a second exception to verbatim `raw_unknown`); halting the tailer. |
| D15 | A `PostToolUse` resolves a card `ran` only if received after the turn watermark and the card, with exactly one equal open card; a card received below the watermark closes at open. No other card rule. | Counting the turn's calls. |
| D16 | A prompt Claude 2.1.331 would run as a command (`!` first, ignoring DEL and C1 controls, or `/` first after trimming) is refused before any write at `send` and `spawn --prompt` (`rejected` `lossy_text`, mvp D6); the owner types commands through attach. | Sending them (no `UserPromptSubmit`, so only `unconfirmed`). |

## 3. Risks and limits

| Risk | Detection | Limit, and why it holds |
|---|---|---|
| `SessionStart` before the trust prompt is answered. | M1 P-12. | If seen, Claude rows switch to `OwnerOnly`; `input_gate_timeout` asks for `ready`. |
| No readiness at bind without intake facts (supervisor-send L-12). | `inspect`; `input_gate_timeout`. | The gate stays closed until evidence or `ready`. |
| `--resume` starts a new session id. | M1 P-18. | Nothing reaches an unbound launch; a new thread recovers. |
| Windows hook shell rejects the command. | M2. | Fix the settings string. |
| An emitter path containing `${CLAUDE_PROJECT_DIR}` is rewritten by Claude. | `session_start_timeout`. | Not fixed: no real install path has it. |
| An unfolded paste trimmed to `exit`, `quit`, `:q`, `:q!`, `:wq`, or `:wq!` runs `/exit` (outside D16). | The launch exits. | Not fixed: Claude's TUI does the same; the input ends `unconfirmed`; refusing would track Claude's submit handler. |
| An escaped descendant or late write can change the version read (D11). | `cli_version`. | Not fixed: diagnostic only. |
| After `/clear` the thread follows the old session (D5). | `foreign_session`. | A send ends `unconfirmed`, never resent. |
| A denied card stays open until the boundary; a later equal call marks it `ran`. | M1 P-16. | Only the label is wrong; nothing is written. |
| **L-13 after C1.** Receipt order can close a legitimate card at open (a late `UserPromptSubmit`) or leave a stale card whose key reaches the composer (a late `PermissionRequest`). | The screen. | The owner sees `card_closed` or a contradicting card and answers through attach (T1, T2). |
| A same-inode rewrite of read bytes, truncate-and-regrow included; Claude 2.1.331 does this to remove a discarded message, within the last 64 KiB, or, for an older line, the whole file (only files up to 50 MiB). | Within 64 KiB before the read position or a batch's end: the next window check; deeper: the next open's or final read's prefix check. | May be missed: a deeper rewrite between those checks or during the final read, and any after its last check; history keeps what was read. An incomplete final check is logged as unverified. |
| The modification time moves back. | The next poll's full prefix check. | May be missed if that read fails; the next open or final read checks again. |
| A truncate or replace between the last check and the commit. | The post-commit check or the next poll, when the id changed or the size fell below the offset. | The batch stays in history; a new generation follows. |
| File descriptors run out. | Tailer log. | Ingest stalls and retries; bytes stay in the file. |
| Lenient forms where captures do not cover a transform. | C1-5. | Two candidates ack none. |
| Memory per line. | — | Values over 64 MiB spool (D14); a line holds at most about 192 MiB plus fixed buffers and a one-bit-per-container tokenizer stack. |
| A resume before the old tailer's final read, or a line growing through it. | Tailer log. | The next tailer reads from the cursor. A line still being written may be stored as `partial_line`. |
